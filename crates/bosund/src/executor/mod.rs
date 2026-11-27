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
//! | `Spawn` | the session task ([`session`]): fail-closed identity checks, the prompt, the agent environment, the run row, the spawn with the power request held, the stream, the exit, the session's commit range. In worktree mode the agent runs under a copy of the session shim (`bosun-shim`, [`find_shim`], [`session::install_shim`]) capped at the item's remaining wall-clock budget; the shim's `process` row is written before the spawn and holds its pid once it runs and how far its output has been read; the session outlives the loop that started it, and a graceful stop ([`Signal::Detach`], E22) leaves it running for the next daemon to re-attach. The run row records the head the session started from |
//! | (after a session) | the process sweep ([`sweep`], E12): with `repo.<name>.sweep`, the listed processes that started during the attempt are killed, reported and noted on the item before the session's event is applied; nothing for a session a graceful stop left running |
//! | `Kill` | the running task's kill signal ([`Signal::Kill`]); for a blocking question, after [`Options::question_grace`] |
//! | `CheckPreconditions` | the guards of #59 ([`guards`]: protected paths, new ignored files, git's own configuration), then the verdict preconditions of `bosun-git` |
//! | `RunVerdict` | the verdict task ([`verdict`]): always the snapshot's command |
//! | `Land` | without `repo.<name>.worktree`: nothing to push; it never waits for the board. In worktree mode, `local` and `merge`: the landing task ([`landing`]): rebase onto the target's head, the verdict again on a rebased head, the commit to land, the fetch into the user's repository and its fast-forward (deferred while the checkout of the target is dirty or busy); `pr`: the same up to the verified head, then the push of the item's branch and its pull request ([`forge`]) |
//! | `Project`, `Comment`, `Report` | the store's outbox, written with the transition; sent by the drainer ([`outbox`]) |
//!
//! Long effects run as tasks that report back an [`Event`]; the loop applies it like any
//! other, so a stale report (the item moved on meanwhile) takes no arrow.
//!
//! # What changes during a run
//!
//! The board is polled every [`Options::poll`]: an edited, added or reordered card takes effect
//! at the next poll. A user's action on a daemon-owned card is a command ([`commands`]): cancel,
//! start over, retry, the `restart` label, a manual Done; anything else is reverted. A claimed item runs from its snapshot: a body edit applies only at its next
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
//! With `repo.<name>.worktree` (#8, E19), and only then, the item runs in its own worktree off
//! the daemon's clone instead ([`workspace`], opened at claim): the session, the guards and the
//! preconditions run there ([`Claimed::workdir`]), while the identity checks still look at the
//! user's repository. The clean tree is not a precondition there: what the session left
//! uncommitted is noted (`uncommitted`), and every verdict run judges the commit, in a
//! throwaway checkout of the worktree's head, set up and closed again ([`verdict`]). The
//! verdict on the base commit at claim still runs in the item's fresh worktree. The user's
//! checkout is captured at claim and compared
//! before every verdict ([`checkout`], E4 as amended by E21). A passing verdict run records the
//! commit it judged; landing ([`landing`], E2, E14) rebases the item onto the target's head,
//! has the rebased head verified again, and fast-forwards the target in the user's repository
//! (its clean checkout with it), so a Done item's work is on the target. The target the daemon
//! moves in its clone, and in the user's repository, is taken into the claim's base and the
//! capture of the user's checkout, and the item's own commits start at the merge base of its
//! head and the target ([`git::item_base`]; in `pr` mode the forge's target, E28, [`base_ref`]),
//! so neither the next attempt nor a restarted daemon
//! takes landing for tampering, nor a person's commits for the agent's. Every item ends with
//! its worktree closed; one that is not Done keeps its work in the clone under
//! `refs/bosun/items/<item>`.
//!
//! In the `pr` landing mode an item is Done when its pull request is open; the loop goes on
//! asking the forge about it every [`Options::pr_poll`] ([`pulls`]): merged is final and
//! releases its dependants (`blocked-by`), which wait until then (E6); closed unmerged is Needs
//! you with one comment.
//!
//! # Leases (#8)
//!
//! In worktree mode an item leases what its snapshot `needs`, plus the shared build's resource
//! (`worktree.shared_build`, E1), from the daemon's one [`crate::leases::Leases`]: inside the
//! claim task, before its worktree is opened and set up, so the setup and the verdict on the
//! base commit run under them, the loop stays responsive while it waits, and a kill withdraws
//! the request ([`hold_names`]). A claim that does not confirm releases them after its
//! worktree is closed. A confirmed item holds them through its sessions, verdicts, landing and
//! teardown, until it leaves the loop ([`Loop::finish`]), except while it waits (E24): it lets
//! go of them when it is Parked ([`Loop::apply`]), when its landing's fast-forward is
//! deferred and while a `pr` landing waits for an unreachable forge ([`landing`]). Each session and each verdict asks again ([`hold_leases`]): granted
//! at once to a holder of exactly that set, else filed all or nothing and awaited, its wait
//! written as new `lease_wait` rows. A fast-forward after a deferral takes none. A refusal (an
//! unknown resource, another set held: the configuration changed since the daemon started) is
//! [`Event::ConfigChanged`], not a failed attempt. A loop the store halts frees its item's
//! leases; a graceful stop keeps them journalled for the next daemon (E22). Lint admits the
//! configured resources only in worktree mode ([`lint_resources`]). Under a shared build every process started in a worktree gets the
//! directory, and the worktree's tracked files are touched first ([`workspace::enter`]).
//!
//! # Guards and trailers (#59, #99)
//!
//! Git's own configuration is recorded at claim, the ignored files before each attempt's first
//! session; both are stored on the attempt's row. Before the verdict, a protected path the
//! commits touched (unless the snapshot's `allow-protected` lists it), a new ignored file
//! outside the build directories or a change of git's configuration is Needs you with the
//! guard, and the verdict does not run. Every session's git gets the trailer hooks in its own
//! directory (`Item:`, `Run:`, `Session:` with the attempt's values); a commit without them is
//! a `trailer_missing` line of the report, one with other values fails the preconditions.
//!
//! # The board
//!
//! The queue a repository names is connected at the loop's first poll ([`board`]). A board
//! error that pauses the repository (`Setup`, `Scope`, another daemon's claim marker) holds
//! every claim for [`BOARD_RETRY`]; a rate limit holds them for its wait; a card that left the
//! board at claim cancels its item. The board is polled while an item is in flight too: a card
//! gone from it (deleted, transferred, converted) for two polls cancels the item and kills its
//! session ([`commands`], #65, #106). A Projects v2 board is read at the poller's pace ([`board::Pace`]):
//! 30 s while active, backing off to five minutes while idle.
//!
//! # Parking
//!
//! See [`park`]: a limit parks the item until its reset time on the loop's [`Clock`] and the
//! next tick past it resumes the session by id; a resume that fails resets the tree to the last
//! commit and starts a fresh session; an expired authentication is Needs you at once. None of
//! these counts, and the uncounted ones are capped (`daemon.park_cap`). A sleep (a gap between
//! ticks) revalidates the session's child; a dead one is a crash of the attempt.
//!
//! # Holds and budgets
//!
//! See [`hold`]: every step runs under the hold of [`crate::control::Control`] on the
//! repository (a pause, the kill switch, the daily cap); the kill switch kills the child in
//! flight as soon as it is thrown; the item's eqv and wall-clock caps are checked every few
//! ticks, and so are the automatic pause and the daily cap.
//!
//! # Not here
//!
//! The agent's calls are answered by [`crate::agent`] (#63) under the credential each session
//! is issued at spawn; a blocking question comes back here as [`Event::Question`].
//!
//! The `bosun-mcp` server and its handshake (#64).
//!
//! # Crash recovery
//!
//! See [`recover`]: at start, before the first claim and before the outbox drainer starts, the
//! items a previous daemon left in flight are recovered from the store: the user's commands on
//! the board first, then the surviving children reaped, then the lost work handed back to the
//! machine (resumed, verified again or landed again, uncounted).

mod artifact;
pub(crate) mod board;
mod breaker;
mod census;
mod checkout;
mod claim;
mod commands;
mod forge;
mod git;
mod health;
mod hold;
mod identity;
mod landing;
mod outbox;
mod park;
mod pulls;
mod recover;
mod said;
mod session;
mod sweep;
#[cfg(test)]
mod tests;
mod verdict;
mod workspace;

pub(crate) use workspace::Disk;

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
use bosun_store::{NewItem, Reader, StoredItem, Write, Writer};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::events::{self, Events};

pub(crate) use forge::{ForgeError, GhForge, output_within};
pub(crate) use identity::{GhLogins, Logins};
pub(crate) use park::{Alive, Clock, SystemClock};
pub(crate) use recover::next_boot;
pub(crate) use sweep::Live;

/// How long a session that asked a blocking question may go on before it is killed: time to
/// write down what it has (#7).
pub(crate) const QUESTION_GRACE: Duration = Duration::from_secs(10);

/// How often the daemon asks the forge about the open pull requests of Done items (#8).
pub(crate) const PR_POLL: Duration = Duration::from_mins(5);

/// How long after spawn a session's `bosun-mcp` has to call `agent.hello` (#64). The agent
/// CLI starts its MCP servers before its first turn; this leaves room for a slow start of the
/// CLI itself.
pub(crate) const HANDSHAKE: Duration = Duration::from_mins(1);

/// How long a board error that pauses the repository holds its claims before the board is
/// asked again (#99): the user fixes the board, the token or the other daemon meanwhile.
pub(crate) const BOARD_RETRY: Duration = Duration::from_mins(5);

/// The MCP server of agent sessions: `bosun-mcp` next to this executable (as `cargo install`
/// and a release archive place it), else on `PATH`, else the bare name, which the agent CLI
/// looks up itself; if it is not there, the handshake fails and says so.
pub(crate) fn find_mcp() -> PathBuf {
    let name = format!("bosun-mcp{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|p| p.is_file());
    sibling
        .or_else(|| {
            bosun_config::tools::find_program("bosun-mcp", std::env::var_os("PATH").as_deref())
        })
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The session shim (`bosun-shim`, #8): next to this executable, else on `PATH`, else the bare
/// name (which the copy before a session then fails to find, and says so). Sessions run a copy
/// of it, never this file ([`session::install_shim`]): a running executable cannot be replaced
/// on Windows, and the shim outlives the daemon (E3).
pub(crate) fn find_shim() -> PathBuf {
    let name = format!("bosun-shim{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|p| p.is_file());
    sibling
        .or_else(|| {
            bosun_config::tools::find_program("bosun-shim", std::env::var_os("PATH").as_deref())
        })
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Starts an agent session of worktree mode through the session shim (#8): the seam through
/// which the scenario tests observe those spawns, as they wrap [`Deps::spawner`] for the rest.
pub(crate) trait ShimSessions: Send + Sync {
    /// Starts `spec` through `shim` with `dir` as the shim's session directory.
    ///
    /// # Errors
    ///
    /// As [`bosun_proc::shim::ShimSpawner::spawn_in`].
    fn spawn_in(
        &self,
        shim: &bosun_proc::shim::ShimSpawner,
        spec: &bosun_proc::ChildSpec,
        dir: &std::path::Path,
    ) -> std::io::Result<bosun_proc::Spawned>;
}

/// The daemon's [`ShimSessions`]: the shim spawner itself.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ThroughShim;

impl ShimSessions for ThroughShim {
    fn spawn_in(
        &self,
        shim: &bosun_proc::shim::ShimSpawner,
        spec: &bosun_proc::ChildSpec,
        dir: &std::path::Path,
    ) -> std::io::Result<bosun_proc::Spawned> {
        shim.spawn_in(spec, dir)
    }
}

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
    /// How often the loop ticks: the board is polled and parked items are woken.
    pub(crate) poll: Duration,
    /// Whether board reads follow the poller's pace (#60, #106: every tick while an item is in
    /// flight, backing off while idle) rather than every tick: a Projects v2 board.
    pub(crate) idle_backoff: bool,
    /// How long a passed identity check before a claim is trusted.
    pub(crate) recheck: Duration,
    /// How long a session that asked a blocking question may go on before it is killed.
    pub(crate) question_grace: Duration,
    /// How often the open pull requests of items Done in the `pr` landing mode are asked
    /// about ([`pulls`]; [`PR_POLL`] in the daemon).
    pub(crate) pr_poll: Duration,
    /// The MCP server every session's configuration starts ([`find_mcp`]).
    pub(crate) mcp: PathBuf,
    /// The session shim as found ([`find_shim`]); worktree-mode sessions run a copy of it.
    pub(crate) shim: PathBuf,
    /// The daemon's own directory (the operator token's and the lease journal's): the copies
    /// of the shim go under its `shim` directory.
    pub(crate) daemon_dir: PathBuf,
    /// The daemon's endpoint, which `bosun-mcp` connects to.
    pub(crate) endpoint: bosun_client::Endpoint,
    /// How long after spawn `bosun-mcp` has to say hello ([`HANDSHAKE`]).
    pub(crate) handshake: Duration,
    /// This start's boot number ([`recover::next_boot`], one per store); `None`: the loop
    /// records one itself before it recovers.
    pub(crate) boot: Option<u32>,
    /// Test only: a transition this returns an error for fails to be written, refused with it.
    #[cfg(test)]
    pub(crate) failpoint: Option<Failpoint>,
}

/// A transition write that fails on purpose (the failpoint test): `None` writes it, `Some(e)`
/// refuses it with `e`.
#[cfg(test)]
pub(crate) type Failpoint =
    Arc<dyn Fn(&bosun_core::Transition) -> Option<bosun_store::Error> + Send + Sync>;

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
    /// Whether the repository is public: the sticky report's verdict tails (#5, #111).
    pub(crate) visibility: Arc<board::Visibility>,
    /// Every child the loop starts, except the agent sessions of worktree mode.
    pub(crate) spawner: Arc<dyn Spawner>,
    /// The agent sessions of worktree mode, through the session shim (#8).
    pub(crate) sessions: Arc<dyn ShimSessions>,
    pub(crate) power: Arc<dyn Power>,
    pub(crate) logins: Arc<dyn Logins>,
    /// The agent credentials the API admits; each session is issued one (#63).
    pub(crate) credentials: crate::agent::Credentials,
    /// The clocks of park timers and of the sleep detector.
    pub(crate) clock: Arc<dyn Clock>,
    /// Whether a recorded child still runs, checked after a sleep.
    pub(crate) alive: Alive,
    /// Pauses, the kill switch and the daily cap (#67).
    pub(crate) control: crate::control::Control,
    /// The daemon's lease manager, shared by every repository's loop (#8).
    pub(crate) leases: Arc<crate::leases::Leases>,
    /// The waiting cards of every repository, which the loop sets after each plan (#5).
    pub(crate) blockers: Arc<crate::blockers::Blockers>,
    /// The trees every repository's loop runs, shared as the lease manager is: the process
    /// sweep spares them (#2). [`Loop::new`] makes [`Deps::spawner`] and [`Deps::sessions`]
    /// guard theirs in it.
    pub(crate) live: Arc<sweep::Live>,
    /// The free space of the disk floor of worktree mode (#8): the system's
    /// ([`workspace::Disk`]) in the daemon.
    pub(crate) free_space: Arc<dyn bosun_git::worktree::FreeSpace + Send + Sync>,
    /// The pull requests of the `pr` landing mode (#8): `gh` ([`GhForge`]) in the daemon.
    pub(crate) forge: Arc<dyn forge::Forge>,
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
    /// The worktrees and setup children of this loop's own, for the sweep (#8).
    pub(crate) worktrees: workspace::Registry,
}

impl Shared {
    fn publish(&self, body: EventBody) {
        self.deps.events.publish(body);
    }

    /// Logs a problem of the repository and publishes it as `board_sync_error`.
    fn trouble(&self, error: impl Into<String>) {
        let error = error.into();
        crate::log::log!("bosund: repository {}: {error}", self.options.repo);
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

/// Whether `stored` is an item of the configured repository `name` (`repo`): its row has that
/// name, wherever its path is now (a moved repository keeps its items, #132). A row without a
/// name (nothing registered it by name yet) is matched by its path.
pub(crate) fn of_repo(stored: &StoredItem, name: &str, repo: &Repo) -> bool {
    // Mutant (`mutant_store_repo_by_path`): items are matched by path, so a moved repository's
    // items are lost to its loop.
    if cfg!(mutant_store_repo_by_path) {
        return stored.repo == repo.key;
    }
    match &stored.repo_name {
        Some(stored_name) => stored_name == name,
        None => stored.repo == repo.key,
    }
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
#[derive(Debug, Clone)]
pub(crate) struct Claimed {
    /// The snapshot the item runs from: its verdict, prose and inlined comments.
    pub(crate) snapshot: bosun_queue::Snapshot,
    /// Where the item's files are: its worktree in worktree mode, else the repository's
    /// checkout (`repo.path`). The session, the guards, the preconditions and the verdict run
    /// here; the identity checks look at `repo.path` (#2).
    pub(crate) workdir: PathBuf,
    /// Worktree mode (#8, E19): the item's worktree and the user's checkout at claim.
    pub(crate) worktree: Option<ItemWorktree>,
    /// The working directory at claim, for the preconditions. In worktree mode its refs follow
    /// the target when landing moves it ([`landing`]).
    pub(crate) base: bosun_git::Base,
    /// Git's own configuration at claim, for the guards (#59).
    pub(crate) git_config: bosun_git::GitConfig,
    /// The repository's configuration hash at claim.
    pub(crate) repo_hash: String,
    /// The repository's policy hash at claim.
    pub(crate) policy_hash: String,
}

/// An item's worktree off the daemon's clone (#8), with the user's checkout as captured at
/// claim (E4, E21).
#[derive(Debug, Clone)]
pub(crate) struct ItemWorktree {
    /// The repository's workspace at claim: what closes the worktree.
    pub(crate) workspace: workspace::Workspace,
    /// The worktree: its directory is [`Claimed::workdir`].
    pub(crate) worktree: bosun_git::worktree::Worktree,
    /// The user's checkout at claim, compared before every verdict; its target follows
    /// landing.
    pub(crate) user: checkout::UserCheckout,
    /// The branch the item started from and lands on (a bare name): `landing.target`, else
    /// the branch the user's checkout was on at claim.
    pub(crate) target: String,
    /// The clone's ref of [`Self::target`] the item started from, is rebased onto and has its
    /// own commits counted from ([`base_ref`]).
    pub(crate) base_ref: String,
}

/// The ref of the daemon's clone that an item of `repo` landing on `target` is based on, rebased
/// onto and compared with: the forge's target in `pr` mode (E28,
/// [`landing::forge_target_ref`]), else `refs/heads/<target>`, which follows the user's branch
/// (E2, E14).
pub(crate) fn base_ref(repo: &Repo, target: &str) -> String {
    // Mutant (`mutant_loop_pr_based_on_local_target`): the user's local target in `pr` mode too.
    if repo.landing.mode == bosun_config::LandingMode::Pr
        && !cfg!(mutant_loop_pr_based_on_local_target)
    {
        landing::forge_target_ref(target)
    } else {
        format!("refs/heads/{target}")
    }
}

/// The branch `repo`'s items land on: `landing.target`, else the branch of the user's
/// checkout `user` (captured at claim). `None` for a detached HEAD without `landing.target`.
pub(crate) fn landing_target(repo: &Repo, user: &bosun_git::Base) -> Option<String> {
    repo.landing.target.clone().or_else(|| {
        user.branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_owned())
    })
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
    /// The worktree's `setup` command failed (#8): Needs you, not a failed attempt.
    Setup(String),
    /// The board refused the claim ([`Loop::board_error`] says what follows).
    Board(bosun_queue::QueueError),
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
        /// How the session ended, for the fast-death breaker ([`breaker`]): from the session
        /// task, for a session it spawned or attached; `None` from every other task.
        session: Option<breaker::SessionEnd>,
    },
    /// Landing moved the target (#8): what the item's preconditions and the checkout guard
    /// compare with from now on. Sent before the landing's event.
    Followed {
        item: ItemId,
        claimed: Arc<Claimed>,
    },
}

/// What the loop tells a running task (#8, E22): go on, end (the kill), or let go of what runs
/// on by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Go on.
    Run,
    /// End: the task kills what it started and reports its end. Every kill but the loop's
    /// graceful stop.
    Kill,
    /// The daemon stops gracefully (uninstall, upgrade): a session under its shim is left
    /// running with its `process` row and its files, for the next daemon to re-attach (E22,
    /// E3), and its task returns without an event. Every other task takes it for [`Kill`].
    ///
    /// [`Kill`]: Signal::Kill
    Detach,
}

/// A running effect: its kill signal and its task, aborted when this is dropped: a loop that
/// is dropped (its own task aborted) drops what its tasks hold, children and all.
#[derive(Debug)]
struct Task {
    kill: watch::Sender<Signal>,
    handle: JoinHandle<()>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
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
    /// The running session's child, for the wake check.
    child: park::ChildSlot,
    /// The ignored files before the first session of the attempt numbered here (#59).
    ignored: Option<(u32, Arc<bosun_git::Ignored>)>,
    /// How many times in a row the landing reported one head rebased in one attempt (#8):
    /// shared with the landing task, which refuses at [`landing::REBASED_LIMIT`].
    rebased: Arc<std::sync::Mutex<Option<landing::Streak>>>,
    /// Whether the census's unknown `allow-census` names were logged for this item (#7).
    census_logged: bool,
}

/// A lint result, cached per card body.
#[derive(Debug, Clone)]
struct Linted {
    body: String,
    claimable: bool,
    findings: Vec<String>,
    blockers: Vec<bosun_queue::ItemId>,
    /// The contract's `needs`: what the probes before a claim are of (#8).
    needs: Vec<String>,
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
    /// The last claim refusal logged: a refusal is logged when it starts and when its text
    /// changes, not at every poll (#186).
    refused: said::Said,
    /// No claim before this (after a failed claim).
    backoff: Option<Instant>,
    /// More than one item of this repository was in flight at start; the user sorts it out.
    waiting: bool,
    /// Crash recovery has run ([`recover`]); nothing is claimed or sent before.
    recovered: bool,
    /// Held until recovery has ended: while any loop holds one, an unknown agent credential is
    /// refused as retryable (#8, E10). A re-attached session takes it and drops it once its
    /// credential is admitted again.
    recovering: Option<crate::agent::Recovering>,
    /// The store failed a write or read the loop cannot go on without (any refusal of a
    /// transition but a stale one, #132): the loop stops.
    halted: bool,
    /// The sleep detector.
    gaps: park::Gaps,
    /// Holds and budgets.
    watch: hold::Watch,
    /// Wakes on every change of the control state.
    control: watch::Receiver<u64>,
    /// The outbox drainer's wake-up and what it renders from the board.
    board: Arc<outbox::Board>,
    /// Items whose `restart` label was acted on and is still on the card.
    restarts: BTreeSet<ItemId>,
    /// Items whose card the last poll did not see: a second poll without it is a cancel.
    absent: BTreeSet<ItemId>,
    /// When the board is read next (#106).
    pace: board::Pace,
    /// The poll of the open pull requests of Done items (#8).
    pulls: pulls::Watch,
    /// The fast-death breaker's streak, across items ([`breaker`]).
    streak: breaker::Streak,
    /// The item the loop last claimed, and when (ms), when it has claimed one since it last
    /// queued the `ready_drained` notice (#9): the next poll with nothing claimable queues it.
    claimed_since_drained: Option<(ItemId, i64)>,
}

/// Aborts its task when dropped: the drainer ends with its loop.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
        let gaps = park::Gaps::new(options.poll);
        let control = deps.control.subscribe();
        let pace = board::Pace::new(options.idle_backoff, options.poll);
        // Raised before the API serves, so no call meets a credential not re-admitted yet as
        // unknown.
        let recovering = deps.credentials.recovering();
        let recovering = if cfg!(mutant_agent_recovering_never_ends) {
            // Mutant (`mutant_agent_recovering_never_ends`): the guard is leaked, never dropped.
            std::mem::forget(recovering);
            None
        } else {
            Some(recovering)
        };
        // Every tree the loop starts is guarded from the other loops' process sweeps.
        let deps = Deps {
            spawner: Arc::new(sweep::Tracking {
                inner: deps.spawner.clone(),
                live: deps.live.clone(),
            }),
            sessions: Arc::new(sweep::TrackingShim {
                inner: deps.sessions.clone(),
                live: deps.live.clone(),
            }),
            ..deps
        };
        Self {
            shared: Arc::new(Shared {
                options,
                deps,
                worktrees: workspace::Registry::default(),
            }),
            config,
            current: None,
            done_tx,
            done_rx,
            cursor: None,
            lints: BTreeMap::new(),
            checked: None,
            refused: said::Said::default(),
            backoff: None,
            waiting: false,
            recovered: false,
            recovering,
            halted: false,
            gaps,
            watch: hold::Watch::default(),
            control,
            board: Arc::default(),
            restarts: BTreeSet::new(),
            absent: BTreeSet::new(),
            pace,
            pulls: pulls::Watch::default(),
            streak: breaker::Streak::default(),
            claimed_since_drained: None,
        }
    }

    /// Runs until the store refuses a write other than a stale transition (the loop cannot
    /// record what it does any more; a stale transition costs its item only,
    /// [`Loop::stale_refused`]) or
    /// the configuration channel closes (the daemon stops). At a stop the task in flight is sent
    /// [`Signal::Detach`] and awaited: a session under its shim is left running for the next
    /// daemon (E22), anything else is killed. Aborting the task kills the child in flight: its
    /// container is dropped.
    pub(crate) async fn run(mut self) {
        // The drainer starts once crash recovery has read the board (#68).
        let mut drainer = None;
        let mut tick = tokio::time::interval(self.shared.options.poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !self.halted {
            if drainer.is_none() && self.recovered {
                drainer = Some(AbortOnDrop(tokio::spawn(outbox::run(
                    self.shared.clone(),
                    self.board.clone(),
                    self.config.clone(),
                ))));
            }
            tokio::select! {
                Some(done) = self.done_rx.recv() => self.on_done(done).await,
                changed = self.config.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.on_config().await;
                }
                _ = tick.tick() => self.on_tick().await,
                Ok(()) = self.control.changed() => self.on_control().await,
            }
        }
        // A graceful stop detaches (E22): a session under its shim goes on for the next daemon;
        // every other task is killed. A task killed already stays killed, and a loop the store
        // halted kills.
        let stop = if self.halted {
            Signal::Kill
        } else {
            Signal::Detach
        };
        if let Some(cur) = self.current.as_mut()
            && let Some(mut task) = cur.task.take()
        {
            task.kill.send_if_modified(|signal| {
                let running = *signal == Signal::Run;
                if running {
                    *signal = stop;
                }
                running
            });
            let _ = (&mut task.handle).await;
        }
        // A halted loop runs nothing more for its item, so it frees what the item holds: its
        // journal entry, this daemon's, would otherwise keep the leases while the daemon runs.
        // A graceful stop keeps them journalled for the next daemon (E22). A claim still in
        // flight released its own when it was killed.
        // Mutant (`mutant_loop_halt_keeps_leases`): a halted loop keeps them.
        if self.halted
            && !cfg!(mutant_loop_halt_keeps_leases)
            && let Some(cur) = &self.current
            && cur.claimed.as_ref().is_some_and(|c| c.worktree.is_some())
        {
            self.release_leases(&cur.item.id).await;
        }
    }

    fn config(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }

    fn ctx(&self) -> Ctx {
        let config = self.config();
        let park_cap = if cfg!(mutant_loop_no_park_cap) {
            // Mutant: the configured cap of uncounted parks is not applied.
            u32::MAX
        } else {
            config.daemon.park_cap
        };
        let now_ms = self.shared.deps.clock.wall_ms();
        let mut rules = Rules {
            park_cap,
            ..Rules::default()
        };
        // The repository's own not-ready cap and backoff (#204), from the live configuration.
        if let Some((repo, _)) = lookup(&config, &self.shared.options.repo) {
            rules.unready_cap = repo.verdict.unready_cap;
            rules.unready_backoff_ms =
                i64::try_from(repo.verdict.unready_backoff.as_millis()).unwrap_or(i64::MAX);
        }
        Ctx {
            now_ms,
            hold: self.hold(now_ms),
            rules,
        }
    }

    /// Writes one arrow and publishes it. The row is written before any effect (#1).
    async fn record(&self, stepped: &Stepped) -> Result<(), bosun_store::Error> {
        #[cfg(test)]
        if let Some(failpoint) = &self.shared.options.failpoint
            && let Some(e) = failpoint(&stepped.transition)
        {
            return Err(e);
        }
        self.shared
            .deps
            .writer
            .write(vec![Write::Transition {
                stepped: stepped.clone(),
                at_ms: now_ms(),
            }])
            .await?;
        self.board.wake();
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
            // A stale one sends the item to Needs you; the arrow of that is a bounce too, its
            // effects the board writes queued with its row.
            let _ = self.stale_refused(&stepped.item.id, e).await;
        }
    }

    /// The store refused a transition of `id` with `error` (#132). A stale transition (the
    /// loop's view of the item was older than its stored history) costs the item only: the
    /// refusal is logged and stored as the item's [`outbox::NOTE_STALE`] note, the item is read
    /// again, and [`Event::Stale`] is stepped from the stored item and recorded, so it goes to
    /// Needs you with the refusal in its comment. A stored item that cannot move (already Needs
    /// you, Done or Cancelled), or whose arrow is refused as stale again, is skipped: nothing
    /// more is written for it, and the stale arrow is never retried. A stored item with no row
    /// is skipped before anything is written for it. Any other refusal, of the transition or
    /// of what this writes and reads, halts the loop ([`Loop::halt`]).
    ///
    /// Returns `None` when the loop halts; else the stored item (`None` when it has no row)
    /// and the arrow [`Event::Stale`] took from it, recorded, whose effects the caller performs.
    async fn stale_refused(
        &mut self,
        id: &ItemId,
        error: bosun_store::Error,
    ) -> Option<(Option<Item>, Option<Stepped>)> {
        let stale = if cfg!(mutant_loop_stale_on_any_refusal) {
            // Mutant (`mutant_loop_stale_on_any_refusal`): every refusal costs its item only.
            true
        } else {
            // Mutant (`mutant_loop_halt_on_stale`): a stale transition halts the loop too.
            matches!(error, bosun_store::Error::Stale(_)) && !cfg!(mutant_loop_halt_on_stale)
        };
        if !stale {
            self.halt("a transition", &error.to_string());
            return None;
        }
        let error = error.to_string();
        // Read first: a note needs the item's row, and an item with none is skipped.
        let stored = match self.shared.deps.reader.item(id).await {
            Ok(Some(stored)) => stored.item,
            Ok(None) => {
                self.shared
                    .trouble(format!("{id}: {error}; the item has no row and is skipped"));
                return Some((None, None));
            }
            Err(e) => {
                self.halt("a read of the item", &e.to_string());
                return None;
            }
        };
        self.shared
            .trouble(format!("{id}: {error}; the item goes to Needs you"));
        let note = Write::ItemNote {
            item: id.clone(),
            kind: outbox::NOTE_STALE.to_owned(),
            body: error,
            at_ms: now_ms(),
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.halt("the item's stale note", &e.to_string());
            return None;
        }
        let Some(stepped) = step(&stored, &Event::Stale, &self.ctx()) else {
            return Some((Some(stored), None));
        };
        match self.record(&stepped).await {
            Ok(()) => Some((Some(stored), Some(stepped))),
            // No third try: the item is skipped.
            Err(e @ bosun_store::Error::Stale(_)) => {
                self.shared
                    .trouble(format!("{id}: {e}; the item is skipped"));
                Some((Some(stored), None))
            }
            Err(e) => {
                self.halt("a transition", &e.to_string());
                None
            }
        }
    }

    /// The store failed `what`, a write or read the loop cannot go on without (a transition
    /// refused other than as stale, a note, an effect, a registration): the loop stops.
    fn halt(&mut self, what: &str, error: &str) {
        self.shared
            .trouble(format!("the store refused {what}, the loop stops: {error}"));
        self.halted = true;
    }

    /// A board error of a poll or a claim (#5, #61). One that pauses the repository (the
    /// board does not match the configuration, the token lost a scope, another daemon claims
    /// on this board) holds every claim for [`BOARD_RETRY`], then the board is asked again; a
    /// rate limit holds them for the wait the server named; anything else is retried at the
    /// next poll.
    fn board_error(&mut self, what: &str, error: &bosun_queue::QueueError) {
        let hold = if error.pauses_repository() {
            self.shared.trouble(format!(
                "{what}: {error}; the repository is paused, retried in {} min",
                BOARD_RETRY.as_secs() / 60
            ));
            Some(BOARD_RETRY)
        } else if let bosun_queue::QueueError::RateLimited(limit) = error {
            let now = u64::try_from(now_ms() / 1000).unwrap_or_default();
            let wait = limit.wait(now);
            self.shared
                .trouble(format!("{what}: {error}; waiting {} s", wait.as_secs()));
            Some(wait)
        } else {
            self.shared.trouble(format!("{what}: {error}"));
            None
        };
        if let Some(hold) = hold {
            self.backoff = Some(Instant::now() + hold);
        }
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
                    self.halt("a transition", &e.to_string());
                    return;
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.item = stepped.item.clone();
                }
                pending.extend(follow);
            } else {
                if let Err(e) = self.record(&stepped).await {
                    self.refused_in_flight(e).await;
                    return;
                }
                if let Some(cur) = self.current.as_mut() {
                    cur.item = stepped.item.clone();
                }
                pending.extend(self.perform(&stepped.transition.effects).await);
            }
            let state = stepped.item.state;
            // A parked item waits for its limit's reset, often for hours, holding nothing (E24):
            // its next session asks again, all or nothing ([`hold_leases`]).
            // Mutant (`mutant_loop_hold_leases_while_parked`): it keeps them while parked.
            if matches!(state, State::Parked(_))
                && !cfg!(mutant_loop_hold_leases_while_parked)
                && let Some(cur) = &self.current
                && cur.claimed.as_ref().is_some_and(|c| c.worktree.is_some())
            {
                self.release_leases(&cur.item.id).await;
            }
            if !state.is_active() {
                self.finish().await;
                return;
            }
        }
    }

    /// A transition of the item in flight was refused with `error` ([`Loop::stale_refused`]).
    /// Unless the loop halts, the stored item replaces the loop's view of it, the effects of the arrow
    /// [`Event::Stale`] took are performed (its child is killed), and the item leaves the loop
    /// ([`Loop::finish`]) whether it stepped or not: the loop is free for the next card.
    async fn refused_in_flight(&mut self, error: bosun_store::Error) {
        let Some(id) = self.current.as_ref().map(|c| c.item.id.clone()) else {
            return;
        };
        let Some((stored, stepped)) = self.stale_refused(&id, error).await else {
            return;
        };
        let item = stepped.as_ref().map(|s| s.item.clone()).or(stored);
        if let (Some(cur), Some(item)) = (self.current.as_mut(), item) {
            cur.item = item;
        }
        if let Some(stepped) = stepped {
            // The arrow ends the item: what its effects would start next is dropped.
            let _ = self.perform(&stepped.transition.effects).await;
        }
        self.finish().await;
    }

    /// Performs `effects` in order; returns the events of those that complete at once.
    async fn perform(&mut self, effects: &[Effect]) -> Vec<Event> {
        let mut follow = Vec::new();
        for effect in effects {
            match effect.kind {
                EffectKind::Claim => {
                    // A restart claims again: the old attempt's work is set aside first.
                    self.before_claim().await;
                    self.start_claim(effect);
                }
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
                        let _ = task.kill.send(Signal::Kill);
                    }
                }
                EffectKind::CheckPreconditions { attempt } => {
                    follow.push(self.preconditions(attempt).await);
                }
                EffectKind::RunVerdict { attempt, run } => self.start_verdict(attempt, run).await,
                // `local` landing (#7): nothing to push; the trailers are injected at commit
                // time (#59) and the report is the outbox's (#65), which landing never waits
                // for: an outage of the board never stops an item.
                // Worktree mode (#8, E19): the landing task rebases, verifies and delivers.
                EffectKind::Land { attempt } if self.worktree_mode() => {
                    self.start_landing(attempt).await;
                }
                EffectKind::Land { .. } => {
                    if cfg!(mutant_outbox_stop_on_outage) {
                        // Mutant: landing waits until the board has every write of the item.
                        self.wait_for_board().await;
                    }
                    follow.push(Event::Landed);
                }
                // Queued in the outbox with the transition; sent by the drainer.
                EffectKind::Project(_) | EffectKind::Comment(_) | EffectKind::Report => {}
            }
        }
        follow
    }

    /// Replaces the task in flight (the previous one has ended or is being killed).
    async fn set_task(&mut self, kill: watch::Sender<Signal>, handle: JoinHandle<()>) {
        let Some(cur) = self.current.as_mut() else {
            handle.abort();
            return;
        };
        if let Some(mut old) = cur.task.replace(Task { kill, handle }) {
            let _ = old.kill.send(Signal::Kill);
            let _ = (&mut old.handle).await;
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
        let (kill, kill_rx) = watch::channel(Signal::Run);
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
            let _ = old.kill.send(Signal::Kill);
            old.handle.abort();
        }
    }

    /// `mutant_outbox_stop_on_outage`: waits until the item in flight has no write left in the
    /// outbox.
    async fn wait_for_board(&self) {
        let Some(item) = self.current.as_ref().map(|c| c.item.id.clone()) else {
            return;
        };
        loop {
            match outbox::pending(&self.shared.deps.reader, &self.shared.options.repo).await {
                Ok(rows) if rows.iter().all(|r| r.item != item) => return,
                Err(_) => return,
                Ok(_) => tokio::time::sleep(self.shared.options.poll).await,
            }
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
            // The repository was removed from the configuration (#186): its item in flight
            // goes to Needs you (`config_changed`), and its loop claims nothing new
            // (`schedule`).
            // Mutant (`mutant_loop_removed_repo_keeps_item`): a removed repository counts as
            // unchanged.
            None => !cfg!(mutant_loop_removed_repo_keeps_item),
        }
    }

    async fn start_session(&mut self, attempt: u32, spawn: u32, mode: SpawnMode) -> Option<Event> {
        let cur = self.current.as_ref()?;
        let claimed = cur.claimed.clone()?;
        if self.hashes_changed(&claimed) {
            return Some(Event::ConfigChanged);
        }
        if cur.ignored.as_ref().is_none_or(|(n, _)| *n != attempt) {
            self.record_guards(attempt, &claimed).await;
        }
        self.start_session_task(attempt, spawn, mode, claimed, None)
            .await;
        None
    }

    /// Follows the session a previous daemon left running under its shim (#8, E3), found by
    /// crash recovery, as the item's session task: no checks, no spawn, no new run.
    async fn start_attached(&mut self, attached: session::Attached) {
        let Some(claimed) = self.current.as_ref().and_then(|c| c.claimed.clone()) else {
            return;
        };
        let (attempt, spawn, mode) = (attached.run.attempt, attached.run.spawn, attached.run.mode);
        // The guard of recovery goes with the session, dropped once its credential is admitted.
        let recovering = self.recovering.take();
        self.start_session_task(attempt, spawn, mode, claimed, Some((attached, recovering)))
            .await;
    }

    /// Starts the task of a session: a new one, or the `attached` one of a previous daemon.
    async fn start_session_task(
        &mut self,
        attempt: u32,
        spawn: u32,
        mode: SpawnMode,
        claimed: Arc<Claimed>,
        attached: Option<(session::Attached, Option<crate::agent::Recovering>)>,
    ) {
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        let (kill, kill_rx) = watch::channel(Signal::Run);
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
                    session: None,
                });
            })
        };
        let child = park::ChildSlot::default();
        let slot = child.clone();
        // The shim's deadman (E9): what is left of the item's wall-clock budget, the figure
        // the loop's own check compares ([`hold`]).
        let budget = claimed
            .snapshot
            .contract
            .budget
            .wall
            .or(config.budget.item_wall_clock);
        let shim_cap = session::shim_cap(budget, self.active_time());
        let handle = tokio::spawn(async move {
            let run = session::Run {
                item: item.clone(),
                attempt,
                spawn,
                mode,
                child: slot,
                shim_cap,
            };
            // `None`: the session was detached at a graceful stop (E22); there is no event.
            let event = match attached {
                Some((attached, recovering)) => {
                    session::attach(
                        &shared,
                        &config,
                        &claimed,
                        &run,
                        attached,
                        kill_rx,
                        on_blocking,
                        recovering,
                    )
                    .await
                }
                None => {
                    session::session(&shared, &config, &claimed, &run, kill_rx, on_blocking).await
                }
            };
            let Some((event, session)) = event else {
                return;
            };
            // The session has ended: the process sweep runs before the item's next step (E12).
            sweep::after_session(&shared, &config, &item, attempt).await;
            let _ = done.send(Done::Event {
                item,
                event,
                failing: Vec::new(),
                session,
            });
        });
        self.set_task(kill, handle).await;
        if let Some(cur) = self.current.as_mut() {
            cur.child = child;
        }
    }

    /// Before the first session of `attempt`: records the ignored files (#59) and stores them
    /// with git's configuration at claim on the attempt's row, for crash recovery (#68). A
    /// snapshot that cannot be taken leaves none, and the attempt fails its preconditions.
    async fn record_guards(&mut self, attempt: u32, claimed: &Claimed) {
        let path = claimed.workdir.clone();
        let ignored =
            match blocking(move || bosun_git::Ignored::capture(&bosun_git::Git::new(path))).await {
                Ok(ignored) => Arc::new(ignored),
                Err(e) => {
                    self.shared
                        .trouble(format!("the ignored files cannot be recorded: {e}"));
                    return;
                }
            };
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        cur.ignored = Some((attempt, ignored.clone()));
        let write = Write::AttemptGuards {
            item: cur.item.id.clone(),
            attempt,
            ignored_json: serde_json::Value::from(
                ignored.paths.iter().cloned().collect::<Vec<_>>(),
            )
            .to_string(),
            git_config_json: serde_json::Value::Object(
                claimed
                    .git_config
                    .files
                    .iter()
                    .map(|(path, digest)| {
                        (
                            path.to_string_lossy().into_owned(),
                            serde_json::Value::from(digest.clone()),
                        )
                    })
                    .collect(),
            )
            .to_string(),
        };
        if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
            self.shared
                .trouble(format!("the guard snapshots cannot be stored: {e}"));
        }
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
        let unready = cur.item.unready;
        let (kill, kill_rx) = watch::channel(Signal::Run);
        let shared = self.shared.clone();
        let config = self.config();
        let item = cur.item.id.clone();
        let done = self.done_tx.clone();
        // The census (#7) follows a pass of the item's own commits, never a landing's
        // re-verdict of a rebased head.
        let census = (cur.item.state == State::Verifying)
            .then(|| lookup(&config, &self.shared.options.repo))
            .flatten()
            .and_then(|(repo, _)| repo.policy.census.clone())
            // Mutant (`mutant_loop_skip_census`): the census never runs.
            .filter(|_| !cfg!(mutant_loop_skip_census));
        if let Some(census) = &census
            && let Some(cur) = self.current.as_mut()
            && !std::mem::replace(&mut cur.census_logged, true)
        {
            census::log_unknown_allowances(
                &self.shared.options.repo,
                &item,
                census,
                &claimed.snapshot.contract.allow_census,
            );
        }
        let handle = tokio::spawn(async move {
            let spec = verdict::Run {
                item: item.clone(),
                attempt,
                number: run,
                first_failing,
                unready,
            };
            let (mut event, failing) =
                verdict::verdict(&shared, &config, &claimed, &spec, kill_rx).await;
            if let Some(census) = &census
                && let Event::Verdict {
                    result: bosun_core::RunResult::Pass,
                    ..
                } = event
                && let Some(dropped) =
                    census::after_pass(&shared, &claimed, census, &item, attempt, run).await
            {
                event = dropped;
            }
            let _ = done.send(Done::Event {
                item,
                event,
                failing,
                session: None,
            });
        });
        self.set_task(kill, handle).await;
    }

    /// Whether the item in flight runs in its own worktree (#8, E19).
    fn worktree_mode(&self) -> bool {
        self.current
            .as_ref()
            .and_then(|c| c.claimed.as_ref())
            .is_some_and(|c| c.worktree.is_some())
    }

    /// Starts the landing task of `attempt` (worktree mode, [`landing`]).
    async fn start_landing(&mut self, attempt: u32) {
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        let Some(claimed) = cur.claimed.clone() else {
            return;
        };
        let (kill, kill_rx) = watch::channel(Signal::Run);
        let shared = self.shared.clone();
        let config = self.config();
        let item = cur.item.id.clone();
        let title = cur.card.title.clone();
        let card_number = cur.card.id.0;
        // The verdict run whose pass emitted this `Land`: it keys a conflict to this pass.
        let run = cur.item.attempt.as_ref().map_or(0, |a| a.run);
        let done = self.done_tx.clone();
        let streak = cur.rebased.clone();
        let handle = tokio::spawn(async move {
            let spec = landing::Spec {
                item: item.clone(),
                attempt,
                run,
                title,
                card: card_number,
            };
            let mut landed = landing::land(&shared, &config, &claimed, &spec, kill_rx).await;
            if let Some(head) = landed.rebased.take() {
                // A verified head the verdict cannot record comes back rebased each time.
                let next = {
                    let mut slot = streak
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let next = landing::rebased_again(slot.as_ref(), attempt, &head);
                    *slot = Some(next.clone());
                    next
                };
                if next.refused() {
                    shared.trouble(format!(
                        "{item}: the landing is refused: the verified commit {head} cannot be \
                         recorded (reported rebased {} times)",
                        next.count
                    ));
                    landed.event = Some(Event::LandingRefused);
                }
            }
            if let Some(claimed) = landed.followed {
                let _ = done.send(Done::Followed {
                    item: item.clone(),
                    claimed: Arc::new(claimed),
                });
            }
            if let Some(event) = landed.event {
                let _ = done.send(Done::Event {
                    item,
                    event,
                    failing: Vec::new(),
                    session: None,
                });
            }
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
        let Some(ignored) = cur
            .ignored
            .as_ref()
            .filter(|(n, _)| *n == attempt)
            .map(|(_, ignored)| ignored.clone())
        else {
            self.shared.trouble(format!(
                "attempt {attempt} has no ignored-file snapshot; it fails its preconditions"
            ));
            let item = cur.item.id.clone();
            let why = "the ignored files were not recorded before the session".to_owned();
            return self.unchecked(&item, attempt, why).await;
        };
        let item = cur.item.id.clone();

        // Worktree mode: the user's checkout first (E4, E21), before anything else is looked at.
        if let Some(event) = self.checkout_guard(&item, attempt, &claimed, repo).await {
            return event;
        }

        // The guards first: a refusal is Needs you, whatever the preconditions say.
        let (guard_repo, guard_claimed) = (repo.clone(), claimed.clone());
        let refused = blocking(move || guards(&guard_repo, &guard_claimed, &ignored)).await;
        match refused {
            Ok(refusals) => {
                if let Some((guard, _)) = refusals.first() {
                    let guard = *guard;
                    let text: Vec<String> = refusals.iter().map(|(_, r)| r.to_string()).collect();
                    self.shared.trouble(format!(
                        "{item}: attempt {attempt} is refused before its verdict: {}",
                        text.join("; ")
                    ));
                    self.refused(&item, attempt, text).await;
                    return Event::Refused { attempt, guard };
                }
            }
            Err(e) => {
                let why = format!("the guards could not be checked: {e}");
                self.shared.trouble(why.clone());
                return self.unchecked(&item, attempt, why).await;
            }
        }

        self.git_preconditions(&item, attempt, claimed).await
    }

    /// The verdict preconditions of `bosun-git`. In worktree mode (E19) the verdict runs in a
    /// checkout of the commit, so what is left uncommitted is noted, not refused, and a rebase
    /// the session left stopped is aborted first ([`landing::abort_stale_rebase`]): its partial
    /// pick is never the head that is verified.
    async fn git_preconditions(&self, item: &ItemId, attempt: u32, claimed: Arc<Claimed>) -> Event {
        let path = claimed.workdir.clone();
        if claimed.worktree.is_some()
            && let Err(e) = landing::abort_stale_rebase(&self.shared, item, path.clone()).await
        {
            let why = format!("the rebase left in the item's worktree cannot be aborted: {e}");
            self.shared.trouble(why.clone());
            return self.unchecked(item, attempt, why).await;
        }
        let runs = self.shared.deps.reader.runs(item).await.unwrap_or_default();
        let checked = {
            let item = item.clone();
            blocking(move || {
                // Mutant (`mutant_loop_pr_message_range_local_target`): the item's own commits
                // are counted from the user's local target in `pr` mode too.
                let from = claimed.worktree.as_ref().map(|wt| {
                    if cfg!(mutant_loop_pr_message_range_local_target) {
                        format!("refs/heads/{}", wt.target)
                    } else {
                        wt.base_ref.clone()
                    }
                });
                git::preconditions(&path, &claimed.base, &item, &runs, from.as_deref())
            })
            .await
        };
        if let Ok(checked) = &checked
            && !checked.uncommitted.is_empty()
        {
            self.note_uncommitted(item, attempt, &checked.uncommitted)
                .await;
        }
        match checked.map(|c| c.reasons) {
            Ok(reasons) if reasons.is_empty() => Event::Preconditions {
                attempt,
                passed: true,
            },
            Ok(reasons) => {
                let text: Vec<String> = reasons.iter().map(ToString::to_string).collect();
                crate::log::log!(
                    "bosund: repository {}: {item}: attempt {attempt} fails its preconditions: {}",
                    self.shared.options.repo,
                    text.join("; ")
                );
                self.refused(item, attempt, text).await;
                Event::Preconditions {
                    attempt,
                    passed: false,
                }
            }
            Err(e) => {
                let why = format!("the preconditions could not be checked: {e}");
                self.shared.trouble(why.clone());
                self.unchecked(item, attempt, why).await
            }
        }
    }

    /// Worktree mode (E4, E21): the user's checkout compared with its capture at claim. A
    /// refusal is `Refused` with `guard_checkout`; changed status lines are only reported;
    /// `None` when the attempt goes on to the other guards.
    async fn checkout_guard(
        &self,
        item: &ItemId,
        attempt: u32,
        claimed: &Claimed,
        repo: &Repo,
    ) -> Option<Event> {
        let wt = claimed.worktree.as_ref()?;
        if cfg!(mutant_loop_skip_checkout_guard) {
            // Mutant (`mutant_loop_skip_checkout_guard`): the user's checkout is not compared.
            return None;
        }
        let (user, before) = (repo.path.clone(), wt.user.clone());
        let diff = match blocking(move || checkout::check(&user, &before)).await {
            Ok(diff) => diff,
            Err(e) => {
                let why = format!("the user's checkout could not be compared: {e}");
                self.shared.trouble(why.clone());
                return Some(self.unchecked(item, attempt, why).await);
            }
        };
        if !diff.warnings.is_empty() {
            self.shared.trouble(format!(
                "{item}: the user's checkout changed during attempt {attempt} (a warning, the \
                 item goes on): {}",
                diff.warnings.join("; ")
            ));
        }
        if diff.refusals.is_empty() {
            return None;
        }
        self.shared.trouble(format!(
            "{item}: attempt {attempt} is refused before its verdict: the user's checkout \
             changed: {}",
            diff.refusals.join("; ")
        ));
        let text = diff
            .refusals
            .iter()
            .map(|r| format!("the user's checkout changed: {r}"))
            .collect();
        self.refused(item, attempt, text).await;
        Some(Event::Refused {
            attempt,
            guard: bosun_core::Guard::Checkout,
        })
    }

    /// Stores why `attempt` was refused before its verdict (#137), in the daemon's words, before
    /// the event that fails it: the report, `bosun item` and the next attempt's prompt read
    /// them from the attempt's row.
    async fn refused(&self, item: &ItemId, attempt: u32, reasons: Vec<String>) {
        if cfg!(mutant_executor_drop_precondition_reasons) {
            // Mutant: the reasons go to the log only.
            return;
        }
        let write = Write::AttemptRefusal {
            item: item.clone(),
            attempt,
            reasons,
        };
        if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
            self.shared
                .trouble(format!("the reasons of the refusal cannot be stored: {e}"));
        }
    }

    /// Worktree mode: `paths` were left uncommitted by `attempt`. They are stored as the item's
    /// `uncommitted` note and reported once; the verdict does not see them.
    async fn note_uncommitted(&self, item: &ItemId, attempt: u32, paths: &[String]) {
        self.shared.trouble(format!(
            "{item}: attempt {attempt} left {} uncommitted path(s) in its worktree, which the \
             verdict does not see: {}",
            paths.len(),
            paths.join(", ")
        ));
        let note = Write::ItemNote {
            item: item.clone(),
            kind: outbox::NOTE_UNCOMMITTED.to_owned(),
            body: serde_json::to_string(paths).unwrap_or_default(),
            at_ms: now_ms(),
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.shared.trouble(format!(
                "the uncommitted paths of {item} cannot be stored: {e}"
            ));
        }
    }

    /// `attempt` fails its preconditions because they could not be checked: `why` is stored as
    /// its reason.
    async fn unchecked(&self, item: &ItemId, attempt: u32, why: String) -> Event {
        self.refused(item, attempt, vec![why]).await;
        Event::Preconditions {
            attempt,
            passed: false,
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
                let _ = task.kill.send(Signal::Kill);
                let _ = (&mut task.handle).await;
            }
        }
        let done = matches!(cur.item.state, State::Done(_));
        // Mutant (`mutant_loop_release_before_teardown`): the leases go before the worktree's
        // teardown, which then runs under the shared build without them.
        if cfg!(mutant_loop_release_before_teardown)
            && cur.claimed.as_ref().is_some_and(|c| c.worktree.is_some())
        {
            self.release_leases(&cur.item.id).await;
        }
        if let Some(claimed) = &cur.claimed
            && let Some(wt) = &claimed.worktree
        {
            // Worktree mode: the worktree and its branch go. A Done item's work is on the
            // target (landing, #8); any other item's is kept in the clone under its aside ref.
            let aside = (!done).then_some(&cur.item.id);
            // The clone's offer of a landed commit goes whatever the item's end: a landing
            // killed while deferred, or one a crashed daemon left (#8, E14).
            if !cfg!(mutant_loop_keep_landing_ref) {
                let (clone, offered) = (wt.workspace.clone_path(), git::landing_ref(&cur.item.id));
                let dropped = blocking(move || {
                    bosun_git::Git::new(clone).run(&["update-ref", "-d", &offered], None)
                })
                .await;
                if let Err(e) = dropped {
                    self.shared.trouble(format!(
                        "{}: the landing ref cannot be removed: {e}",
                        cur.item.id
                    ));
                }
            }
            self.close_worktree(wt, aside).await;
            // Its leases are held through its verdicts, its landing and its teardown (#8, E1):
            // until now.
            if !cfg!(mutant_loop_release_before_teardown) {
                self.release_leases(&cur.item.id).await;
            }
        } else if !done && let Some(claimed) = &cur.claimed {
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

    /// Releases what `item` holds of the lease manager (#8); nothing happens when it holds
    /// nothing.
    async fn release_leases(&self, item: &ItemId) {
        self.shared
            .deps
            .leases
            .release(&crate::leases::Holder::Item(item.clone()))
            .await;
    }

    /// Closes the worktree of the item in flight (#8); `aside`: its work is kept first. What
    /// cannot be removed is quarantined and reported by the workspace; the loop goes on.
    async fn close_worktree(&self, wt: &ItemWorktree, aside: Option<&ItemId>) {
        let config = self.config();
        let Some((_, identity)) = lookup(&config, &self.shared.options.repo) else {
            self.shared.trouble(format!(
                "the worktree {} is not closed: the repository is no longer configured",
                wt.worktree.path.display()
            ));
            return;
        };
        wt.workspace
            .close(&self.shared, identity, &wt.worktree, aside, None)
            .await;
    }

    async fn on_done(&mut self, done: Done) {
        // Before anything else: the breaker observes the session of its item, current or not.
        let done = self.observe_session(done).await;
        let item = match &done {
            Done::Claim { item, .. } | Done::Event { item, .. } | Done::Followed { item, .. } => {
                item.clone()
            }
        };
        if self.current.as_ref().is_none_or(|cur| cur.item.id != item) {
            // A claim confirmed after its item left the loop: nobody takes its worktree.
            if let Done::Claim {
                outcome: ClaimOutcome::Confirmed(claimed),
                ..
            } = done
                && let Some(wt) = &claimed.worktree
            {
                self.close_worktree(wt, Some(&item)).await;
                // The claim's leases, kept for a first session that never comes.
                self.release_leases(&item).await;
            }
            return;
        }
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        match done {
            Done::Claim { outcome, .. } => {
                let event = match outcome {
                    ClaimOutcome::Confirmed(claimed) => {
                        cur.claimed = Some(Arc::from(claimed));
                        Event::ClaimConfirmed
                    }
                    ClaimOutcome::Lost => Event::ClaimLost,
                    ClaimOutcome::Lint(findings) => {
                        if let Err(e) =
                            outbox::note_findings(&self.shared.deps.writer, &item, &findings).await
                        {
                            self.shared
                                .trouble(format!("the lint findings of {item}: {e}"));
                        }
                        self.shared.publish(EventBody::LintFindings {
                            repo: self.shared.options.repo.clone(),
                            card: item.to_string(),
                            findings,
                        });
                        Event::LintFailed
                    }
                    ClaimOutcome::Setup(e) => {
                        self.shared.trouble(format!(
                            "the worktree of {item} could not be made or set up: {e}"
                        ));
                        Event::SetupFailed
                    }
                    ClaimOutcome::Failed(e) => {
                        self.shared.trouble(format!("claiming {item} failed: {e}"));
                        self.backoff = Some(Instant::now() + Duration::from_mins(1));
                        Event::ClaimLost
                    }
                    // The card left the board meanwhile (#5): the item is cancelled.
                    ClaimOutcome::Board(e) if e.is_cancel() => {
                        self.shared
                            .trouble(format!("claiming {item}: {e}; cancelled"));
                        Event::User(bosun_core::UserCommand::Cancel)
                    }
                    ClaimOutcome::Board(e) => {
                        self.board_error(&format!("claiming {item}"), &e);
                        if self.backoff.is_none() {
                            self.backoff = Some(Instant::now() + Duration::from_mins(1));
                        }
                        Event::ClaimLost
                    }
                };
                self.apply(event).await;
            }
            Done::Event { event, failing, .. } => {
                // A not-ready run judged nothing: the failing tests of the first run that did
                // stay (#204).
                if matches!(event, Event::Verdict { result, .. } if result != bosun_core::RunResult::Unready)
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
            Done::Followed { claimed, .. } => {
                if cur.claimed.is_some() {
                    cur.claimed = Some(claimed);
                }
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
        if !self.recovered {
            self.recovered = self.recover().await;
            if self.recovered {
                // Recovery has ended; a session it re-attached took the guard already.
                self.recovering = None;
            }
            return;
        }
        let clock = &self.shared.deps.clock;
        let gap = self.gaps.observe(clock.wall_ms(), clock.mono());
        if let Some(gap) = gap
            && !cfg!(mutant_loop_skip_wake_check)
        {
            // Mutant (`mutant_loop_skip_wake_check`): the child is not revalidated after a sleep.
            self.revalidate(gap).await;
        }
        self.watch_tick().await;
        self.poll_pull_requests().await;
        if self.halted {
            return;
        }
        // A lease whose holder is gone (E16): one of a `bosun with` command that ended.
        self.shared.deps.leases.release_stale();
        if let Some(cur) = self.current.as_ref() {
            if matches!(cur.item.state, State::Parked(_)) {
                self.apply(Event::Tick).await;
            }
            // The user's commands on the item in flight, and a card gone from the board (#106),
            // at the board's active pace.
            let now = self.shared.deps.clock.mono();
            if self.pace.due(now) && !cfg!(mutant_loop_no_midrun_poll) {
                // Mutant (`mutant_loop_no_midrun_poll`): the board is not read while an item is
                // in flight.
                self.sync_board().await;
                let (rate, wall) = self.quota();
                self.pace.polled(now, true, false, rate.as_ref(), wall);
            }
            return;
        }
        if self.waiting || self.backoff.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let now = self.shared.deps.clock.mono();
        if !self.pace.due(now) {
            return;
        }
        let cursor = self.cursor.clone();
        self.schedule().await;
        let (rate, wall) = self.quota();
        self.pace.polled(
            now,
            self.current.is_some(),
            self.cursor != cursor,
            rate.as_ref(),
            wall,
        );
    }

    /// The board's quota as last seen, and the Unix seconds now, for [`board::Pace`].
    fn quota(&self) -> (Option<bosun_queue::RateLimit>, u64) {
        let wall = self.shared.deps.clock.wall_ms() / 1000;
        (
            self.shared.deps.queue.rate_limit(),
            u64::try_from(wall).unwrap_or_default(),
        )
    }

    /// After a sleep: the running session's child is revalidated by its pid and start time
    /// (#7, Parking). A dead one is a crash of the attempt, which the machine retries once
    /// uncounted; its task is stopped, so its own late report is stale.
    async fn revalidate(&mut self, gap: Duration) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        let recorded = *cur
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(key), State::Running, Some(attempt)) =
            (recorded, cur.item.state, cur.item.attempt.as_ref())
        else {
            return;
        };
        let (number, spawn) = (attempt.number, attempt.spawn);
        let alive = self.shared.deps.alive.clone();
        if blocking(move || alive(key)).await {
            crate::log::log!(
                "bosund: repository {}: woke after {} s; the session of {} still runs",
                self.shared.options.repo,
                gap.as_secs(),
                cur.item.id
            );
            return;
        }
        self.shared.trouble(format!(
            "woke after {} s; the session of {} died: a crash of attempt {number}",
            gap.as_secs(),
            cur.item.id
        ));
        if let Some(mut task) = cur.task.take() {
            let _ = task.kill.send(Signal::Kill);
            let _ = (&mut task.handle).await;
        }
        self.apply(Event::Exited {
            attempt: number,
            spawn,
            exit: bosun_core::Exit::Crash,
            text: String::new(),
        })
        .await;
    }

    /// Polls the board and claims the next card, bouncing those that fail lint.
    #[allow(clippy::too_many_lines)]
    async fn schedule(&mut self) {
        let config = self.config();
        let repo_name = self.shared.options.repo.clone();
        // A repository removed from the configuration claims nothing new (#186): its loop runs
        // on until the next daemon start, and its item in flight went to Needs you
        // (`hashes_changed`).
        let Some((repo, identity)) = lookup(&config, &repo_name) else {
            return;
        };
        let Some(commands::Synced { changes, stored }) = self.sync_board().await else {
            return;
        };
        if stored.values().any(|s| s.item.state.is_active()) {
            // Recovery made the item in flight current; another one is not the loop's to guess.
            self.waiting = true;
            self.shared.trouble(
                "an item is in flight that the loop does not run; the repository waits for the \
                 user",
            );
            return;
        }

        let on_board: BTreeSet<u64> = changes.cards.iter().map(|c| c.id.0).collect();
        let mut cards = Vec::new();
        let mut bounced = Vec::new();
        for card in &changes.cards {
            let id = item_id(&repo_name, card.id);
            let free = stored.get(&id).is_none_or(|s| s.item.state == State::Ready);
            // A closed issue is never claimed (#111).
            let ready = card.column == Column::Ready && free && !card.closed;
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
        let Some(unmerged) = self.unmerged(repo).await else {
            return;
        };
        let satisfied: BTreeSet<ItemId> = stored
            .values()
            .filter(|s| matches!(s.item.state, State::Done(_)))
            .filter(|s| !pulls::waits_for_merge(s.item.state, unmerged.contains(&s.item.id)))
            .map(|s| s.item.id.clone())
            .collect();
        let now = self.shared.deps.clock.wall_ms();
        let plan = bosun_core::plan(&cards, &satisfied, &self.gates(now), now);
        // Published before the bounces below are written, so a Needs-you comment rendered for
        // one of them already names the cards waiting on it (the bounces change no blocker's
        // `satisfied`).
        // Mutant (`mutant_loop_blockers_unpublished`): the plan's waiting cards are ignored.
        if !cfg!(mutant_loop_blockers_unpublished) {
            self.shared
                .deps
                .blockers
                .set(&repo_name, plan.blocked.clone());
        }
        // A waiting card gets its row, so `item.get` answers for it.
        for waiting in &plan.blocked {
            if stored.contains_key(&waiting.item) {
                continue;
            }
            let Some(card) = changes
                .cards
                .iter()
                .find(|c| item_id(&repo_name, c.id) == waiting.item)
            else {
                continue;
            };
            if !self.register(repo, card, &waiting.item).await {
                return;
            }
        }
        for (card, findings) in bounced {
            let id = item_id(&repo_name, card.id);
            if !self.register(repo, &card, &id).await {
                return;
            }
            // Stored before the bounce queues its comment: a comment still pending when the
            // daemon restarts is posted with them (#111).
            if let Err(e) = outbox::note_findings(&self.shared.deps.writer, &id, &findings).await {
                self.halt("the lint findings", &e);
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
            self.shared
                .deps
                .blockers
                .set_probing(&repo_name, Vec::new());
            if self.gates(now).hold(&repo_name, now).is_none() {
                self.ready_drained().await;
            }
            return;
        };
        let Some(card) = changes
            .cards
            .iter()
            .find(|c| item_id(&repo_name, c.id) == *next)
        else {
            return;
        };
        match self.ready_to_claim(repo, identity).await {
            Err(e) => {
                // Mutant (`mutant_loop_claim_refusal_every_poll`): every refusal is logged.
                if self.refused.first(&e) || cfg!(mutant_loop_claim_refusal_every_poll) {
                    self.shared.trouble(e);
                }
                return;
            }
            // Mutant (`mutant_loop_refusal_kept_after_a_pass`): the last refusal is kept across
            // a claim that passed, so the same refusal later is not said.
            Ok(()) if cfg!(mutant_loop_refusal_kept_after_a_pass) => {}
            Ok(()) => self.refused.clear(),
        }
        // The probes of what the card would lease (#8): a failing one keeps it in Ready, and
        // the next claimable card of a new plan is tried the same way.
        let (mut next, mut card) = (next.clone(), card);
        let mut probing = Vec::new();
        // Mutant (`mutant_loop_skip_probe`): the loop claims without probing.
        while !cfg!(mutant_loop_skip_probe) {
            let needs = self
                .lints
                .get(&card.id.0)
                .map_or_else(Vec::new, |l| l.needs.clone());
            let names = probe_names(&needs, repo);
            let Some(failing) = health::check(&self.shared, repo, identity, &names, &next).await
            else {
                break;
            };
            health::tell(&self.shared, repo, &next, &failing).await;
            probing.push(crate::blockers::ProbeWait {
                item: next.clone(),
                resource: failing.resource,
                streak: failing.streak,
            });
            if let Some(waits) = cards.iter_mut().find(|c| c.item == next) {
                waits.ready = false;
            }
            // Mutant (`mutant_loop_probe_holds_the_repository`): the poll ends at the first
            // failing probe, so no other card is claimed while it fails.
            let again = if cfg!(mutant_loop_probe_holds_the_repository) {
                None
            } else {
                let plan = bosun_core::plan(&cards, &satisfied, &self.gates(now), now);
                plan.claim.first().and_then(|n| {
                    changes
                        .cards
                        .iter()
                        .find(|c| item_id(&repo_name, c.id) == *n)
                        .map(|c| (n.clone(), c))
                })
            };
            let Some((n, c)) = again else {
                // Not drained: the cards that wait on a probe are still Ready.
                self.wait_on_probes(repo, &changes.cards, &stored, probing)
                    .await;
                return;
            };
            (next, card) = (n, c);
        }
        if !self
            .wait_on_probes(repo, &changes.cards, &stored, probing)
            .await
        {
            return;
        }
        let next = &next;
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
            child: park::ChildSlot::default(),
            ignored: None,
            rebased: Arc::default(),
            census_logged: false,
        });
        self.claimed_since_drained = Some((next.clone(), now_ms()));
        self.apply(Event::Claim).await;
    }

    /// Nothing is claimable: queues the `ready_drained` notice (#9) if the loop claimed an item
    /// since it last queued one, keyed by the last item it claimed and the time of that claim:
    /// the same item claimed again (back in Ready) drains the column again, and that is a new
    /// notice. A loop that never claimed, and every idle poll after the first, queues nothing.
    async fn ready_drained(&mut self) {
        // Mutant (`mutant_loop_no_drained_notice`): a drained Ready column queues nothing.
        if cfg!(mutant_loop_no_drained_notice) {
            return;
        }
        let Some((last, claimed_ms)) = self.claimed_since_drained.take() else {
            return;
        };
        let repo = self.shared.options.repo.clone();
        // Mutant (`mutant_loop_drained_key_per_item`): the key names the item only, so a second
        // drain after the same item's claim is deduplicated away.
        let key = if cfg!(mutant_loop_drained_key_per_item) {
            format!("ready_drained:{repo}/{last}")
        } else {
            format!("ready_drained:{repo}/{last}/{claimed_ms}")
        };
        let notice = bosun_store::NewNotice {
            key,
            event: "ready_drained".to_owned(),
            repo,
            item: Some(last.to_string()),
            facts: std::collections::BTreeMap::from([("item".to_owned(), last.to_string())]),
            at_ms: now_ms(),
        };
        if let Err(e) = self
            .shared
            .deps
            .writer
            .write(vec![Write::NoticeQueued(notice)])
            .await
        {
            self.shared.trouble(format!(
                "the notice of the drained Ready column cannot be queued: {e}"
            ));
        }
    }

    /// Publishes `probing`, the cards of the repository that wait on a failing probe, and
    /// gives each one not stored yet its row, so `item.get` answers for it; `false` when the
    /// store refused (the loop halts).
    async fn wait_on_probes(
        &mut self,
        repo: &Repo,
        on_board: &[Card],
        stored: &BTreeMap<ItemId, StoredItem>,
        probing: Vec<crate::blockers::ProbeWait>,
    ) -> bool {
        let name = self.shared.options.repo.clone();
        let items: Vec<ItemId> = probing.iter().map(|w| w.item.clone()).collect();
        self.shared.deps.blockers.set_probing(&name, probing);
        for id in &items {
            if stored.contains_key(id) {
                continue;
            }
            let Some(card) = on_board.iter().find(|c| item_id(&name, c.id) == *id) else {
                continue;
            };
            if !self.register(repo, card, id).await {
                return false;
            }
        }
        true
    }

    /// Records the item of `card`; `false` when the store refused (the loop halts).
    async fn register(&mut self, repo: &Repo, card: &Card, id: &ItemId) -> bool {
        let write = Write::Item(NewItem {
            item: id.clone(),
            name: self.shared.options.repo.clone(),
            repo: repo.key.clone(),
            card_key: Some(card.id.to_string()),
            at_ms: now_ms(),
        });
        if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
            self.halt("the item's row", &e.to_string());
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
        let resources = lint_resources(repo, &self.shared.deps.leases.names());
        let repo = repo.clone();
        let on_board = on_board.clone();
        let lint =
            blocking(move || claim::static_lint(&card_owned, &repo, &on_board, &resources)).await;
        let linted = Linted {
            body: card.body.clone(),
            claimable: lint.claimable(),
            findings: lint.findings.iter().map(ToString::to_string).collect(),
            blockers: lint
                .contract
                .as_ref()
                .map_or_else(|| card.blocked_by.clone(), |c| card.blockers(c)),
            needs: lint
                .contract
                .as_ref()
                .map_or_else(Vec::new, |c| c.needs.clone()),
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
        // Worktree mode: the item runs off the daemon's clone, and the user may be working in
        // the checkout (E21); only stage 1 needs it clean.
        if repo.worktree.is_none() {
            let path = repo.path.clone();
            let dirty = blocking(move || bosun_git::clean_tree(&bosun_git::Git::new(path))).await;
            match dirty {
                Ok(reasons) if reasons.is_empty() => {}
                Ok(_) => {
                    return Err(
                        "the checkout has uncommitted changes; no item is claimed".to_owned()
                    );
                }
                Err(e) => return Err(format!("the checkout cannot be read: {e}")),
            }
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

/// The resource names the lint of `repo`'s cards admits in `needs` (#8): `configured` (the
/// lease manager's, read at daemon start) in worktree mode; none without
/// `repo.<name>.worktree` (E19), where a card that needs anything is refused as in stage 1.
pub(crate) fn lint_resources(repo: &Repo, configured: &BTreeSet<String>) -> BTreeSet<String> {
    if repo.worktree.is_some() {
        configured.clone()
    } else {
        BTreeSet::new()
    }
}

/// What the item of `claimed` leases (#8): its snapshot's `needs`, plus the shared build's
/// resource when `repo` has `worktree.shared_build` (E1). Nothing outside worktree mode (E19).
pub(crate) fn lease_names(claimed: &Claimed, repo: &Repo) -> Vec<String> {
    if claimed.worktree.is_none() {
        return Vec::new();
    }
    lease_names_of(&claimed.snapshot, repo)
}

/// What an item with `snapshot` leases in worktree mode (#8): its `needs`, plus the shared
/// build's resource when `repo` has `worktree.shared_build` (E1); sorted, without duplicates.
pub(crate) fn lease_names_of(snapshot: &bosun_queue::Snapshot, repo: &Repo) -> Vec<String> {
    lease_names_for(&snapshot.contract.needs, repo)
}

/// What an item whose contract `needs` these leases in worktree mode (#8): `needs`, plus the
/// shared build's resource when `repo` has `worktree.shared_build` (E1); sorted, without
/// duplicates.
pub(crate) fn lease_names_for(needs: &[String], repo: &Repo) -> Vec<String> {
    let mut names = needs.to_vec();
    if let Some(build) = repo.worktree.as_ref().and_then(|w| w.shared_build.as_ref()) {
        names.push(build.resource.clone());
    }
    names.sort();
    names.dedup();
    names
}

/// What the probes before the claim of a card whose contract `needs` these are of (#8): what it
/// would lease ([`lease_names_for`]); nothing outside worktree mode (E19).
pub(crate) fn probe_names(needs: &[String], repo: &Repo) -> Vec<String> {
    if repo.worktree.is_none() {
        return Vec::new();
    }
    lease_names_for(needs, repo)
}

/// Holds the leases of `item` (#8) before one of its processes starts: resolves at once when
/// it needs nothing or holds them already, else when they are granted. `Err(None)`: killed
/// while waiting, its request withdrawn; `Err(Some(_))`: the request was refused.
pub(crate) async fn hold_leases(
    shared: &Shared,
    repo: &Repo,
    claimed: &Claimed,
    item: &ItemId,
    kill: &mut watch::Receiver<Signal>,
) -> Result<(), Option<String>> {
    hold_names(shared, repo, lease_names(claimed, repo), item, kill).await
}

/// Holds `names` for `item` (#8), as [`hold_leases`] does: at once when `names` is empty (the
/// manager is not asked) or `item` holds exactly them already; `Err(None)` when killed while
/// waiting (the request withdrawn); `Err(Some(_))` when refused.
pub(crate) async fn hold_names(
    shared: &Shared,
    repo: &Repo,
    names: Vec<String>,
    item: &ItemId,
    kill: &mut watch::Receiver<Signal>,
) -> Result<(), Option<String>> {
    if names.is_empty() {
        return Ok(());
    }
    let request = crate::leases::Request {
        holder: crate::leases::Holder::Item(item.clone()),
        names,
        repo: shared.options.repo.clone(),
        identity: repo.identity.clone(),
        writer: Some(shared.deps.writer.clone()),
    };
    tokio::select! {
        held = shared.deps.leases.acquire(request) => held.map_err(Some),
        () = verdict::killed(kill) => Err(None),
    }
}

/// The guards of #59 that send an item to Needs you before its verdict, each with what it
/// found: protected paths the item's commits touched that its snapshot does not allow, new
/// ignored files outside the build directories since `ignored` (before the attempt's first
/// session), and changes to git's own configuration since the claim. Empty when none refuses.
fn guards(
    repo: &Repo,
    claimed: &Claimed,
    ignored: &bosun_git::Ignored,
) -> Result<Vec<(bosun_core::Guard, bosun_git::Refusal)>, bosun_git::Error> {
    use bosun_core::Guard;
    use bosun_git::Refusal;

    if cfg!(mutant_loop_skip_guards) {
        // Mutant: the loop skips the guards of #59 and goes on to the preconditions.
        return Ok(Vec::new());
    }
    let git = bosun_git::Git::new(&claimed.workdir);
    // Worktree mode: the item's own commits, not those that came in from the target (#8).
    let base = match &claimed.worktree {
        Some(wt) => git::item_base(&git, &claimed.base, &wt.base_ref)?,
        None => claimed.base.clone(),
    };
    let mut refusals = bosun_git::protected(
        &git,
        &base,
        |path| repo.policy.protects(path),
        &claimed.snapshot.contract.allow_protected,
    )?;
    refusals.extend(bosun_git::ignored(&git, ignored, &repo.build_dirs)?);
    refusals.extend(bosun_git::config(&git, &claimed.git_config)?);
    Ok(refusals
        .into_iter()
        .map(|refusal| {
            let guard = match &refusal {
                Refusal::Protected { .. } => Guard::ProtectedPath,
                Refusal::NewIgnored { .. } => Guard::IgnoredFile,
                // A change of git's configuration, and any refusal added later: Needs you all
                // the same.
                _ => Guard::GitConfig,
            };
            (guard, refusal)
        })
        .collect())
}
