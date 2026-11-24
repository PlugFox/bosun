//! A verdict run (#7, #58): always the snapshot's command (#5), in the checkout (in worktree
//! mode, a throwaway checkout of the attempt's head commit, #8), under the agent environment;
//! its child recorded, its output kept, its flaky tests written.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use bosun_api::EventBody;
use bosun_config::Config;
use bosun_core::{Event, ItemId, RunResult};
use bosun_git::Git;
use bosun_proc::{ChildSpec, ProcessKey, Spawned, Spawner};
use bosun_queue::lint::verdict_shell;
use bosun_runner::verdict::{Libtest, Limits, Outcome, VerdictRun, VerdictSpec};
use bosun_store::{VerdictEnd, VerdictStart, Write};
use tokio::sync::{oneshot, watch};

use super::workspace::Unopened;
use super::{Claimed, Shared, blocking, git, identity, lookup, now_ms, workspace};
use crate::events;

/// Resolves when `kill` is raised; never when its sender is gone without raising it.
pub(crate) async fn killed(kill: &mut watch::Receiver<bool>) {
    if kill.wait_for(|k| *k).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Which run this is.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) item: ItemId,
    pub(crate) attempt: u32,
    pub(crate) number: u32,
    /// For a re-run after a non-passing first run: that run's failing tests. A pass now makes
    /// the attempt flaky, and they are its flaky tests (#44 D).
    pub(crate) first_failing: Option<Vec<String>>,
}

/// A spawner that reports the key of the child it starts.
#[derive(Debug)]
struct Reporting<'a> {
    inner: &'a dyn Spawner,
    key: Mutex<Option<oneshot::Sender<ProcessKey>>>,
    started: Arc<Mutex<Option<ProcessKey>>>,
}

impl Spawner for Reporting<'_> {
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned> {
        // Taken in every case: a failed spawn drops the sender, so nobody waits for a key.
        let tx = self
            .key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let spawned = self.inner.spawn(spec)?;
        let key = spawned.tree.key();
        *self.started.lock().unwrap_or_else(PoisonError::into_inner) = Some(key);
        if let Some(tx) = tx {
            let _ = tx.send(key);
        }
        Ok(spawned)
    }
}

/// The verdict command the run executes: the snapshot's (#5).
fn command(shared: &Shared, claimed: &Claimed, item: &ItemId) -> String {
    if cfg!(mutant_loop_live_verdict) {
        // Mutant: the verdict is read from the live card, not from the snapshot.
        let live = item
            .0
            .rsplit_once('#')
            .and_then(|(_, n)| n.parse().ok())
            .and_then(|n| shared.deps.queue.lint_target(bosun_queue::ItemId(n)).ok())
            .and_then(|card| bosun_queue::Contract::parse(&card.body).ok());
        if let Some(contract) = live {
            return contract.verdict;
        }
    }
    claimed.snapshot.contract.verdict.clone()
}

pub(crate) fn run_result(outcome: Outcome) -> RunResult {
    match outcome {
        Outcome::Pass => RunResult::Pass,
        Outcome::Fail => RunResult::Fail,
        Outcome::Timeout => RunResult::Timeout,
    }
}

/// Runs one verdict run and records it. Returns the machine's event and, for a run that did
/// not pass, its failing tests.
///
/// In worktree mode (#8, E19) the run judges the commit, not the directory: it runs in a
/// throwaway checkout of the head of the item's worktree
/// ([`super::workspace::Workspace::checkout`], set up as the item's worktree is), closed when
/// the run is over however it ended. A checkout that
/// cannot be made or set up is [`Event::SetupFailed`], not a failed run. Without worktree mode
/// it runs in the checkout, as in stage 1.
pub(crate) async fn verdict(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<bool>,
) -> (Event, Vec<String>) {
    let Some(wt) = &claimed.worktree else {
        return run_in(shared, config, claimed, run, kill, &claimed.workdir).await;
    };
    let fail = || {
        (
            Event::Verdict {
                attempt: run.attempt,
                run: run.number,
                result: RunResult::Fail,
            },
            Vec::new(),
        )
    };
    let Some((repo, identity)) = lookup(config, &shared.options.repo) else {
        return fail();
    };
    // The item's leases are held through its verdicts (#8): already, unless it was recovered
    // straight to its verdict.
    if let Err(e) = super::hold_leases(shared, repo, claimed, &run.item, &mut kill).await {
        if let Some(e) = e {
            shared.trouble(format!("the verdict of {}: its leases: {e}", run.item));
        }
        return fail();
    }
    let dir = claimed.workdir.clone();
    let head = match blocking(move || git::head(&Git::new(dir))).await {
        Ok(head) => head,
        Err(e) => {
            shared.trouble(format!(
                "the verdict of {}: the head of its worktree: {e}",
                run.item
            ));
            return fail();
        }
    };
    let name = format!("{}-v{}", run.item, run.number);
    let checkout = match wt
        .workspace
        .checkout(shared, identity, &run.item, &name, &head)
        .await
    {
        Ok(checkout) => checkout,
        Err(Unopened::Setup(failed)) => {
            shared.trouble(format!(
                "the setup of the verdict checkout of {} failed: {failed}",
                run.item
            ));
            return (Event::SetupFailed, Vec::new());
        }
        Err(Unopened::Error(e)) => {
            shared.trouble(format!(
                "the verdict checkout of {} cannot be made: {e}",
                run.item
            ));
            return (Event::SetupFailed, Vec::new());
        }
    };
    // Mutant (`mutant_loop_verdict_in_item_worktree`): the verdict runs among what the session
    // left in the item's worktree.
    let cwd = if cfg!(mutant_loop_verdict_in_item_worktree) {
        claimed.workdir.as_path()
    } else {
        checkout.path()
    };
    let ran = run_in(shared, config, claimed, run, kill, cwd).await;
    // Mutant (`mutant_loop_keep_verdict_checkout`): the checkout is left behind.
    if !cfg!(mutant_loop_keep_verdict_checkout) {
        // A quarantine is reported by the workspace; the run's result stands.
        checkout.close(shared, identity).await;
    }
    ran
}

/// [`verdict`]'s run itself, in `cwd`.
#[allow(clippy::too_many_lines)]
async fn run_in(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<bool>,
    cwd: &Path,
) -> (Event, Vec<String>) {
    let fail = |result| Event::Verdict {
        attempt: run.attempt,
        run: run.number,
        result,
    };
    let Some((repo, identity)) = lookup(config, &shared.options.repo) else {
        return (fail(RunResult::Fail), Vec::new());
    };
    let command = command(shared, claimed, &run.item);
    let env = identity::bundle(&repo.identity, identity, &repo.path)
        .and_then(|b| identity::agent_env(&b, &shared.options.work_dir).map_err(|e| e.to_string()));
    let shell = verdict_shell(repo.verdict.shell.as_deref().map(Path::new));
    let (env, shell) = match (env, shell) {
        (Ok(env), Some(shell)) => (env, shell),
        (Err(e), _) => {
            shared.trouble(format!("verdict of {}: {e}", run.item));
            return (fail(RunResult::Fail), Vec::new());
        }
        (_, None) => {
            shared.trouble("the verdict shell cannot be found");
            return (fail(RunResult::Fail), Vec::new());
        }
    };
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect();
    // The shared build directory (E1), and the checkout's files made newer than its last build.
    // Mutant (`mutant_loop_no_build_env`): the directory is not injected into the run.
    let inject = !cfg!(mutant_loop_no_build_env);
    if let Err(e) = workspace::shared_build(repo, cwd, &mut vars, inject).await {
        shared.trouble(format!("the verdict of {}: {e}", run.item));
        return (fail(RunResult::Fail), Vec::new());
    }
    let spec = VerdictSpec {
        shell,
        command: command.clone(),
        env: vars,
        // The checkout in stage 1; a checkout of the commit in worktree mode (#8).
        cwd: cwd.to_owned(),
        timeout: claimed
            .snapshot
            .contract
            .verdict_timeout
            .unwrap_or(repo.verdict.timeout),
        limits: Limits::default(),
    };

    let (key_tx, key_rx) = oneshot::channel();
    let started = Arc::new(Mutex::new(None));
    let reporting = Reporting {
        inner: shared.deps.spawner.as_ref(),
        key: Mutex::new(Some(key_tx)),
        started: started.clone(),
    };
    let record_start = async {
        if let Ok(key) = key_rx.await {
            let write = Write::VerdictStarted(VerdictStart {
                item: run.item.clone(),
                attempt: run.attempt,
                run: run.number,
                command: command.clone(),
                pid: key.pid,
                pid_started_ms: key.start.raw().cast_signed(),
                started_ms: now_ms(),
            });
            if let Err(e) = shared.deps.writer.write(vec![write]).await {
                shared.trouble(format!("the verdict run cannot be recorded: {e}"));
            }
            shared.publish(EventBody::VerdictStarted {
                item: run.item.to_string(),
                attempt: run.attempt,
                run: run.number,
            });
        }
    };
    let finished = tokio::select! {
        (result, ()) = async { tokio::join!(bosun_runner::verdict::run(&reporting, &spec), record_start) } => Some(result),
        () = killed(&mut kill) => None,
    };
    let Some(result) = finished else {
        // Killed: the run's container went with its future; its child is no longer there.
        let key = *started.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = key {
            let _ = shared
                .deps
                .writer
                .write(vec![Write::ChildGone {
                    pid: key.pid,
                    started_ms: key.start.raw().cast_signed(),
                }])
                .await;
        }
        return (fail(RunResult::Fail), Vec::new());
    };
    let verdict = match result {
        Ok(verdict) => verdict,
        Err(e) => {
            shared.trouble(format!("the verdict of {} cannot run: {e}", run.item));
            return (fail(RunResult::Fail), Vec::new());
        }
    };
    let result = run_result(verdict.outcome);
    record_end(shared, run, &verdict, result).await;
    let failing = if result == RunResult::Pass {
        Vec::new()
    } else {
        verdict.failing_tests(&Libtest)
    };
    (fail(result), failing)
}

async fn record_end(shared: &Shared, run: &Run, verdict: &VerdictRun, result: RunResult) {
    let mut writes = vec![Write::VerdictEnded(VerdictEnd {
        item: run.item.clone(),
        attempt: run.attempt,
        run: run.number,
        result,
        exit_code: verdict.exit.code,
        duration_ms: u64::try_from(verdict.duration.as_millis()).unwrap_or(u64::MAX),
        verdict_line: verdict.verdict_line().map(|l| l.text),
        tail: Some(verdict.interleaved.text.clone()),
        ended_ms: now_ms(),
    })];
    if result == RunResult::Pass
        && let Some(names) = &run.first_failing
        && !names.is_empty()
    {
        writes.push(Write::FlakyTests {
            item: run.item.clone(),
            attempt: run.attempt,
            names: names.clone(),
            at_ms: now_ms(),
        });
    }
    if let Err(e) = shared.deps.writer.write(writes).await {
        shared.trouble(format!("the verdict run cannot be recorded: {e}"));
    }
    shared.publish(EventBody::VerdictFinished {
        item: run.item.to_string(),
        attempt: run.attempt,
        run: run.number,
        result: events::run_result_name(result).to_owned(),
        duration_ms: u64::try_from(verdict.duration.as_millis()).unwrap_or(u64::MAX),
    });
}
