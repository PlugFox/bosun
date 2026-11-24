//! The loop of one repository (#7, #62): claim the next Ready card, run it to Done or Needs you,
//! and go on to the next, one item at a time, in the repository's main checkout.
//!
//! # The executor
//!
//! [`bosun_core::step`] decides; this module performs. Every arrow is written to the store
//! ([`Write::Transition`]) before any of its effects is performed, so an effect never happens
//! without its row (#1, invariant 7); every effect carries the key the machine gave it. The
//! effects:
//!
//! | Effect | Performed by |
//! | --- | --- |
//! | `Claim` | the claim task ([`claim`]): claim write and re-read (#5), snapshot, static lint, base capture, the verdict on the base commit (D7), the configuration and policy hashes |
//! | `Spawn` | the session task ([`session`]): fail-closed identity checks, the prompt, the agent environment, the run row, the spawn with the power request held, the stream, the exit, the session's commit range |
//! | `Kill` | the running task's kill signal; for a blocking question, after [`Options::question_grace`] |
//! | `CheckPreconditions` | the verdict preconditions of `bosun-git` (and the guards of #59, see [`guards`]) |
//! | `RunVerdict` | the verdict task ([`verdict`]): always the snapshot's command |
//! | `Land` | `local` landing: nothing to push |
//! | `Project`, `Comment`, `Report` | the store's outbox, written with the transition; sending it is #65 |
//!
//! Long effects run as tasks that report back an [`Event`]; the loop applies it like any
//! other, so a stale report (the item moved on meanwhile) takes no arrow.
//!
//! # What changes during a run
//!
//! The board is polled every [`Options::poll`]: an edited, added or reordered card takes effect
//! at the next poll. A claimed item runs from its snapshot: a body edit applies only at its next
//! claim. The configuration arrives through a watch channel ([`crate::config_watch`]); a change
//! of the repository's configuration hash or its policy hash while an item is in flight sends
//! the item to Needs you (#2).
//!
//! # The checkout
//!
//! Stage 1 works in the repository's main checkout, on the branch HEAD is on at claim. A Done
//! item leaves its commits there. An item that ends anywhere else has its work saved under
//! `refs/bosun/items/<item>` (uncommitted changes as one extra commit) and the checkout is
//! reset to the item's base, so the next item starts from a clean tree on the claim branch.
//!
//! # Not here
//!
//! The agent's calls are answered by [`crate::agent`] (#63) under the credential each session
//! is issued at spawn; a blocking question comes back here as [`Event::Question`].
//!
//! The `bosun-mcp` server and its handshake (#64), the outbox drainer, the sticky report and user
//! commands (#65), parking control (#66), pauses, the kill switch and budgets (#67), and crash
//! recovery (#68): an item found in flight at start keeps its repository waiting for #68.

mod claim;
mod git;
mod identity;
mod session;
#[cfg(test)]
mod tests;
mod verdict;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun_api::EventBody;
use bosun_config::{Config, Identity, Repo};
use bosun_core::{
    Ctx, Effect, EffectKind, Event, Item, ItemId, Reason, Rules, SpawnMode, State, Stepped, step,
};
use bosun_proc::{Power, PowerGuard, Spawner};
use bosun_queue::{Card, Column, Cursor, Queue};
use bosun_store::{NewItem, Reader, Write, Writer};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::events::{self, Events};

pub(crate) use identity::{GhLogins, Logins};

/// How long a session that asked a blocking question may go on before it is killed: time to
/// write down what it has (#7).
pub(crate) const QUESTION_GRACE: Duration = Duration::from_secs(10);

/// The fixed parts of one repository's loop.
pub(crate) struct Options {
    /// The repository's name in the configuration.
    pub(crate) repo: String,
    /// This daemon's stable id on boards (claim markers, #61).
    pub(crate) daemon_id: String,
    /// The agent CLI (`claude`, resolved on `PATH`); `None` refuses every claim.
    pub(crate) agent: Option<PathBuf>,
    /// The identity's directory for session files (system prompt, MCP configuration) and the
    /// empty `gh` configuration of agent sessions.
    pub(crate) work_dir: PathBuf,
    /// How often the board is polled and parked items are woken.
    pub(crate) poll: Duration,
    /// How long a passed identity check before a claim is trusted.
    pub(crate) recheck: Duration,
    /// How long a session that asked a blocking question may go on before it is killed.
    pub(crate) question_grace: Duration,
    /// Test only: a transition this returns `true` for fails to be written.
    #[cfg(test)]
    pub(crate) failpoint: Option<Failpoint>,
}

/// A transition write that fails on purpose (the failpoint test).
#[cfg(test)]
pub(crate) type Failpoint = Arc<dyn Fn(&bosun_core::Transition) -> bool + Send + Sync>;

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("repo", &self.repo)
            .field("daemon_id", &self.daemon_id)
            .field("agent", &self.agent)
            .field("work_dir", &self.work_dir)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

/// What the loop talks to.
pub(crate) struct Deps {
    pub(crate) reader: Reader,
    pub(crate) writer: Writer,
    pub(crate) events: Events,
    pub(crate) queue: Arc<dyn Queue + Send + Sync>,
    pub(crate) spawner: Arc<dyn Spawner>,
    pub(crate) power: Arc<dyn Power>,
    pub(crate) logins: Arc<dyn Logins>,
    /// The agent credentials the API admits; each session is issued one (#63).
    pub(crate) credentials: crate::agent::Credentials,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

/// What the loop and its tasks share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) options: Options,
    pub(crate) deps: Deps,
}

impl Shared {
    fn publish(&self, body: EventBody) {
        self.deps.events.publish(body);
    }

    /// Logs a problem of the repository and publishes it as `board_sync_error`.
    fn trouble(&self, error: impl Into<String>) {
        let error = error.into();
        eprintln!("bosund: repository {}: {error}", self.options.repo);
        self.publish(EventBody::BoardSyncError {
            repo: self.options.repo.clone(),
            error,
        });
    }
}

/// The repository and its identity in `config`.
pub(crate) fn lookup<'a>(config: &'a Config, repo: &str) -> Option<(&'a Repo, &'a Identity)> {
    let repo = config.repos.get(repo)?;
    let identity = config.identities.get(&repo.identity)?;
    Some((repo, identity))
}

/// The item id of card `n` of repository `repo`: unique across repositories.
pub(crate) fn item_id(repo: &str, card: bosun_queue::ItemId) -> ItemId {
    ItemId::new(format!("{repo}#{}", card.0))
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Runs `f` on the blocking pool; a panic in it is a panic here.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// What the claim found, beyond the machine's item.
#[derive(Debug)]
pub(crate) struct Claimed {
    /// The snapshot the item runs from: its verdict, prose and inlined comments.
    pub(crate) snapshot: bosun_queue::Snapshot,
    /// The repository at claim, for the preconditions.
    pub(crate) base: bosun_git::Base,
    /// The repository's configuration hash at claim.
    pub(crate) repo_hash: String,
    /// The repository's policy hash at claim.
    pub(crate) policy_hash: String,
}

/// How a claim task ended.
#[derive(Debug)]
pub(crate) enum ClaimOutcome {
    /// Claimed and snapshotted.
    Confirmed(Box<Claimed>),
    /// The user moved the card meanwhile; the user wins.
    Lost,
    /// The card fails lint (static, or its verdict passes on the base commit).
    Lint(Vec<String>),
    /// The claim could not be carried out; the item goes back to Ready.
    Failed(String),
}

/// What a task reports to the loop.
#[derive(Debug)]
pub(crate) enum Done {
    Claim {
        item: ItemId,
        outcome: ClaimOutcome,
    },
    Event {
        item: ItemId,
        event: Event,
        /// The failing test names of a verdict run that did not pass.
        failing: Vec<String>,
    },
}

/// A running effect: its kill signal and its task.
#[derive(Debug)]
struct Task {
    kill: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

/// The item in flight.
#[derive(Debug)]
struct Current {
    item: Item,
    /// The card as the last poll saw it, for the claim.
    card: Card,
    claimed: Option<Arc<Claimed>>,
    task: Option<Task>,
    /// Held from the claim until the item leaves the loop (in a mutex only to be `Sync`).
    power: Option<std::sync::Mutex<PowerGuard>>,
    /// The failing tests of the current attempt's first verdict run, for `flaky_test`.
    failing: Vec<String>,
}

/// A lint result, cached per card body.
#[derive(Debug, Clone)]
struct Linted {
    body: String,
    claimable: bool,
    findings: Vec<String>,
    blockers: Vec<bosun_queue::ItemId>,
}

/// The loop of one repository.
pub(crate) struct Loop {
    shared: Arc<Shared>,
    config: watch::Receiver<Arc<Config>>,
    current: Option<Current>,
    done_tx: mpsc::UnboundedSender<Done>,
    done_rx: mpsc::UnboundedReceiver<Done>,
    cursor: Option<Cursor>,
    lints: BTreeMap<u64, Linted>,
    /// The last identity check before a claim and when it ran.
    checked: Option<(Instant, Result<(), String>)>,
    /// No claim before this (after a failed claim).
    backoff: Option<Instant>,
    /// An item of this repository was in flight at start; recovery (#68) owns it.
    waiting: bool,
    /// The store refused a transition: the loop stops.
    halted: bool,
}

impl fmt::Debug for Loop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loop")
            .field("repo", &self.shared.options.repo)
            .field("current", &self.current.as_ref().map(|c| &c.item))
            .finish_non_exhaustive()
    }
}

impl Loop {
    pub(crate) fn new(options: Options, deps: Deps, config: watch::Receiver<Arc<Config>>) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            shared: Arc::new(Shared { options, deps }),
            config,
            current: None,
            done_tx,
            done_rx,
            cursor: None,
            lints: BTreeMap::new(),
            checked: None,
            backoff: None,
            waiting: false,
            halted: false,
        }
    }

    /// Runs until the store refuses a write (the loop cannot record what it does any more) or
    /// the configuration channel closes. Aborting the task kills the child in flight: its
    /// container is dropped.
    pub(crate) async fn run(mut self) {
        let mut tick = tokio::time::interval(self.shared.options.poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !self.halted {
            tokio::select! {
                Some(done) = self.done_rx.recv() => self.on_done(done).await,
                changed = self.config.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.on_config().await;
                }
                _ = tick.tick() => self.on_tick().await,
            }
        }
        if let Some(cur) = self.current.as_mut()
            && let Some(task) = cur.task.take()
        {
            let _ = task.kill.send(true);
            let _ = task.handle.await;
        }
    }

    fn config(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }

    fn ctx(&self) -> Ctx {
        let config = self.config();
        Ctx {
            now_ms: now_ms(),
            hold: None,
            rules: Rules {
                park_cap: config.daemon.park_cap,
                ..Rules::default()
            },
        }
    }

    /// Writes one arrow and publishes it. The row is written before any effect (#1).
    async fn record(&self, stepped: &Stepped) -> Result<(), String> {
        #[cfg(test)]
        if let Some(failpoint) = &self.shared.options.failpoint
            && failpoint(&stepped.transition)
        {
            return Err("the failpoint refused the transition".to_owned());
        }
        self.shared
            .deps
            .writer
            .write(vec![Write::Transition {
                stepped: stepped.clone(),
                at_ms: now_ms(),
            }])
            .await
            .map_err(|e| e.to_string())?;
        for body in events::transition(&stepped.item.id, &stepped.transition) {
            self.shared.publish(body);
        }
        Ok(())
    }

    /// Steps an item that is not in flight (a lint bounce, a cycle): its effects are board
    /// writes only, which the store queues with the row.
    async fn bounce(&mut self, item: &Item, event: &Event) {
        let Some(stepped) = step(item, event, &self.ctx()) else {
            return;
        };
        if let Err(e) = self.record(&stepped).await {
            self.halt(&e);
        }
    }

    fn halt(&mut self, error: &str) {
        self.shared.trouble(format!(
            "the store refused a transition, the loop stops: {error}"
        ));
        self.halted = true;
    }

    /// Applies `event` to the item in flight, then every event its effects produce at once.
    async fn apply(&mut self, event: Event) {
        let mut pending = VecDeque::from([event]);
        while let Some(event) = pending.pop_front() {
            let ctx = self.ctx();
            let Some(cur) = self.current.as_ref() else {
                return;
            };
            let Some(stepped) = step(&cur.item, &event, &ctx) else {
                continue;
            };
            if cfg!(mutant_loop_effect_before_transition) {
                // Mutant: the effects are performed first and the row written after.
                let follow = self.perform(&stepped.transition.effects).await;
                if let Err(e) = self.record(&stepped).await {
                    self.halt(&e);
                    return;
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.item = stepped.item.clone();
                }
                pending.extend(follow);
            } else {
                if let Err(e) = self.record(&stepped).await {
                    self.halt(&e);
                    return;
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.item = stepped.item.clone();
                }
                pending.extend(self.perform(&stepped.transition.effects).await);
            }
            let state = stepped.item.state;
            if !state.is_active() {
                self.finish().await;
                return;
            }
        }
    }

    /// Performs `effects` in order; returns the events of those that complete at once.
    async fn perform(&mut self, effects: &[Effect]) -> Vec<Event> {
        let mut follow = Vec::new();
        for effect in effects {
            match effect.kind {
                EffectKind::Claim => self.start_claim(effect),
                EffectKind::Spawn {
                    attempt,
                    spawn,
                    mode,
                } => {
                    if let Some(event) = self.start_session(attempt, spawn, mode).await {
                        follow.push(event);
                    }
                }
                EffectKind::Kill => {
                    // A blocking question's session gets its grace when the item leaves the
                    // loop ([`Loop::finish`]).
                    let question = self
                        .current
                        .as_ref()
                        .is_some_and(|c| c.item.state == State::NeedsYou(Reason::Question));
                    if !question
                        && let Some(task) = self.current.as_ref().and_then(|c| c.task.as_ref())
                    {
                        let _ = task.kill.send(true);
                    }
                }
                EffectKind::CheckPreconditions { attempt } => {
                    follow.push(self.preconditions(attempt).await);
                }
                EffectKind::RunVerdict { attempt, run } => self.start_verdict(attempt, run).await,
                // `local` landing (#7): nothing to push; the trailers are injected at commit
                // time (#59) and the report is the outbox's (#65).
                EffectKind::Land { .. } => follow.push(Event::Landed),
                // Queued in the outbox with the transition; sent by #65.
                EffectKind::Project(_) | EffectKind::Comment(_) | EffectKind::Report => {}
            }
        }
        follow
    }

    /// Replaces the task in flight (the previous one has ended or is being killed).
    async fn set_task(&mut self, kill: watch::Sender<bool>, handle: JoinHandle<()>) {
        let Some(cur) = self.current.as_mut() else {
            handle.abort();
            return;
        };
        if let Some(old) = cur.task.replace(Task { kill, handle }) {
            let _ = old.kill.send(true);
            let _ = old.handle.await;
        }
    }

    fn start_claim(&mut self, effect: &Effect) {
        let config = self.config();
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        if cur.power.is_none() {
            match self
                .shared
                .deps
                .power
                .acquire("bosun is working on an item")
            {
                Ok(guard) => cur.power = Some(std::sync::Mutex::new(guard)),
                Err(e) => self
                    .shared
                    .trouble(format!("the power request failed: {e}")),
            }
        }
        let (kill, kill_rx) = watch::channel(false);
        let shared = self.shared.clone();
        let card = cur.card.clone();
        let item = cur.item.id.clone();
        let key = effect.key.to_string();
        let done = self.done_tx.clone();
        let handle = tokio::spawn(async move {
            let outcome = claim::claim(&shared, &config, card, &item, &key, kill_rx).await;
            let _ = done.send(Done::Claim { item, outcome });
        });
        if let Some(old) = cur.task.replace(Task { kill, handle }) {
            let _ = old.kill.send(true);
            old.handle.abort();
        }
    }

    /// The hashes at claim differ from the configuration's now.
    fn hashes_changed(&self, claimed: &Claimed) -> bool {
        if cfg!(mutant_loop_ignore_hash_change) {
            // Mutant: a change of the configuration or the policy goes unnoticed.
            return false;
        }
        let config = self.config();
        match claim::hashes(&config, &self.shared.options.repo) {
            Some((repo, policy)) => repo != claimed.repo_hash || policy != claimed.policy_hash,
            None => true,
        }
    }

    async fn start_session(&mut self, attempt: u32, spawn: u32, mode: SpawnMode) -> Option<Event> {
        let cur = self.current.as_ref()?;
        let claimed = cur.claimed.clone()?;
        if self.hashes_changed(&claimed) {
            return Some(Event::ConfigChanged);
        }
        let (kill, kill_rx) = watch::channel(false);
        let shared = self.shared.clone();
        let config = self.config();
        let item = cur.item.id.clone();
        let done = self.done_tx.clone();
        // A blocking question the API recorded under this session's credential (#63).
        let on_blocking: Box<dyn Fn() + Send + Sync> = {
            let (done, item) = (done.clone(), item.clone());
            Box::new(move || {
                let _ = done.send(Done::Event {
                    item: item.clone(),
                    event: Event::Question { attempt },
                    failing: Vec::new(),
                });
            })
        };
        let handle = tokio::spawn(async move {
            let run = session::Run {
                item: item.clone(),
                attempt,
                spawn,
                mode,
            };
            let event =
                session::session(&shared, &config, &claimed, &run, kill_rx, on_blocking).await;
            let _ = done.send(Done::Event {
                item,
                event,
                failing: Vec::new(),
            });
        });
        self.set_task(kill, handle).await;
        None
    }

    async fn start_verdict(&mut self, attempt: u32, run: u32) {
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        let Some(claimed) = cur.claimed.clone() else {
            return;
        };
        // A re-run after a non-passing first run: a pass makes the attempt flaky.
        let rerun = cur
            .item
            .attempt
            .as_ref()
            .is_some_and(|a| a.first_run.is_some());
        let first_failing = rerun.then(|| cur.failing.clone());
        let (kill, kill_rx) = watch::channel(false);
        let shared = self.shared.clone();
        let config = self.config();
        let item = cur.item.id.clone();
        let done = self.done_tx.clone();
        let handle = tokio::spawn(async move {
            let spec = verdict::Run {
                item: item.clone(),
                attempt,
                number: run,
                first_failing,
            };
            let (event, failing) =
                verdict::verdict(&shared, &config, &claimed, &spec, kill_rx).await;
            let _ = done.send(Done::Event {
                item,
                event,
                failing,
            });
        });
        self.set_task(kill, handle).await;
    }

    /// The verdict's preconditions and guards, and a last look at the hashes.
    async fn preconditions(&mut self, attempt: u32) -> Event {
        let Some(cur) = self.current.as_ref() else {
            return Event::Preconditions {
                attempt,
                passed: false,
            };
        };
        let Some(claimed) = cur.claimed.clone() else {
            return Event::Preconditions {
                attempt,
                passed: false,
            };
        };
        if self.hashes_changed(&claimed) {
            return Event::ConfigChanged;
        }
        let config = self.config();
        let Some((repo, _)) = lookup(&config, &self.shared.options.repo) else {
            return Event::ConfigChanged;
        };
        let path = repo.path.clone();
        let item = cur.item.id.clone();
        let runs = self
            .shared
            .deps
            .reader
            .runs(&item)
            .await
            .unwrap_or_default();
        let checked =
            blocking(move || git::preconditions(&path, &claimed.base, &item, &runs)).await;
        match checked {
            Ok(reasons) if reasons.is_empty() => {
                if let Some(guard) = guards() {
                    return Event::Refused { attempt, guard };
                }
                Event::Preconditions {
                    attempt,
                    passed: true,
                }
            }
            Ok(reasons) => {
                let text: Vec<String> = reasons.iter().map(ToString::to_string).collect();
                eprintln!(
                    "bosund: repository {}: attempt {attempt} fails its preconditions: {}",
                    self.shared.options.repo,
                    text.join("; ")
                );
                Event::Preconditions {
                    attempt,
                    passed: false,
                }
            }
            Err(e) => {
                self.shared
                    .trouble(format!("the preconditions could not be checked: {e}"));
                Event::Preconditions {
                    attempt,
                    passed: false,
                }
            }
        }
    }

    /// The item left the loop: its task ends, the checkout goes back to the claim branch.
    async fn finish(&mut self) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        if let Some(mut task) = cur.task.take() {
            // A session that asked a blocking question may end on its own within its grace.
            let grace = if cur.item.state == State::NeedsYou(Reason::Question) {
                self.shared.options.question_grace
            } else {
                Duration::ZERO
            };
            if tokio::time::timeout(grace, &mut task.handle).await.is_err() {
                let _ = task.kill.send(true);
                let _ = task.handle.await;
            }
        }
        let done = matches!(cur.item.state, State::Done(_));
        if !done && let Some(claimed) = &cur.claimed {
            let config = self.config();
            if let Some((repo, _)) = lookup(&config, &self.shared.options.repo) {
                let path = repo.path.clone();
                let base = claimed.base.clone();
                let item = cur.item.id.clone();
                if let Err(e) = blocking(move || git::set_aside(&path, &base, &item)).await {
                    self.shared
                        .trouble(format!("the checkout could not be restored: {e}"));
                }
            }
        }
        drop(cur.power.take());
    }

    async fn on_done(&mut self, done: Done) {
        let item = match &done {
            Done::Claim { item, .. } | Done::Event { item, .. } => item.clone(),
        };
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        if cur.item.id != item {
            return;
        }
        match done {
            Done::Claim { outcome, .. } => {
                let event = match outcome {
                    ClaimOutcome::Confirmed(claimed) => {
                        cur.claimed = Some(Arc::from(claimed));
                        Event::ClaimConfirmed
                    }
                    ClaimOutcome::Lost => Event::ClaimLost,
                    ClaimOutcome::Lint(findings) => {
                        self.shared.publish(EventBody::LintFindings {
                            repo: self.shared.options.repo.clone(),
                            card: item.to_string(),
                            findings,
                        });
                        Event::LintFailed
                    }
                    ClaimOutcome::Failed(e) => {
                        self.shared.trouble(format!("claiming {item} failed: {e}"));
                        self.backoff = Some(Instant::now() + Duration::from_mins(1));
                        Event::ClaimLost
                    }
                };
                self.apply(event).await;
            }
            Done::Event { event, failing, .. } => {
                if matches!(event, Event::Verdict { .. })
                    && cur
                        .item
                        .attempt
                        .as_ref()
                        .is_some_and(|a| a.first_run.is_none())
                {
                    cur.failing = failing;
                }
                self.apply(event).await;
            }
        }
    }

    async fn on_config(&mut self) {
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        let Some(claimed) = cur.claimed.clone() else {
            return;
        };
        if self.hashes_changed(&claimed) {
            self.apply(Event::ConfigChanged).await;
        }
    }

    async fn on_tick(&mut self) {
        if let Some(cur) = self.current.as_ref() {
            if matches!(cur.item.state, State::Parked(_)) {
                self.apply(Event::Tick).await;
            }
            return;
        }
        if self.waiting || self.backoff.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.schedule().await;
    }

    /// Polls the board and claims the next card, bouncing those that fail lint.
    #[allow(clippy::too_many_lines)]
    async fn schedule(&mut self) {
        let config = self.config();
        let repo_name = self.shared.options.repo.clone();
        let Some((repo, identity)) = lookup(&config, &repo_name) else {
            return;
        };
        let queue = self.shared.deps.queue.clone();
        let cursor = self.cursor.clone();
        let changes = match blocking(move || queue.poll(cursor.as_ref())).await {
            Ok(changes) => changes,
            Err(e) => {
                self.shared
                    .trouble(format!("the board cannot be read: {e}"));
                return;
            }
        };
        if !changes.is_empty() {
            self.shared.publish(EventBody::QueueChanged {
                repo: repo_name.clone(),
                hash: claim::text_hash(changes.cursor.as_str()),
            });
        }
        self.cursor = Some(changes.cursor.clone());

        let stored = match self.shared.deps.reader.items(false).await {
            Ok(items) => items,
            Err(e) => {
                self.shared
                    .trouble(format!("the store cannot be read: {e}"));
                return;
            }
        };
        let stored: BTreeMap<ItemId, bosun_store::StoredItem> = stored
            .into_iter()
            .filter(|s| s.repo == repo.key)
            .map(|s| (s.item.id.clone(), s))
            .collect();
        if stored.values().any(|s| s.item.state.is_active()) {
            self.waiting = true;
            self.shared.trouble(
                "an item was in flight when the daemon stopped; the repository waits for crash \
                 recovery (#68)",
            );
            return;
        }

        let on_board: BTreeSet<u64> = changes.cards.iter().map(|c| c.id.0).collect();
        let mut cards = Vec::new();
        let mut bounced = Vec::new();
        for card in &changes.cards {
            let id = item_id(&repo_name, card.id);
            let free = stored.get(&id).is_none_or(|s| s.item.state == State::Ready);
            let ready = card.column == Column::Ready && free;
            let linted = if ready {
                Some(self.lint(card, repo, &on_board).await)
            } else {
                None
            };
            if let Some(linted) = &linted
                && !linted.claimable
            {
                bounced.push((card.clone(), linted.findings.clone()));
                continue;
            }
            let blockers = linted.map_or_else(|| card.blocked_by.clone(), |l| l.blockers);
            cards.push(bosun_core::Card {
                item: id,
                repo: repo_name.clone(),
                ready,
                blocked_by: blockers
                    .into_iter()
                    .map(|b| item_id(&repo_name, b))
                    .collect(),
            });
        }
        for (card, findings) in bounced {
            let id = item_id(&repo_name, card.id);
            if !self.register(repo, &card, &id).await {
                return;
            }
            self.shared.publish(EventBody::LintFindings {
                repo: repo_name.clone(),
                card: id.to_string(),
                findings,
            });
            let item = stored
                .get(&id)
                .map_or_else(|| Item::new(id.clone()), |s| s.item.clone());
            self.bounce(&item, &Event::LintFailed).await;
        }

        let satisfied: BTreeSet<ItemId> = stored
            .values()
            .filter(|s| matches!(s.item.state, State::Done(_)))
            .map(|s| s.item.id.clone())
            .collect();
        let plan = bosun_core::plan(&cards, &satisfied, &bosun_core::Gates::default(), now_ms());
        for id in &plan.cycles {
            let Some(card) = changes
                .cards
                .iter()
                .find(|c| item_id(&repo_name, c.id) == *id)
            else {
                continue;
            };
            if !self.register(repo, card, id).await {
                return;
            }
            let item = stored
                .get(id)
                .map_or_else(|| Item::new(id.clone()), |s| s.item.clone());
            self.bounce(&item, &Event::Cycle).await;
        }
        let Some(next) = plan.claim.first() else {
            return;
        };
        let Some(card) = changes
            .cards
            .iter()
            .find(|c| item_id(&repo_name, c.id) == *next)
        else {
            return;
        };
        if let Err(e) = self.ready_to_claim(repo, identity).await {
            self.shared.trouble(e);
            return;
        }
        if !self.register(repo, card, next).await {
            return;
        }
        let item = stored
            .get(next)
            .map_or_else(|| Item::new(next.clone()), |s| s.item.clone());
        self.current = Some(Current {
            item,
            card: card.clone(),
            claimed: None,
            task: None,
            power: None,
            failing: Vec::new(),
        });
        self.apply(Event::Claim).await;
    }

    /// Records the item of `card`; `false` when the store refused (the loop halts).
    async fn register(&mut self, repo: &Repo, card: &Card, id: &ItemId) -> bool {
        let write = Write::Item(NewItem {
            item: id.clone(),
            repo: repo.key.clone(),
            card_key: Some(card.id.to_string()),
            at_ms: now_ms(),
        });
        if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
            self.halt(&e.to_string());
            return false;
        }
        true
    }

    /// The static lint of a Ready card, cached until its body changes.
    async fn lint(&mut self, card: &Card, repo: &Repo, on_board: &BTreeSet<u64>) -> Linted {
        if let Some(cached) = self.lints.get(&card.id.0)
            && cached.body == card.body
        {
            return cached.clone();
        }
        let card_owned = card.clone();
        let repo = repo.clone();
        let on_board = on_board.clone();
        let lint = blocking(move || claim::static_lint(&card_owned, &repo, &on_board)).await;
        let linted = Linted {
            body: card.body.clone(),
            claimable: lint.claimable(),
            findings: lint.findings.iter().map(ToString::to_string).collect(),
            blockers: lint
                .contract
                .as_ref()
                .map_or_else(|| card.blocked_by.clone(), |c| card.blockers(c)),
        };
        self.lints.insert(card.id.0, linted.clone());
        linted
    }

    /// Before a claim: the checkout is clean, and the identity checks pass (#2, rule 2). A
    /// refusal holds the repository, not the card.
    async fn ready_to_claim(&mut self, repo: &Repo, identity: &Identity) -> Result<(), String> {
        if self.shared.options.agent.is_none() {
            return Err("the agent CLI `claude` was not found on PATH".to_owned());
        }
        let path = repo.path.clone();
        let dirty = blocking(move || bosun_git::clean_tree(&bosun_git::Git::new(path))).await;
        match dirty {
            Ok(reasons) if reasons.is_empty() => {}
            Ok(_) => {
                return Err("the checkout has uncommitted changes; no item is claimed".to_owned());
            }
            Err(e) => return Err(format!("the checkout cannot be read: {e}")),
        }
        if let Some((at, result)) = &self.checked
            && at.elapsed() < self.shared.options.recheck
        {
            return result.clone();
        }
        let shared = self.shared.clone();
        let repo = repo.clone();
        let name = repo.identity.clone();
        let identity = identity.clone();
        let result = blocking(move || {
            let bundle = identity::bundle(&name, &identity, &repo.path)?;
            identity::check(&bundle, &repo.path, shared.deps.logins.as_ref())
        })
        .await;
        self.checked = Some((Instant::now(), result.clone()));
        result
    }
}

/// The guards of #59 that send an item to Needs you before its verdict (ignored files, git's
/// configuration, protected paths). `bosun-git`'s snapshots for them are #59 (PR #94); until it
/// lands, no guard refuses. The seam: capture at claim into [`Claimed`], compare here.
fn guards() -> Option<bosun_core::Guard> {
    None
}
