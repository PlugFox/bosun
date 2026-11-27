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
use super::{
    Claimed, Shared, Signal, artifact, blocking, git, identity, lookup, now_ms, outbox, workspace,
};
use crate::events;

/// Resolves when `kill` is raised; never when its sender is gone without raising it.
pub(crate) async fn killed(kill: &mut watch::Receiver<Signal>) {
    if kill.wait_for(|k| *k != Signal::Run).await.is_err() {
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
    /// The item's not-ready verdict runs before this one (#204), for its `unready` note.
    pub(crate) unready: u32,
}

/// A spawner that reports the key of the child it starts.
#[derive(Debug)]
pub(super) struct Reporting<'a> {
    pub(super) inner: &'a dyn Spawner,
    pub(super) key: Mutex<Option<oneshot::Sender<ProcessKey>>>,
    pub(super) started: Arc<Mutex<Option<ProcessKey>>>,
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
        // Mutant (`mutant_loop_unready_counts`): the not-ready code counts as a failure.
        Outcome::Unready if cfg!(mutant_loop_unready_counts) => RunResult::Fail,
        Outcome::Unready => RunResult::Unready,
    }
}

/// Runs one verdict run and records it. Returns the machine's event and, for a run that did
/// not pass, its failing tests.
///
/// In worktree mode (#8, E19) the run judges the commit, not the directory: it runs in a
/// throwaway checkout of the head of the item's worktree
/// ([`super::workspace::Workspace::checkout`], set up as the item's worktree is), closed when
/// the run is over however it ended. Below the disk floor the run waits for space: reported once,
/// tried again every poll, until the checkout is made or the run is killed; the item stays where
/// it is. A checkout that cannot be made or set up otherwise is [`Event::SetupFailed`], not a
/// failed run, with its cause stored as the item's `setup` note. A setup killed by a cancel or
/// the kill switch is a killed run. A run that passes
/// records the commit it judged and its tree in the item's `landing` row: what landing accepts
/// as verified ([`super::landing`]). Without worktree mode it runs in the checkout, as in
/// stage 1.
pub(crate) async fn verdict(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<Signal>,
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
    // The item's leases (#8): held already, or taken again, all or nothing, after a park or a
    // deferred landing let them go (E24), or when it was recovered straight to its verdict. A
    // refusal (an unknown resource, another set held) is the configuration's change since the
    // daemon started: retrying cannot help, so it counts no attempt.
    match super::hold_leases(shared, repo, claimed, &run.item, &mut kill).await {
        Ok(()) => {}
        Err(None) => return fail(),
        Err(Some(e)) => {
            shared.trouble(format!("the verdict of {}: its leases: {e}", run.item));
            // Mutant (`mutant_loop_lease_refusal_fails_verdict`): the refusal is a failed run,
            // which counts an attempt.
            if cfg!(mutant_loop_lease_refusal_fails_verdict) {
                return fail();
            }
            return (Event::ConfigChanged, Vec::new());
        }
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
    let checkout = match make_checkout(shared, wt, identity, run, &name, &head, &mut kill).await {
        Ok(checkout) => checkout,
        Err(None) => return fail(),
        Err(Some(event)) => return (event, Vec::new()),
    };
    // Mutant (`mutant_loop_verdict_in_item_worktree`): the verdict runs among what the session
    // left in the item's worktree.
    let cwd = if cfg!(mutant_loop_verdict_in_item_worktree) {
        claimed.workdir.as_path()
    } else {
        checkout.path()
    };
    let ran = run_in(shared, config, claimed, run, kill.clone(), cwd).await;
    // Mutant (`mutant_loop_keep_verdict_checkout`): the checkout is left behind.
    if !cfg!(mutant_loop_keep_verdict_checkout) {
        // A quarantine is reported by the workspace; the run's result stands. A kill of the run
        // ends its teardown too.
        checkout.close(shared, identity, Some(&mut kill)).await;
    }
    if matches!(
        ran.0,
        Event::Verdict {
            result: RunResult::Pass,
            ..
        }
    ) && let Err(e) = verified(shared, claimed, &wt.target, run, &head).await
    {
        // Without the row the commit is not taken as verified: landing asks for another run.
        shared.trouble(format!(
            "the verdict of {}: the verified commit cannot be recorded: {e}",
            run.item
        ));
    }
    ran
}

/// Worktree mode: the verdict checkout of `run`, named `name`, at `head`. Below the disk floor
/// it waits (reported once, tried again every poll). `Err(None)`: `kill` was raised (a killed
/// run); `Err(Some(event))`: it cannot be made or set up, with its cause stored as the item's
/// `setup` note.
async fn make_checkout(
    shared: &Shared,
    wt: &super::ItemWorktree,
    identity: &bosun_config::Identity,
    run: &Run,
    name: &str,
    head: &str,
    kill: &mut watch::Receiver<Signal>,
) -> Result<workspace::Checkout, Option<Event>> {
    let mut waiting = false;
    loop {
        let made = wt
            .workspace
            .checkout(shared, identity, &run.item, name, head, kill)
            .await;
        match made {
            Ok(checkout) => return Ok(checkout),
            // Killed: as a killed run; the kill moved the item.
            Err(Unopened::Killed) => return Err(None),
            // Below the disk floor: no Needs you, the run waits for space.
            // Mutant (`mutant_loop_floor_is_setup_failure`): it is a setup failure.
            Err(Unopened::Error(workspace::Error::Worktree(
                bosun_git::worktree::Error::BelowFloor { free, floor, .. },
            ))) if !cfg!(mutant_loop_floor_is_setup_failure) => {
                if !waiting {
                    waiting = true;
                    shared.trouble(format!(
                        "{}: no verdict checkout below the disk floor: {free} free, {floor} \
                         needed; waiting",
                        run.item
                    ));
                }
                tokio::select! {
                    () = tokio::time::sleep(shared.options.poll) => {}
                    () = killed(kill) => return Err(None),
                }
            }
            Err(Unopened::Setup(failed)) => {
                shared.trouble(format!(
                    "the setup of the verdict checkout of {} failed: {failed}",
                    run.item
                ));
                super::outbox::note_setup(shared, &run.item, &failed.cause(), &failed.tail).await;
                return Err(Some(Event::SetupFailed));
            }
            Err(Unopened::Error(e)) => {
                shared.trouble(format!(
                    "the verdict checkout of {} cannot be made: {e}",
                    run.item
                ));
                let cause = format!("the verdict checkout cannot be made: {e}");
                super::outbox::note_setup(shared, &run.item, &cause, "").await;
                return Err(Some(Event::SetupFailed));
            }
        }
    }
}

/// Worktree mode (#8): records `head` as the commit a passing run of `run`'s attempt verified,
/// with its tree; the only evidence landing accepts that a commit was verified.
async fn verified(
    shared: &Shared,
    claimed: &Claimed,
    target: &str,
    run: &Run,
    head: &str,
) -> Result<(), String> {
    let (dir, at) = (claimed.workdir.clone(), head.to_owned());
    let tree = blocking(move || {
        Git::new(dir)
            .run(
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{at}^{{tree}}"),
                ],
                None,
            )
            .map(|t| t.trim().to_owned())
    })
    .await
    .map_err(|e| e.to_string())?;
    let row = bosun_store::LandingRow {
        item: run.item.clone(),
        attempt: run.attempt,
        target: target.to_owned(),
        head_sha: Some(head.to_owned()),
        verified_tree: Some(tree),
        landed_sha: None,
        delivered_ms: None,
        pr_number: None,
        pr_url: None,
        pr_state: None,
        updated_ms: now_ms(),
    };
    shared
        .deps
        .writer
        .write(vec![Write::Landing(row)])
        .await
        .map_err(|e| e.to_string())
}

/// [`verdict`]'s run itself, in `cwd`.
#[allow(clippy::too_many_lines)]
async fn run_in(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<Signal>,
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
        env: vars.clone(),
        // The checkout in stage 1; a checkout of the commit in worktree mode (#8).
        cwd: cwd.to_owned(),
        timeout: claimed
            .snapshot
            .contract
            .verdict_timeout
            .unwrap_or(repo.verdict.timeout),
        limits: Limits::default(),
        // The repository's code for "a resource the verdict needs is not ready" (#204).
        unready_exit: repo.verdict.unready_exit,
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
    // The device check (#44 C): after the run, before it is recorded, since a pass against a
    // device copy that is not the run's own build does not stand.
    let place = artifact::RunPlace {
        shell: &spec.shell,
        vars: &vars,
        cwd,
    };
    let checked = artifact::check(
        shared, config, repo, claimed, run, &place, result, &mut kill,
    )
    .await;
    let Some(checked) = checked else {
        // Killed during the check: a killed run; the unchecked run does not stand.
        record_end(shared, run, &verdict, RunResult::Unready).await;
        return (fail(RunResult::Fail), Vec::new());
    };
    if let Some(note) = &checked.note {
        // Stored before the event is applied: the Needs-you comment quotes it.
        outbox::note_unready(shared, &run.item, note).await;
    }
    match checked.decision {
        artifact::Decision::Stands | artifact::Decision::Noted(_) => {}
        artifact::Decision::Unready(_) => {
            record_end(shared, run, &verdict, RunResult::Unready).await;
            return (fail(RunResult::Unready), Vec::new());
        }
        artifact::Decision::Stale(_) => {
            record_end(shared, run, &verdict, RunResult::Unready).await;
            return (
                Event::StaleArtifact {
                    attempt: run.attempt,
                    run: run.number,
                },
                Vec::new(),
            );
        }
    }
    record_end(shared, run, &verdict, result).await;
    if result == RunResult::Unready
        && let Some(code) = repo.verdict.unready_exit
    {
        // Stored before the event is applied: the Needs-you comment at the cap quotes it.
        // Daemon facts only, never the run's output.
        let text = format!(
            "run {} of the verdict exited {code} (repo.{}.verdict.unready_exit): not ready, \
             {} of unready_cap {}",
            run.number,
            shared.options.repo,
            run.unready.saturating_add(1),
            repo.verdict.unready_cap,
        );
        outbox::note_unready(shared, &run.item, &text).await;
    }
    // A not-ready run judged nothing: it has no failing tests.
    let failing = if matches!(result, RunResult::Pass | RunResult::Unready) {
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
