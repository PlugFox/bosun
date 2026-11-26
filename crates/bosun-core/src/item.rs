//! The item machine of #7.
//!
//! ```text
//! ready -> claimed -> running -> verifying -> landing -> done
//!             |          |           |            +-> needs_you   (push or PR refused,
//!             |          |           |            |                second conflict)
//!             |          |           |            +-> landing     (rebased: verdict again)
//!             |          |           |            +-> running     (re-verdict failed, or
//!             |          |           |            |                first conflict: one session)
//!             |          |           +-> running        (verdict failed, attempts left)
//!             |          |           +-> needs_you      (attempts exhausted, guard)
//!             |          +-> parked -> running          (limit, pause, kill, daily cap)
//!             |          +-> needs_you                  (question, auth, budget,
//!             |                                          uncounted-park cap, config changed)
//!             +-> ready                                 (user won the claim race)
//! any daemon-owned state -> cancelled | ready | done    (user command, #5)
//! done (pass, flaky) -> needs_you                       (`pr` mode: its pull request was
//!                                                        closed unmerged, #8)
//! ```
//!
//! [`step`] is a pure function of the item, the event and a [`Ctx`] (the time and the active
//! hold are inputs, never read from a clock). Every arrow yields one [`Transition`]: an
//! idempotence key, the from and to states, the cause and the side effects as data, each with
//! its own key. The keys derive from the item and its transition count only, so replaying an
//! event from a recorded state yields the same keys, and the executor (#62) and the store (#57)
//! turn a replay into a no-op. Events address the attempt, session and verdict run they are
//! about; an event that is no longer current, or that has no arrow from the current state, is
//! ignored ([`step`] returns `None`), so applying an event twice adds nothing.
//!
//! Counting (#7): two failing verdict runs are one failed attempt; fail then pass is `flaky`
//! and passes; two failed attempts are Needs you. A limit, a pause, the kill switch, the daily
//! cap, a crash retry and a lost daemon do not count. The uncounted parks and resumes that the
//! agent can provoke (a limit, a crash, a failed resume, a lost session) are capped per item;
//! the cap is Needs you, so a printed fake limit cannot park an item forever.
//!
//! Landing (#8, E20) is one keyed [`EffectKind::Land`] and has no state of its own besides
//! [`State::Landing`]: the executor rebases the item branch onto the target's head and reports
//! [`Event::Rebased`], and the rebased commit is verified again ([`EffectKind::RunVerdict`])
//! before the target is fast-forwarded. A verdict that passed before the rebase is never
//! accepted as a verdict on the target branch; a failing re-verdict is a failed attempt. A
//! conflict gets one session of the same attempt with the conflict as its prompt; a second
//! conflict is Needs you. A failed worktree setup is Needs you and counts nothing. In the `pr`
//! landing mode an item is Done when its pull request is open; one closed unmerged later
//! ([`Event::PullRequestClosed`]) is Needs you, once.
//!
//! Invariant 2 of #1: an agent-originated event ([`Actor::Agent`]: a question, a summary, a
//! note) and the text of an exit can move an item to Needs you or nowhere. Only a verdict run
//! leads toward Done; the user may also put a card there by hand ([`Outcome::UserClosed`]).

use std::fmt;

use crate::attempt::{Attempt, AttemptEvent, AttemptState, RunResult};

/// The identity of an item: its key in the store, unique across repositories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub String);

impl ItemId {
    /// An item id from its key.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A daemon-owned board column (#5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    /// In progress: claimed, running or parked.
    InProgress,
    /// Verify: verifying or landing.
    Verify,
    /// Done.
    Done,
    /// Needs you; the reason is an issue comment.
    NeedsYou,
}

/// A gate that holds new work back: no session starts while one is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hold {
    /// A global or per-repository pause: the current attempt finishes, nothing new starts.
    Pause,
    /// The kill switch: every child is stopped and every item parks.
    Kill,
    /// The daily eqv cap is reached; the queue parks until local midnight.
    DailyCap,
}

/// Why an item is parked. None of these counts as a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParkReason {
    /// The provider's limit; resumes at the reset time.
    Limit,
    /// A pause was active when a session would have started.
    Paused,
    /// The kill switch.
    Killed,
    /// The daily cap.
    DailyCap,
}

impl From<Hold> for ParkReason {
    fn from(hold: Hold) -> Self {
        match hold {
            Hold::Pause => Self::Paused,
            Hold::Kill => Self::Killed,
            Hold::DailyCap => Self::DailyCap,
        }
    }
}

/// What a parked item does when it wakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Next {
    /// Start a new attempt.
    NewAttempt,
    /// Resume the attempt's last session by id.
    Resume,
    /// Reset the working tree to the last commit and start a fresh session of the attempt.
    Reset,
    /// Go on verifying the attempt.
    Verify,
}

/// A parked item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Park {
    /// Why it parked.
    pub reason: ParkReason,
    /// The earliest wake time (ms since the Unix epoch); `None` wakes when no hold is active.
    pub until_ms: Option<i64>,
    /// What it does on waking.
    pub next: Next,
}

/// How an item reached Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The verdict passed.
    Pass,
    /// The verdict failed once and passed on its re-run.
    Flaky,
    /// The user moved the card into Done by hand; no verdict (#5).
    UserClosed,
}

/// A check before the verdict whose failure is Needs you rather than a failed attempt (#7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Guard {
    /// The item's commits touch a protected path.
    ProtectedPath,
    /// A new ignored file outside the declared build directories.
    IgnoredFile,
    /// Git's own configuration (hooks, config, excludes, attributes) changed.
    GitConfig,
    /// The user's checkout (HEAD, refs, status) captured at claim changed during the item: a
    /// switched or detached HEAD, a ref deleted or moved to a non-descendant (#8, E4 and E21).
    Checkout,
}

/// Why an item needs the user. Each is posted as a new comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The card failed lint.
    Lint,
    /// The card is in a `blocked-by` cycle.
    Cycle,
    /// The agent asked a blocking question.
    Question,
    /// The provider's authentication expired; retrying cannot help.
    Auth,
    /// A per-item eqv or wall-clock cap was exceeded.
    Budget,
    /// The uncounted parks and resumes reached their per-item cap.
    ParkCap,
    /// The configuration or the permission policy changed during the item.
    ConfigChanged,
    /// The failed attempts reached the limit.
    AttemptsExhausted,
    /// A guard before the verdict refused the attempt.
    Guard(Guard),
    /// Landing was refused (push or pull request refused, a second conflict).
    LandingRefused,
    /// The worktree's setup failed (#8): not an attempt failure.
    Setup,
    /// The session's MCP server (`bosun-mcp`) did not say hello in time, again after one
    /// uncounted retry (#64): the agent would run without its tools.
    Handshake,
    /// The identity checks before a session refused (#2, rule 2; #99): the remote's owner,
    /// the token's login or the resolved author is not the identity's. Retrying cannot help.
    Identity,
    /// The daemon's view of the item was older than its stored history; a transition computed
    /// from it was refused (#132).
    Stale,
}

/// The state of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// In Ready, not claimed: user-owned.
    Ready,
    /// The claim is being written and re-read; the snapshot is taken.
    Claimed,
    /// A session of the current attempt is running.
    Running,
    /// Parked; does not count.
    Parked(Park),
    /// The verdict (its preconditions, then its runs) is in progress.
    Verifying,
    /// The landing step is in progress.
    Landing,
    /// Done.
    Done(Outcome),
    /// Needs the user.
    NeedsYou(Reason),
    /// Cancelled by the user.
    Cancelled,
}

impl State {
    /// The stable name stored in the `item` and `transition` tables (#3).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Parked(_) => "parked",
            Self::Verifying => "verifying",
            Self::Landing => "landing",
            Self::Done(_) => "done",
            Self::NeedsYou(_) => "needs_you",
            Self::Cancelled => "cancelled",
        }
    }

    /// The board column the daemon projects this state to; `None` for the user's states.
    #[must_use]
    pub fn column(self) -> Option<Column> {
        match self {
            Self::Ready | Self::Cancelled => None,
            Self::Claimed | Self::Running | Self::Parked(_) => Some(Column::InProgress),
            Self::Verifying | Self::Landing => Some(Column::Verify),
            Self::Done(_) => Some(Column::Done),
            Self::NeedsYou(_) => Some(Column::NeedsYou),
        }
    }

    /// The daemon owns the item and has work in flight: claimed, running, parked, verifying or
    /// landing. These are the states crash recovery visits.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Claimed | Self::Running | Self::Parked(_) | Self::Verifying | Self::Landing
        )
    }

    /// A child process (a session or a verdict run) may be alive in this state. In
    /// [`State::Landing`] it depends on the attempt: see `Draft::has_child`.
    fn has_child(self) -> bool {
        matches!(self, Self::Running | Self::Verifying)
    }
}

/// How a session ended, as the runner classified it (#7, `classify_exit`). A blocking question
/// is not an exit kind: it is [`Event::Question`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exit {
    /// A normal exit; the verdict decides.
    Ok,
    /// A provider limit, with the reset time if one was parsed.
    Limit {
        /// The reset time, ms since the Unix epoch.
        reset_ms: Option<i64>,
    },
    /// Authentication expired.
    Auth,
    /// The session died (a crash, a dead child after sleep). A missing MCP handshake is
    /// [`Event::NoHandshake`].
    Crash,
}

/// A user action on a card (#5): a command, not a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserCommand {
    /// The card left the board or went to Backlog, or the issue was closed: cancel.
    Cancel,
    /// The card is in Ready again: start over when its turn comes, with a fresh attempt counter.
    Requeue,
    /// The `restart` label: kill the current session, re-snapshot, start again.
    Restart,
    /// The card was moved into Done by hand: accepted, recorded as user-closed, no verdict.
    ManualDone,
}

/// Who an event comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Actor {
    /// The daemon's own observation: the board, a process, the verdict, the clock.
    Daemon,
    /// The agent, through its MCP calls. May move an item to Needs you only (#1, invariant 2).
    Agent,
    /// The user, through the board.
    User,
}

/// Something that happened to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The scheduler picked the item; the claim procedure starts.
    Claim,
    /// The claim was written, re-read, linted and snapshotted.
    ClaimConfirmed,
    /// The re-read showed a user move in between: the user wins and the claim is released.
    ClaimLost,
    /// The card failed lint.
    LintFailed,
    /// The card is in a `blocked-by` cycle.
    Cycle,
    /// A session exited.
    Exited {
        /// The attempt it belonged to.
        attempt: u32,
        /// The session's number.
        spawn: u32,
        /// The runner's classification.
        exit: Exit,
        /// The session's final text. The agent writes it, so the machine never reads it.
        text: String,
    },
    /// Resuming a session by id failed.
    ResumeFailed {
        /// The attempt.
        attempt: u32,
        /// The session that failed to resume.
        spawn: u32,
    },
    /// The repository's fast-death breaker tripped on this session (#7): it was the last of
    /// several sessions in a row that died by themselves soon after their spawn, across items.
    /// The repository is paused; the item parks paused and charges nothing for this session
    /// (no crash retry, no uncounted resume, no failed attempt). After the pause it starts a
    /// fresh session of the same attempt from its last commit.
    FastDeath {
        /// The attempt it belonged to.
        attempt: u32,
        /// The session's number.
        spawn: u32,
    },
    /// The session's MCP server did not say hello within the handshake timeout after spawn
    /// (#64); the daemon has killed the session. Retried once, uncounted toward the attempts;
    /// a second miss is Needs you.
    NoHandshake {
        /// The attempt it belonged to.
        attempt: u32,
        /// The session's number.
        spawn: u32,
    },
    /// The fail-closed identity checks refused the session before its spawn (#2, rule 2;
    /// #99): nothing ran. Needs you at once, not a failed attempt.
    IdentityRefused {
        /// The attempt it belonged to.
        attempt: u32,
        /// The session's number.
        spawn: u32,
    },
    /// A blocking `ask_question` call (agent).
    Question {
        /// The attempt of the session that asked.
        attempt: u32,
    },
    /// A `submit_summary` call (agent). Recorded with its attempt; moves nothing.
    Summary {
        /// The attempt.
        attempt: u32,
    },
    /// A `note` call (agent). Recorded with its attempt; moves nothing.
    Note {
        /// The attempt.
        attempt: u32,
    },
    /// The verdict's preconditions were checked.
    Preconditions {
        /// The attempt.
        attempt: u32,
        /// All held; `false` fails the attempt.
        passed: bool,
    },
    /// A guard refused the attempt before its verdict.
    Refused {
        /// The attempt.
        attempt: u32,
        /// Which guard.
        guard: Guard,
    },
    /// A verdict run finished.
    Verdict {
        /// The attempt.
        attempt: u32,
        /// The run's number.
        run: u32,
        /// Its result.
        result: RunResult,
    },
    /// The landing step finished.
    Landed,
    /// The landing step was refused.
    LandingRefused,
    /// The landing step rebased the item branch onto the target's head; the rebased commit is
    /// verified again before it lands.
    Rebased {
        /// The attempt being landed.
        attempt: u32,
    },
    /// The rebase onto the target's head conflicted.
    Conflict {
        /// The attempt being landed.
        attempt: u32,
        /// The attempt's verdict run whose pass emitted the `Land` this landing pass answers;
        /// a conflict of an earlier pass names an older run and is ignored.
        run: u32,
    },
    /// The worktree's setup (its creation or its setup command) failed.
    SetupFailed,
    /// The clock: a parked item wakes once its time has come and no hold is active.
    Tick,
    /// The kill switch was thrown.
    KillSwitch,
    /// A per-item budget cap was exceeded.
    BudgetExceeded,
    /// The configuration or the policy hash changed during the item.
    ConfigChanged,
    /// The store refused a transition computed from the daemon's view of the item as stale: the
    /// view was older than the item's stored history (#132). Stepped from the stored item.
    Stale,
    /// The daemon restarted; the work in flight in the current state is gone (crash recovery).
    Restarted {
        /// The daemon's boot number, increasing across starts.
        boot: u32,
        /// For a running item, the exit classified from the transcript tail, if any.
        tail: Option<Exit>,
    },
    /// The daemon restarted and re-attached the running item's session, which went on under its
    /// shim (#8, E3): nothing was lost, so nothing is spawned or counted; only the boot is
    /// recorded.
    Reattached {
        /// The daemon's boot number, increasing across starts.
        boot: u32,
    },
    /// The pull request of an item Done in the `pr` landing mode was closed without being
    /// merged (#8): the daemon saw it when it polled the forge. Needs you, from a Done item
    /// whose verdict passed only.
    PullRequestClosed,
    /// A user command.
    User(UserCommand),
}

impl Event {
    /// The stable name recorded as the transition's cause.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::ClaimConfirmed => "claim_confirmed",
            Self::ClaimLost => "claim_lost",
            Self::LintFailed => "lint_failed",
            Self::Cycle => "cycle",
            Self::Exited { .. } => "exited",
            Self::ResumeFailed { .. } => "resume_failed",
            Self::FastDeath { .. } => "fast_death",
            Self::NoHandshake { .. } => "no_handshake",
            Self::IdentityRefused { .. } => "identity_refused",
            Self::Question { .. } => "question",
            Self::Summary { .. } => "summary",
            Self::Note { .. } => "note",
            Self::Preconditions { .. } => "preconditions",
            Self::Refused { .. } => "refused",
            Self::Verdict { .. } => "verdict",
            Self::Landed => "landed",
            Self::LandingRefused => "landing_refused",
            Self::Rebased { .. } => "rebased",
            Self::Conflict { .. } => "conflict",
            Self::SetupFailed => "setup_failed",
            Self::Tick => "tick",
            Self::KillSwitch => "kill_switch",
            Self::BudgetExceeded => "budget_exceeded",
            Self::ConfigChanged => "config_changed",
            Self::Stale => "stale",
            Self::Restarted { .. } => "restarted",
            Self::Reattached { .. } => "reattached",
            Self::PullRequestClosed => "pr_closed",
            Self::User(UserCommand::Cancel) => "user_cancel",
            Self::User(UserCommand::Requeue) => "user_requeue",
            Self::User(UserCommand::Restart) => "user_restart",
            Self::User(UserCommand::ManualDone) => "user_done",
        }
    }

    /// Who the event comes from.
    #[must_use]
    pub fn actor(&self) -> Actor {
        match self {
            Self::Question { .. } | Self::Summary { .. } | Self::Note { .. } => Actor::Agent,
            Self::User(_) => Actor::User,
            _ => Actor::Daemon,
        }
    }
}

/// The counting rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    /// Failed attempts that send the item to Needs you.
    pub max_attempts: u32,
    /// Uncounted parks and resumes allowed per item; one more is Needs you.
    pub park_cap: u32,
    /// Crash retries per attempt; one more crash fails the attempt.
    pub crash_retries: u32,
    /// How long a limit parks when no reset time was parsed, in ms.
    pub default_backoff_ms: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            park_cap: 6,
            crash_retries: 1,
            default_backoff_ms: 15 * 60 * 1000,
        }
    }
}

/// The inputs of a step besides the item and the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
    /// The time, ms since the Unix epoch.
    pub now_ms: i64,
    /// The hold active for the item's repository, if any (see [`crate::Gates::hold`]).
    pub hold: Option<Hold>,
    /// The counting rules.
    pub rules: Rules,
}

impl Ctx {
    /// A context at `now_ms` with no hold and the default rules.
    #[must_use]
    pub fn at(now_ms: i64) -> Self {
        Self {
            now_ms,
            hold: None,
            rules: Rules::default(),
        }
    }
}

/// An item and its counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The item's id.
    pub id: ItemId,
    /// Its state.
    pub state: State,
    /// The current (or last) attempt.
    pub attempt: Option<Attempt>,
    /// Transitions recorded so far; the next one is `seq + 1`.
    pub seq: u32,
    /// Attempt numbers issued (item-wide, never reset).
    pub attempts: u32,
    /// Session numbers issued (item-wide, never reset).
    pub spawns: u32,
    /// Verdict run numbers issued (item-wide, never reset).
    pub runs: u32,
    /// Failed attempts since the counter was last reset.
    pub failed: u32,
    /// Uncounted parks and resumes since the counter was last reset.
    pub uncounted: u32,
    /// Conflict sessions since the counters were last reset.
    pub conflicts: u32,
    /// The last daemon boot whose recovery this item went through.
    pub boot: u32,
}

impl Item {
    /// A new item in Ready.
    #[must_use]
    pub fn new(id: ItemId) -> Self {
        Self {
            id,
            state: State::Ready,
            attempt: None,
            seq: 0,
            attempts: 0,
            spawns: 0,
            runs: 0,
            failed: 0,
            uncounted: 0,
            conflicts: 0,
            boot: 0,
        }
    }

    fn attempt_number(&self) -> u32 {
        self.attempt.as_ref().map_or(0, |a| a.number)
    }
}

/// The idempotence key of a transition: the item and its transition number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransitionKey {
    /// The item.
    pub item: ItemId,
    /// The transition's number within the item, from 1.
    pub seq: u32,
}

impl fmt::Display for TransitionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/t{}", self.item, self.seq)
    }
}

/// The key of a side effect: the item, the attempt, the transition and the effect's index in
/// it. Redoing an effect with the same key after a crash is a no-op.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectKey {
    /// The item.
    pub item: ItemId,
    /// The attempt current after the transition (0 before the first).
    pub attempt: u32,
    /// The transition's number.
    pub seq: u32,
    /// The effect's index within the transition.
    pub index: u32,
}

impl fmt::Display for EffectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/a{}/t{}/e{}",
            self.item, self.attempt, self.seq, self.index
        )
    }
}

/// How a session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpawnMode {
    /// A fresh session of a new attempt.
    Fresh,
    /// Resume the attempt's last session by id.
    Resume,
    /// Reset the working tree to the last commit, then a fresh session of the same attempt.
    Reset,
}

/// A side effect, as data; the executor (#62) performs it after the transition is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    /// The claim procedure (#5, #62): fail-closed checks, snapshot, lint, hashes, base capture,
    /// write the In progress status, re-read; it removes a `restart` label if present. Reports
    /// [`Event::ClaimConfirmed`], [`Event::ClaimLost`] or [`Event::LintFailed`].
    Claim,
    /// Start a session.
    Spawn {
        /// The attempt.
        attempt: u32,
        /// The new session's number.
        spawn: u32,
        /// How.
        mode: SpawnMode,
    },
    /// Kill the item's child (session or verdict run) now; a question's session after a grace.
    Kill,
    /// Check the verdict's preconditions and guards.
    CheckPreconditions {
        /// The attempt.
        attempt: u32,
    },
    /// Run the verdict from the snapshot.
    RunVerdict {
        /// The attempt.
        attempt: u32,
        /// The run's number.
        run: u32,
    },
    /// The landing step (stage 1: `local`, trailers and the item report).
    Land {
        /// The attempt whose verdict passed.
        attempt: u32,
    },
    /// Write a daemon-owned column to the board.
    Project(Column),
    /// Post a new comment for the user: the reason (a question carries its text).
    Comment(Reason),
    /// Create or edit the sticky item report; coalesces per [`Effect::coalesce_key`].
    Report,
}

/// A keyed side effect.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Effect {
    /// Its key.
    pub key: EffectKey,
    /// What to do.
    pub kind: EffectKind,
}

impl Effect {
    /// The outbox key under which pending rows of this effect coalesce, if they do: the sticky
    /// report is one per item and is rendered from the database at send time (#5).
    #[must_use]
    pub fn coalesce_key(&self) -> Option<String> {
        match self.kind {
            EffectKind::Report => Some(format!("{}/report", self.key.item)),
            _ => None,
        }
    }
}

/// One arrow taken: recorded before any of its effects is performed (#1, invariant 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// Its idempotence key.
    pub key: TransitionKey,
    /// The attempt current after the transition (0 before the first).
    pub attempt: u32,
    /// The item's state before.
    pub from: State,
    /// The item's state after.
    pub to: State,
    /// The attempt's state before, if there was an attempt.
    pub attempt_from: Option<AttemptState>,
    /// The attempt's state after, if there is an attempt.
    pub attempt_to: Option<AttemptState>,
    /// The attempt this transition ended and replaced with a new one, in its final state. When
    /// a failed attempt is followed at once by the next one, `attempt_from` and `attempt_to`
    /// describe the new attempt's arrow, and this is the only record of how the old one ended.
    pub ended: Option<Attempt>,
    /// The event's name ([`Event::name`]).
    pub cause: &'static str,
    /// The side effects, in order.
    pub effects: Vec<Effect>,
}

/// The result of a step that took an arrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stepped {
    /// The item after the transition.
    pub item: Item,
    /// The transition to record.
    pub transition: Transition,
}

/// Applies `event` to `item`. `None` when the event is stale or has no arrow from the item's
/// state: nothing is recorded and nothing is done.
#[must_use]
pub fn step(item: &Item, event: &Event, ctx: &Ctx) -> Option<Stepped> {
    let mut draft = Draft {
        item: item.clone(),
        effects: Vec::new(),
        ended: None,
        ctx: *ctx,
    };
    draft.apply(event)?;
    Some(draft.finish(item, event))
}

/// The item being changed by one step, and the effects so far.
struct Draft {
    item: Item,
    effects: Vec<EffectKind>,
    /// The attempt a new attempt replaced in this step, in its final state.
    ended: Option<Attempt>,
    ctx: Ctx,
}

impl Draft {
    fn apply(&mut self, event: &Event) -> Option<()> {
        match event {
            Event::Claim => {
                self.expect(&[State::Ready])?;
                self.item.state = State::Claimed;
                self.effects.push(EffectKind::Claim);
            }
            Event::ClaimConfirmed => {
                self.expect(&[State::Claimed])?;
                self.start(Next::NewAttempt);
            }
            Event::ClaimLost => {
                self.expect(&[State::Claimed])?;
                self.item.state = State::Ready;
            }
            Event::LintFailed => {
                self.expect(&[State::Ready, State::Claimed])?;
                self.needs_you(Reason::Lint);
            }
            Event::Cycle => {
                self.expect(&[State::Ready])?;
                self.needs_you(Reason::Cycle);
            }
            Event::Exited {
                attempt,
                spawn,
                exit,
                text,
            } => {
                self.expect(&[State::Running])?;
                self.session(*attempt, *spawn)?;
                self.exited(*exit, text);
            }
            Event::ResumeFailed { attempt, spawn } => {
                self.expect(&[State::Running])?;
                self.session(*attempt, *spawn)?;
                if !self.uncounted() {
                    self.start(Next::Reset);
                }
            }
            Event::FastDeath { attempt, spawn } => self.fast_death(*attempt, *spawn)?,
            Event::NoHandshake { attempt, spawn } => {
                self.expect(&[State::Running])?;
                self.session(*attempt, *spawn)?;
                self.no_handshake();
            }
            Event::IdentityRefused { attempt, spawn } => {
                self.expect(&[State::Running])?;
                self.session(*attempt, *spawn)?;
                self.attempt_mut().state = AttemptState::Exited;
                self.needs_you(Reason::Identity);
            }
            Event::Question { attempt } => {
                self.expect(&[State::Running, State::Verifying])?;
                self.current(*attempt)?;
                self.kill_child();
                self.needs_you(Reason::Question);
            }
            // Recorded as the attempt's agent calls (#57), never a transition.
            Event::Summary { .. } | Event::Note { .. } => return None,
            Event::Preconditions { .. } | Event::Refused { .. } | Event::Verdict { .. } => {
                self.verification(event)?;
            }
            Event::Landed
            | Event::LandingRefused
            | Event::Rebased { .. }
            | Event::Conflict { .. } => self.landing(event)?,
            Event::SetupFailed => {
                self.expect(&[
                    State::Claimed,
                    State::Running,
                    State::Verifying,
                    State::Landing,
                ])?;
                self.kill_child();
                self.needs_you(Reason::Setup);
            }
            Event::Tick => self.tick()?,
            Event::KillSwitch => self.kill_switch()?,
            Event::BudgetExceeded => {
                self.expect(&[State::Running, State::Parked(PARKED), State::Verifying])?;
                self.kill_child();
                self.needs_you(Reason::Budget);
            }
            Event::ConfigChanged => {
                self.expect(&[
                    State::Claimed,
                    State::Running,
                    State::Parked(PARKED),
                    State::Verifying,
                ])?;
                self.kill_child();
                self.needs_you(Reason::ConfigChanged);
            }
            Event::Stale => self.stale()?,
            Event::Restarted { boot, tail } => self.restarted(*boot, *tail)?,
            Event::Reattached { boot } => self.reattached(*boot)?,
            Event::PullRequestClosed => self.pr_closed()?,
            Event::User(command) => self.user(*command)?,
        }
        Some(())
    }

    /// The store refused a transition computed from a stale view of the item (#132): from any
    /// state the daemon is not done with, the child is killed and the item needs the user.
    fn stale(&mut self) -> Option<()> {
        self.expect(&[
            State::Ready,
            State::Claimed,
            State::Running,
            State::Parked(PARKED),
            State::Verifying,
            State::Landing,
        ])?;
        self.kill_child();
        self.needs_you(Reason::Stale);
        Some(())
    }

    /// The verification events: preconditions, guards and verdict runs.
    fn verification(&mut self, event: &Event) -> Option<()> {
        match event {
            Event::Preconditions { attempt, passed } => {
                self.expect(&[State::Verifying])?;
                self.current(*attempt)?;
                if self.item.attempt.as_ref()?.state != AttemptState::VerdictPending {
                    return None;
                }
                if *passed {
                    self.attempt_step(AttemptEvent::VerdictStarted)?;
                    self.run_verdict();
                } else {
                    self.attempt_step(AttemptEvent::Failed)?;
                    self.attempt_failed();
                }
            }
            Event::Refused { attempt, guard } => {
                self.expect(&[State::Verifying])?;
                self.current(*attempt)?;
                self.needs_you(Reason::Guard(*guard));
            }
            Event::Verdict {
                attempt,
                run,
                result,
            } => {
                // In Landing, the verdict of the rebased commit (#8).
                self.expect(&[State::Verifying, State::Landing])?;
                self.current(*attempt)?;
                if self.item.attempt.as_ref()?.run != *run {
                    return None;
                }
                let landing = self.item.state == State::Landing;
                self.attempt_step(AttemptEvent::VerdictFinished(*result))?;
                let failed = self.item.attempt.as_ref().is_some_and(|a| a.state.failed());
                if landing && failed && cfg!(mutant_core_landing_failure_uncounted) {
                    // Mutant (`mutant_core_landing_failure_uncounted`): the failed re-verdict
                    // starts the next attempt without counting the failed one.
                    self.start(Next::NewAttempt);
                } else {
                    self.verdict_finished();
                }
            }
            _ => return None,
        }
        Some(())
    }

    /// The landing events (#8): the rebase onto the target's head, its conflict, the end.
    fn landing(&mut self, event: &Event) -> Option<()> {
        match event {
            Event::Landed => {
                self.expect(&[State::Landing])?;
                // Only a verdict that passed leads on: not a rebased commit still verifying.
                let outcome = match self.item.attempt.as_ref()?.state {
                    AttemptState::Pass => Outcome::Pass,
                    AttemptState::Flaky => Outcome::Flaky,
                    _ => return None,
                };
                self.item.state = State::Done(outcome);
            }
            Event::LandingRefused => {
                self.expect(&[State::Landing])?;
                self.needs_you(Reason::LandingRefused);
            }
            Event::Rebased { attempt } => {
                self.expect(&[State::Landing])?;
                self.current(*attempt)?;
                if cfg!(mutant_core_rebased_lands_unverified) {
                    // Mutant (`mutant_core_rebased_lands_unverified`): the rebased commit lands
                    // on the verdict that passed before the rebase.
                    if !self.item.attempt.as_ref()?.state.passed() {
                        return None;
                    }
                    self.effects.push(EffectKind::Land { attempt: *attempt });
                    return Some(());
                }
                // A verdict that passed before the rebase is never a verdict on the target.
                self.attempt_step(AttemptEvent::Reverify)?;
                if cfg!(mutant_core_rebased_ignores_kill_hold) {
                    // Mutant (`mutant_core_rebased_ignores_kill_hold`): the re-verdict starts
                    // after the kill switch was thrown.
                    self.run_verdict();
                } else {
                    self.reverify();
                }
            }
            Event::Conflict { attempt, run } => {
                self.expect(&[State::Landing])?;
                self.current(*attempt)?;
                // Only a conflict of the current landing pass: the one the pass of run `run`
                // started. After a conflict session the attempt verifies under a new run.
                let a = self.item.attempt.as_ref()?;
                // Mutant (`mutant_core_conflict_unkeyed`): the run is not compared, so a late
                // conflict of an earlier pass counts as the second one.
                if !a.state.passed() || (a.run != *run && !cfg!(mutant_core_conflict_unkeyed)) {
                    return None;
                }
                // Mutant (`mutant_core_conflict_sessions_unbounded`): every conflict starts
                // another session.
                if self.item.conflicts == 0 || cfg!(mutant_core_conflict_sessions_unbounded) {
                    self.item.conflicts = 1;
                    // The session's commits get a verdict of their own, re-run rule included.
                    self.attempt_mut().first_run = None;
                    self.start(Next::Reset);
                } else {
                    self.needs_you(Reason::LandingRefused);
                }
            }
            _ => return None,
        }
        Some(())
    }

    /// After Done in the `pr` landing mode (#8): the item's pull request was closed unmerged.
    fn pr_closed(&mut self) -> Option<()> {
        // Mutant (`mutant_core_pr_closed_from_anywhere`): any state takes the arrow.
        if !cfg!(mutant_core_pr_closed_from_anywhere) {
            // Not a card the user closed by hand: its pull request is not the daemon's.
            self.expect(&[State::Done(Outcome::Pass), State::Done(Outcome::Flaky)])?;
        }
        self.needs_you(Reason::LandingRefused);
        Some(())
    }

    /// Succeeds when the item is in one of `states`; any parked state matches `PARKED`.
    fn expect(&self, states: &[State]) -> Option<()> {
        let matches = |s: &State| match (s, self.item.state) {
            (State::Parked(_), State::Parked(_)) => true,
            (s, current) => *s == current,
        };
        states.iter().any(matches).then_some(())
    }

    /// Succeeds when `attempt` is the current attempt.
    fn current(&self, attempt: u32) -> Option<()> {
        (self.item.attempt.as_ref()?.number == attempt).then_some(())
    }

    /// Succeeds when `attempt` and `spawn` name the current session.
    fn session(&self, attempt: u32, spawn: u32) -> Option<()> {
        let a = self.item.attempt.as_ref()?;
        (a.number == attempt && a.spawn == spawn).then_some(())
    }

    fn attempt_step(&mut self, event: AttemptEvent) -> Option<()> {
        let next = self.item.attempt.as_ref()?.step(event)?;
        self.item.attempt = Some(next);
        Some(())
    }

    fn attempt_mut(&mut self) -> &mut Attempt {
        self.item
            .attempt
            .as_mut()
            .expect("an item past its claim has an attempt")
    }

    fn exited(&mut self, exit: Exit, text: &str) {
        // Invariant 2 of #1: the text of an exit is the agent's and never leads toward Done.
        let trusted_done = cfg!(mutant_core_trust_done) && text.to_lowercase().contains("done");
        match exit {
            Exit::Ok if trusted_done => {
                self.attempt_mut().state = AttemptState::Pass;
                self.item.state = State::Landing;
                let attempt = self.item.attempt_number();
                self.effects.push(EffectKind::Land { attempt });
            }
            Exit::Ok => {
                self.attempt_mut().state = AttemptState::Exited;
                self.start(Next::Verify);
            }
            Exit::Limit { reset_ms } => {
                if cfg!(mutant_core_limit_is_failure) {
                    self.attempt_mut().state = AttemptState::Fail;
                    self.attempt_failed();
                    return;
                }
                self.attempt_mut().state = AttemptState::Limited;
                if self.uncounted() {
                    return;
                }
                let until = reset_ms
                    .unwrap_or(self.ctx.now_ms + self.ctx.rules.default_backoff_ms)
                    .max(self.ctx.now_ms);
                self.item.state = State::Parked(Park {
                    reason: ParkReason::Limit,
                    until_ms: Some(until),
                    next: Next::Resume,
                });
            }
            Exit::Auth => {
                self.attempt_mut().state = AttemptState::Exited;
                self.needs_you(Reason::Auth);
            }
            Exit::Crash => {
                let retries = self.ctx.rules.crash_retries;
                let attempt = self.attempt_mut();
                attempt.state = AttemptState::Exited;
                if attempt.crash_retries < retries {
                    attempt.crash_retries += 1;
                    if !self.uncounted() {
                        self.start(Next::Resume);
                    }
                } else {
                    attempt.state = AttemptState::Fail;
                    self.attempt_failed();
                }
            }
        }
    }

    /// The session that tripped the repository's fast-death breaker (#7): parked paused
    /// whatever the hold (the breaker has just paused the repository, and the session is gone
    /// already), charged nothing, to start a fresh session of the same attempt after the pause.
    fn fast_death(&mut self, attempt: u32, spawn: u32) -> Option<()> {
        self.expect(&[State::Running])?;
        self.session(attempt, spawn)?;
        if cfg!(mutant_core_fast_death_charges_a_crash) {
            // Mutant (`mutant_core_fast_death_charges_a_crash`): the tripping session is charged
            // as any crash.
            self.exited(Exit::Crash, "");
            return Some(());
        }
        self.attempt_mut().state = AttemptState::Exited;
        self.item.state = State::Parked(Park {
            reason: ParkReason::Paused,
            until_ms: None,
            next: Next::Reset,
        });
        Some(())
    }

    /// A session whose MCP server never said hello (#64). It shares the attempt's crash retry:
    /// the first miss resumes uncounted, the next one is Needs you, not a failed attempt,
    /// because another attempt would start the same server again.
    fn no_handshake(&mut self) {
        let retries = self.ctx.rules.crash_retries;
        let attempt = self.attempt_mut();
        attempt.state = AttemptState::Exited;
        if attempt.crash_retries < retries {
            attempt.crash_retries += 1;
            if !self.uncounted() {
                self.start(Next::Resume);
            }
        } else {
            self.needs_you(Reason::Handshake);
        }
    }

    /// Counts one uncounted park or resume. `true` when it exceeds the cap; the item then
    /// needs the user.
    fn uncounted(&mut self) -> bool {
        self.item.uncounted += 1;
        let over = self.item.uncounted > self.ctx.rules.park_cap;
        if over && !cfg!(mutant_core_no_park_cap) {
            self.needs_you(Reason::ParkCap);
            return true;
        }
        false
    }

    /// The current attempt has failed (its state is already `fail` or `timeout`).
    fn attempt_failed(&mut self) {
        self.item.failed += 1;
        if self.item.failed >= self.ctx.rules.max_attempts {
            self.needs_you(Reason::AttemptsExhausted);
        } else {
            self.start(Next::NewAttempt);
        }
    }

    fn verdict_finished(&mut self) {
        let state = self.item.attempt.as_ref().map(|a| a.state);
        match state {
            Some(s) if s.passed() => {
                self.item.state = State::Landing;
                let attempt = self.item.attempt_number();
                self.effects.push(EffectKind::Land { attempt });
            }
            Some(s) if s.failed() => self.attempt_failed(),
            // A non-passing first run: re-run once, with no session in between.
            _ => {
                self.attempt_mut().state = AttemptState::VerdictRunning;
                self.run_verdict();
            }
        }
    }

    /// The verdict of a rebased commit in [`State::Landing`] (#8): it runs at once, or the item
    /// parks to verify when the kill switch holds. A pause and the daily cap hold sessions only.
    fn reverify(&mut self) {
        if self.ctx.hold == Some(Hold::Kill) {
            self.item.state = State::Parked(Park {
                reason: ParkReason::Killed,
                until_ms: None,
                next: Next::Verify,
            });
        } else {
            self.run_verdict();
        }
    }

    fn run_verdict(&mut self) {
        self.item.runs += 1;
        let run = self.item.runs;
        let attempt = self.attempt_mut();
        attempt.run = run;
        let attempt = attempt.number;
        self.effects.push(EffectKind::RunVerdict { attempt, run });
    }

    /// Starts `next`, or parks with it when a hold is active. A pause and the daily cap hold
    /// sessions only: the current attempt still finishes its verdict.
    fn start(&mut self, next: Next) {
        let held = match (self.ctx.hold, next) {
            (Some(Hold::Kill), _) => Some(Hold::Kill),
            (_, Next::Verify) => None,
            (hold, _) => hold,
        };
        if let Some(hold) = held {
            self.item.state = State::Parked(Park {
                reason: hold.into(),
                until_ms: None,
                next,
            });
            return;
        }
        match next {
            Next::NewAttempt => {
                // A requeue or a restart has already dropped the old attempt.
                self.ended = self.item.attempt.take();
                self.item.attempts += 1;
                self.item.spawns += 1;
                let (attempt, spawn) = (self.item.attempts, self.item.spawns);
                self.item.attempt = Some(Attempt::new(attempt, spawn));
                self.item.state = State::Running;
                self.effects.push(EffectKind::Spawn {
                    attempt,
                    spawn,
                    mode: SpawnMode::Fresh,
                });
            }
            Next::Resume | Next::Reset => {
                self.item.spawns += 1;
                let spawn = self.item.spawns;
                let attempt = self.attempt_mut();
                attempt.spawn = spawn;
                attempt.state = AttemptState::Spawned;
                let attempt = attempt.number;
                let mode = if next == Next::Resume {
                    SpawnMode::Resume
                } else {
                    SpawnMode::Reset
                };
                self.item.state = State::Running;
                self.effects.push(EffectKind::Spawn {
                    attempt,
                    spawn,
                    mode,
                });
            }
            Next::Verify => {
                self.item.state = State::Verifying;
                let attempt = self.attempt_mut();
                if attempt.state == AttemptState::Exited {
                    attempt.state = AttemptState::VerdictPending;
                }
                if attempt.state == AttemptState::VerdictRunning {
                    self.run_verdict();
                } else {
                    let attempt = attempt.number;
                    self.effects
                        .push(EffectKind::CheckPreconditions { attempt });
                }
            }
        }
    }

    fn tick(&mut self) -> Option<()> {
        let State::Parked(park) = self.item.state else {
            return None;
        };
        if self.ctx.hold.is_some() || park.until_ms.is_some_and(|t| self.ctx.now_ms < t) {
            return None;
        }
        self.start(park.next);
        Some(())
    }

    fn kill_switch(&mut self) -> Option<()> {
        let next = match self.item.state {
            State::Running => {
                self.attempt_step(AttemptEvent::Killed)?;
                Next::Resume
            }
            State::Verifying => Next::Verify,
            // The verdict of the rebased commit (#8); a landing task in flight is not stopped.
            // Mutant (`mutant_core_kill_switch_skips_landing`): the re-verdict runs on.
            State::Landing if self.has_child() && !cfg!(mutant_core_kill_switch_skips_landing) => {
                Next::Verify
            }
            _ => return None,
        };
        self.effects.push(EffectKind::Kill);
        self.item.state = State::Parked(Park {
            reason: ParkReason::Killed,
            until_ms: None,
            next,
        });
        Some(())
    }

    fn restarted(&mut self, boot: u32, tail: Option<Exit>) -> Option<()> {
        if boot <= self.item.boot {
            return None;
        }
        match self.item.state {
            State::Claimed => self.effects.push(EffectKind::Claim),
            State::Running => match tail {
                Some(exit @ (Exit::Ok | Exit::Limit { .. } | Exit::Auth)) => {
                    self.exited(exit, "");
                }
                // A lost session costs one uncounted resume (#7, D1).
                Some(Exit::Crash) | None => {
                    if !self.uncounted() {
                        self.start(Next::Resume);
                    }
                }
            },
            State::Verifying => self.start(Next::Verify),
            // The verdict of the rebased commit was in flight: it runs again. A landing task
            // reports `Rebased` for a head no passing run recorded, which a re-verdict in
            // flight does not take.
            // Mutant (`mutant_core_restart_lands_unverified`): `Land` whatever the attempt.
            State::Landing if self.has_child() && !cfg!(mutant_core_restart_lands_unverified) => {
                self.reverify();
            }
            State::Landing => {
                let attempt = self.item.attempt_number();
                self.effects.push(EffectKind::Land { attempt });
            }
            _ => return None,
        }
        self.item.boot = boot;
        Some(())
    }

    /// A running item whose session the restarted daemon follows again (#8): nothing was lost,
    /// so only the boot is recorded, once per boot.
    fn reattached(&mut self, boot: u32) -> Option<()> {
        self.expect(&[State::Running])?;
        if boot <= self.item.boot {
            return None;
        }
        if cfg!(mutant_core_reattach_counts) {
            // Mutant (`mutant_core_reattach_counts`): the re-attach costs an uncounted resume.
            self.item.uncounted += 1;
        }
        self.item.boot = boot;
        Some(())
    }

    fn user(&mut self, command: UserCommand) -> Option<()> {
        let state = self.item.state;
        match command {
            UserCommand::Cancel => {
                if !(state.is_active() || matches!(state, State::NeedsYou(_))) {
                    return None;
                }
                self.kill_child();
                self.item.state = State::Cancelled;
            }
            UserCommand::Requeue => {
                if matches!(state, State::Ready | State::Done(_)) {
                    return None;
                }
                self.kill_child();
                self.reset_counters();
                self.item.state = State::Ready;
            }
            UserCommand::Restart => {
                if !(state.is_active() && state != State::Claimed) {
                    return None;
                }
                self.kill_child();
                self.reset_counters();
                self.item.state = State::Claimed;
                self.effects.push(EffectKind::Claim);
            }
            UserCommand::ManualDone => {
                if matches!(state, State::Done(_)) {
                    return None;
                }
                self.kill_child();
                self.item.state = State::Done(Outcome::UserClosed);
            }
        }
        Some(())
    }

    /// A fresh attempt counter (#5). Attempt, session and run numbers stay unique.
    fn reset_counters(&mut self) {
        self.item.failed = 0;
        self.item.uncounted = 0;
        self.item.conflicts = 0;
        self.item.attempt = None;
    }

    fn kill_child(&mut self) {
        if self.has_child() {
            self.effects.push(EffectKind::Kill);
        }
    }

    /// A child process may be alive: a session or a verdict run, including the verdict of a
    /// rebased commit in [`State::Landing`] (#8).
    fn has_child(&self) -> bool {
        self.item.state.has_child()
            || (self.item.state == State::Landing
                && self
                    .item
                    .attempt
                    .as_ref()
                    .is_some_and(|a| a.state == AttemptState::VerdictRunning))
    }

    fn needs_you(&mut self, reason: Reason) {
        self.item.state = State::NeedsYou(reason);
        self.effects.push(EffectKind::Comment(reason));
    }

    fn finish(mut self, before: &Item, event: &Event) -> Stepped {
        let to = self.item.state;
        // The user moved the card themselves; the claim procedure writes its own status.
        let projects = event.actor() != Actor::User && *event != Event::Claim;
        if projects
            && to.column() != before.state.column()
            && let Some(column) = to.column()
        {
            self.effects.push(EffectKind::Project(column));
        }
        self.effects.push(EffectKind::Report);

        self.item.seq = before.seq + 1;
        let seq = self.item.seq;
        let attempt = self.item.attempt_number();
        let effects = self
            .effects
            .into_iter()
            .zip(0..)
            .map(|(kind, index)| Effect {
                key: EffectKey {
                    item: self.item.id.clone(),
                    attempt,
                    seq,
                    index,
                },
                kind,
            })
            .collect();
        let transition = Transition {
            key: TransitionKey {
                item: self.item.id.clone(),
                seq,
            },
            attempt,
            from: before.state,
            to,
            attempt_from: before.attempt.as_ref().map(|a| a.state),
            attempt_to: self.item.attempt.as_ref().map(|a| a.state),
            ended: self.ended,
            cause: event.name(),
            effects,
        };
        Stepped {
            item: self.item,
            transition,
        }
    }
}

/// Any parked state, for [`Draft::expect`].
const PARKED: Park = Park {
    reason: ParkReason::Limit,
    until_ms: None,
    next: Next::Resume,
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    const T0: i64 = 1_800_000_000_000;

    fn ctx() -> Ctx {
        Ctx::at(T0)
    }

    /// Applies `event`, which must take an arrow.
    fn go(item: &Item, event: &Event, ctx: &Ctx) -> Stepped {
        step(item, event, ctx).unwrap_or_else(|| panic!("{event:?} ignored in {:?}", item.state))
    }

    fn apply(item: Item, events: &[Event]) -> Item {
        events
            .iter()
            .fold(item, |item, e| go(&item, e, &ctx()).item)
    }

    fn running() -> Item {
        apply(
            Item::new(ItemId::new("local:demo#1")),
            &[Event::Claim, Event::ClaimConfirmed],
        )
    }

    fn cur(item: &Item) -> (u32, u32, u32) {
        let a = item.attempt.as_ref().unwrap();
        (a.number, a.spawn, a.run)
    }

    fn exit(item: &Item, exit: Exit) -> Event {
        let (attempt, spawn, _) = cur(item);
        Event::Exited {
            attempt,
            spawn,
            exit,
            text: String::new(),
        }
    }

    fn preconditions(item: &Item, passed: bool) -> Event {
        Event::Preconditions {
            attempt: cur(item).0,
            passed,
        }
    }

    fn verdict(item: &Item, result: RunResult) -> Event {
        let (attempt, _, run) = cur(item);
        Event::Verdict {
            attempt,
            run,
            result,
        }
    }

    /// Runs one attempt of `item` from its session's exit through the verdict runs `results`.
    fn attempt(mut item: Item, results: &[RunResult]) -> Item {
        item = go(&item, &exit(&item, Exit::Ok), &ctx()).item;
        item = go(&item, &preconditions(&item, true), &ctx()).item;
        for &r in results {
            item = go(&item, &verdict(&item, r), &ctx()).item;
        }
        item
    }

    #[test]
    fn a_passing_verdict_lands_and_is_done() {
        let item = attempt(running(), &[RunResult::Pass]);
        assert_eq!(item.state, State::Landing);
        let done = go(&item, &Event::Landed, &ctx());
        assert_eq!(done.item.state, State::Done(Outcome::Pass));
        assert!(
            done.transition
                .effects
                .iter()
                .any(|e| e.kind == EffectKind::Project(Column::Done))
        );
    }

    #[test]
    fn fail_then_pass_is_flaky_and_done() {
        let item = attempt(running(), &[RunResult::Fail, RunResult::Pass]);
        assert_eq!(item.state, State::Landing);
        assert_eq!(item.failed, 0, "a flaky verdict is not a failed attempt");
        let item = go(&item, &Event::Landed, &ctx()).item;
        assert_eq!(item.state, State::Done(Outcome::Flaky));
    }

    #[test]
    fn fail_fail_twice_is_needs_you() {
        let item = attempt(running(), &[RunResult::Fail, RunResult::Fail]);
        assert_eq!(item.state, State::Running, "one failed attempt, one left");
        assert_eq!((item.failed, cur(&item).0), (1, 2));
        let item = attempt(item, &[RunResult::Fail, RunResult::Fail]);
        assert_eq!(item.state, State::NeedsYou(Reason::AttemptsExhausted));
        assert_eq!(item.failed, 2);
    }

    #[test]
    fn the_transition_to_the_next_attempt_carries_the_failed_one() {
        let item = attempt(running(), &[RunResult::Fail]);
        let next = go(&item, &verdict(&item, RunResult::Fail), &ctx());
        assert_eq!(cur(&next.item).0, 2);
        let ended = next.transition.ended.expect("attempt 1 ended here");
        assert_eq!((ended.number, ended.state), (1, AttemptState::Fail));
        assert_eq!(next.transition.attempt_to, Some(AttemptState::Spawned));
        // A step that keeps the attempt ends none.
        let first = go(&running(), &exit(&running(), Exit::Ok), &ctx());
        assert_eq!(first.transition.ended, None);
    }

    #[test]
    fn a_failing_run_is_rerun_with_no_session_in_between() {
        let item = attempt(running(), &[]);
        let first = go(&item, &verdict(&item, RunResult::Fail), &ctx());
        assert_eq!(first.item.state, State::Verifying);
        let kinds: Vec<_> = first.transition.effects.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EffectKind::RunVerdict { attempt: 1, run: 2 }));
        assert!(!kinds.iter().any(|k| matches!(k, EffectKind::Spawn { .. })));
    }

    #[test]
    fn a_limit_exit_parks_and_resumes_uncounted() {
        let item = running();
        let reset = T0 + 60_000;
        let parked = go(
            &item,
            &exit(
                &item,
                Exit::Limit {
                    reset_ms: Some(reset),
                },
            ),
            &ctx(),
        )
        .item;
        assert_eq!(
            parked.state,
            State::Parked(Park {
                reason: ParkReason::Limit,
                until_ms: Some(reset),
                next: Next::Resume,
            })
        );
        assert_eq!(parked.failed, 0);
        assert_eq!(
            step(&parked, &Event::Tick, &ctx()),
            None,
            "not before the reset"
        );

        let resumed = go(&parked, &Event::Tick, &Ctx::at(reset));
        assert_eq!(resumed.item.state, State::Running);
        assert_eq!(resumed.item.failed, 0, "the attempt counter did not move");
        assert_eq!(cur(&resumed.item).0, cur(&item).0, "still the same attempt");
        assert!(resumed.transition.effects.iter().any(|e| matches!(
            e.kind,
            EffectKind::Spawn {
                mode: SpawnMode::Resume,
                ..
            }
        )));
    }

    #[test]
    fn fake_limits_in_a_row_end_in_needs_you() {
        // The bound is far above the cap: without the cap the item parks forever.
        const STEPS: usize = 1_000;
        let mut item = running();
        let mut now = T0;
        for _ in 0..STEPS {
            if let State::NeedsYou(reason) = item.state {
                assert_eq!(reason, Reason::ParkCap);
                assert_eq!(item.failed, 0);
                return;
            }
            let ctx = Ctx::at(now);
            item = if item.state == State::Running {
                go(&item, &exit(&item, Exit::Limit { reset_ms: None }), &ctx)
            } else {
                now += Rules::default().default_backoff_ms;
                go(&item, &Event::Tick, &Ctx::at(now))
            }
            .item;
        }
        panic!("no Needs you within {STEPS} steps: {:?}", item.state);
    }

    #[test]
    fn a_crash_is_retried_once_uncounted_then_fails_the_attempt() {
        let item = running();
        let retried = go(&item, &exit(&item, Exit::Crash), &ctx()).item;
        assert_eq!((retried.state, retried.failed), (State::Running, 0));
        assert_eq!(cur(&retried).0, 1);
        let failed = go(&retried, &exit(&retried, Exit::Crash), &ctx()).item;
        assert_eq!((failed.failed, cur(&failed).0), (1, 2));
    }

    #[test]
    fn a_fast_death_parks_the_item_paused_and_charges_it_nothing() {
        let item = running();
        let item = go(&item, &exit(&item, Exit::Crash), &ctx()).item;
        assert_eq!(item.state, State::Running, "the crash retry was taken");
        let before = item.attempt.clone().unwrap();
        assert_eq!(before.crash_retries, 1);
        let (attempt, spawn, _) = cur(&item);
        let stale = Event::FastDeath {
            attempt,
            spawn: spawn - 1,
        };
        assert_eq!(step(&item, &stale, &ctx()), None, "an older session");
        let tripped = go(&item, &Event::FastDeath { attempt, spawn }, &ctx());
        assert_eq!(tripped.transition.cause, "fast_death");
        let parked = tripped.item;
        assert_eq!(
            parked.state,
            State::Parked(Park {
                reason: ParkReason::Paused,
                until_ms: None,
                next: Next::Reset,
            })
        );
        let after = parked.attempt.as_ref().unwrap();
        assert_eq!(after.crash_retries, before.crash_retries);
        assert_eq!(after.number, before.number);
        assert_eq!(
            (parked.failed, parked.uncounted, parked.attempts),
            (item.failed, item.uncounted, item.attempts)
        );
        let woken = go(&parked, &Event::Tick, &ctx());
        assert_eq!(woken.item.state, State::Running);
        assert_eq!(cur(&woken.item).0, attempt, "the same attempt");
        assert_eq!(cur(&woken.item).1, spawn + 1, "a new session");
        assert!(
            woken.transition.effects.iter().any(|e| e.kind
                == EffectKind::Spawn {
                    attempt,
                    spawn: spawn + 1,
                    mode: SpawnMode::Reset,
                }),
            "{:?}",
            woken.transition.effects
        );
    }

    #[test]
    fn a_missed_handshake_is_retried_once_uncounted_then_needs_you() {
        let item = running();
        let miss = |item: &Item| {
            let (attempt, spawn, _) = cur(item);
            Event::NoHandshake { attempt, spawn }
        };
        let retried = go(&item, &miss(&item), &ctx());
        assert_eq!(retried.transition.cause, "no_handshake");
        let retried = retried.item;
        assert_eq!((retried.state, retried.failed), (State::Running, 0));
        assert_eq!(cur(&retried).0, 1, "the attempt counter is unchanged");
        assert_eq!(cur(&retried).1, cur(&item).1 + 1, "a new session");
        assert_eq!(retried.uncounted, 1);
        let stuck = go(&retried, &miss(&retried), &ctx()).item;
        assert_eq!(stuck.state, State::NeedsYou(Reason::Handshake));
        assert_eq!((stuck.failed, cur(&stuck).0), (0, 1));
    }

    #[test]
    fn a_reattached_item_keeps_running_and_counts_nothing() {
        let item = running();
        let boot = item.boot + 1;
        let next = go(&item, &Event::Reattached { boot }, &ctx());
        assert_eq!(next.transition.cause, "reattached");
        assert!(
            next.transition
                .effects
                .iter()
                .all(|e| e.kind == EffectKind::Report),
            "nothing but the report: {:?}",
            next.transition
        );
        let next = next.item;
        assert_eq!(next.state, State::Running);
        assert_eq!(cur(&next), cur(&item), "the same attempt, spawn and run");
        assert_eq!(
            (next.attempts, next.failed, next.uncounted),
            (item.attempts, item.failed, item.uncounted)
        );
        assert_eq!(next.boot, boot);
        assert_eq!(
            step(&next, &Event::Reattached { boot }, &ctx()),
            None,
            "once per boot"
        );
        let mut verifying = item.clone();
        verifying.state = State::Verifying;
        assert_eq!(step(&verifying, &Event::Reattached { boot }, &ctx()), None);
    }

    #[test]
    fn an_identity_refusal_is_needs_you_at_once_and_counts_nothing() {
        let item = running();
        let (attempt, spawn, _) = cur(&item);
        let next = go(&item, &Event::IdentityRefused { attempt, spawn }, &ctx());
        assert_eq!(next.transition.cause, "identity_refused");
        let next = next.item;
        assert_eq!(next.state, State::NeedsYou(Reason::Identity));
        assert_eq!((next.failed, next.uncounted), (0, 0));
    }

    #[test]
    fn expired_auth_is_needs_you_at_once() {
        let item = running();
        let next = go(&item, &exit(&item, Exit::Auth), &ctx()).item;
        assert_eq!(next.state, State::NeedsYou(Reason::Auth));
    }

    #[test]
    fn a_pause_lets_the_attempt_finish_and_starts_nothing_new() {
        let paused = Ctx {
            hold: Some(Hold::Pause),
            ..ctx()
        };
        let item = running();
        let verifying = go(&item, &exit(&item, Exit::Ok), &paused).item;
        assert_eq!(verifying.state, State::Verifying, "the verdict still runs");
        let v = go(&verifying, &preconditions(&verifying, true), &paused).item;
        let v = go(&v, &verdict(&v, RunResult::Fail), &paused).item;
        let held = go(&v, &verdict(&v, RunResult::Fail), &paused).item;
        assert!(matches!(
            held.state,
            State::Parked(Park {
                reason: ParkReason::Paused,
                next: Next::NewAttempt,
                ..
            })
        ));
        assert_eq!(step(&held, &Event::Tick, &paused), None);
        let woke = go(&held, &Event::Tick, &ctx()).item;
        assert_eq!((woke.state, cur(&woke).0), (State::Running, 2));
    }

    #[test]
    fn the_kill_switch_parks_and_kills() {
        let item = running();
        let killed = go(&item, &Event::KillSwitch, &ctx());
        assert!(
            killed
                .transition
                .effects
                .iter()
                .any(|e| e.kind == EffectKind::Kill)
        );
        let kill = Ctx {
            hold: Some(Hold::Kill),
            ..ctx()
        };
        assert_eq!(step(&killed.item, &Event::Tick, &kill), None);
        let resumed = go(&killed.item, &Event::Tick, &ctx()).item;
        assert_eq!(resumed.state, State::Running);
        assert_eq!(cur(&resumed).0, 1);
    }

    #[test]
    fn user_commands_on_daemon_owned_cards() {
        let item = running();
        let cancelled = go(&item, &Event::User(UserCommand::Cancel), &ctx());
        assert_eq!(cancelled.item.state, State::Cancelled);
        assert!(
            !cancelled
                .transition
                .effects
                .iter()
                .any(|e| matches!(e.kind, EffectKind::Project(_))),
            "the user moved the card; nothing is projected"
        );

        let exhausted = attempt(
            attempt(running(), &[RunResult::Fail, RunResult::Fail]),
            &[RunResult::Fail, RunResult::Fail],
        );
        let retried = go(&exhausted, &Event::User(UserCommand::Requeue), &ctx()).item;
        assert_eq!((retried.state, retried.failed), (State::Ready, 0));

        let restarted = go(&item, &Event::User(UserCommand::Restart), &ctx()).item;
        assert_eq!(restarted.state, State::Claimed);

        let done = go(&item, &Event::User(UserCommand::ManualDone), &ctx()).item;
        assert_eq!(done.state, State::Done(Outcome::UserClosed));
        assert_eq!(step(&done, &Event::User(UserCommand::Cancel), &ctx()), None);
    }

    #[test]
    fn a_card_failing_lint_is_moved_to_needs_you_with_a_comment() {
        let item = Item::new(ItemId::new("local:demo#2"));
        let claimed = go(&item, &Event::Claim, &ctx());
        assert!(
            !claimed
                .transition
                .effects
                .iter()
                .any(|e| matches!(e.kind, EffectKind::Project(_))),
            "the claim procedure writes its own status"
        );
        let linted = go(&claimed.item, &Event::LintFailed, &ctx());
        let kinds: Vec<_> = linted.transition.effects.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                EffectKind::Comment(Reason::Lint),
                EffectKind::Project(Column::NeedsYou),
                EffectKind::Report,
            ]
        );
        assert_eq!(linted.transition.key.to_string(), "local:demo#2/t2");
        assert_eq!(
            linted.transition.effects[1].key.to_string(),
            "local:demo#2/a0/t2/e1"
        );
    }

    #[test]
    fn stale_events_are_ignored() {
        let item = running();
        let next = go(&item, &exit(&item, Exit::Crash), &ctx()).item;
        assert_eq!(
            step(&next, &exit(&item, Exit::Ok), &ctx()),
            None,
            "old session"
        );
        let v = attempt(next, &[]);
        let old_run = verdict(&v, RunResult::Fail);
        let v = go(&v, &old_run, &ctx()).item;
        assert_eq!(step(&v, &old_run, &ctx()), None, "a run finishes once");
    }

    /// The kinds of a step's effects.
    fn kinds(stepped: &Stepped) -> Vec<EffectKind> {
        stepped.transition.effects.iter().map(|e| e.kind).collect()
    }

    fn rebased(item: &Item) -> Event {
        Event::Rebased {
            attempt: cur(item).0,
        }
    }

    /// The conflict of the current landing pass: the one the attempt's current run started.
    fn conflict(item: &Item) -> Event {
        let (attempt, _, run) = cur(item);
        Event::Conflict { attempt, run }
    }

    fn is_run_verdict(k: &EffectKind) -> bool {
        matches!(k, EffectKind::RunVerdict { .. })
    }

    fn parked_to_verify(item: &Item) -> bool {
        item.state
            == State::Parked(Park {
                reason: ParkReason::Killed,
                until_ms: None,
                next: Next::Verify,
            })
    }

    #[test]
    fn a_daemon_restarted_during_a_re_verdict_runs_it_again() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let item = go(&landing, &rebased(&landing), &ctx()).item;
        let (attempt, _, in_flight) = cur(&item);
        let boot = item.boot + 1;
        let restarted = go(&item, &Event::Restarted { boot, tail: None }, &ctx());
        assert_eq!(restarted.item.state, State::Landing);
        let run = cur(&restarted.item).2;
        assert!(run > in_flight, "a new run: {run} after {in_flight}");
        let effects = kinds(&restarted);
        assert!(effects.contains(&EffectKind::RunVerdict { attempt, run }));
        assert!(
            !effects.contains(&EffectKind::Land { attempt }),
            "the rebased commit has not passed: {effects:?}"
        );
        let item = restarted.item;
        let old = Event::Verdict {
            attempt,
            run: in_flight,
            result: RunResult::Pass,
        };
        assert_eq!(step(&item, &old, &ctx()), None, "the lost run is ignored");
        let passed = go(&item, &verdict(&item, RunResult::Pass), &ctx());
        assert_eq!(passed.item.state, State::Landing);
        assert!(kinds(&passed).contains(&EffectKind::Land { attempt }));
        let done = go(&passed.item, &Event::Landed, &ctx()).item;
        assert_eq!(done.state, State::Done(Outcome::Pass));

        // A passed item in landing lands again.
        let boot = landing.boot + 1;
        let again = go(&landing, &Event::Restarted { boot, tail: None }, &ctx());
        assert_eq!(again.item.state, State::Landing);
        let effects = kinds(&again);
        assert!(effects.contains(&EffectKind::Land { attempt }));
        assert!(!effects.iter().any(is_run_verdict), "{effects:?}");
    }

    #[test]
    fn the_kill_switch_reaches_a_re_verdict_in_landing() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let attempt = cur(&landing).0;
        let item = go(&landing, &rebased(&landing), &ctx()).item;
        let killed = go(&item, &Event::KillSwitch, &ctx());
        assert!(kinds(&killed).contains(&EffectKind::Kill));
        assert!(parked_to_verify(&killed.item), "{:?}", killed.item.state);
        let kill = Ctx {
            hold: Some(Hold::Kill),
            ..ctx()
        };
        assert_eq!(step(&killed.item, &Event::Tick, &kill), None);
        let woke = go(&killed.item, &Event::Tick, &ctx());
        assert_eq!(woke.item.state, State::Verifying);
        let run = cur(&woke.item).2;
        assert!(kinds(&woke).contains(&EffectKind::RunVerdict { attempt, run }));
        let passed = go(&woke.item, &verdict(&woke.item, RunResult::Pass), &ctx());
        assert_eq!(passed.item.state, State::Landing);
        assert!(kinds(&passed).contains(&EffectKind::Land { attempt }));

        // A user command there stops the verdict child.
        for command in [
            UserCommand::Cancel,
            UserCommand::Requeue,
            UserCommand::Restart,
            UserCommand::ManualDone,
        ] {
            let next = go(&item, &Event::User(command), &ctx());
            assert!(kinds(&next).contains(&EffectKind::Kill), "{command:?}");
        }
        let setup = go(&item, &Event::SetupFailed, &ctx());
        assert!(kinds(&setup).contains(&EffectKind::Kill));

        // The landing task of a passed attempt is not the kill switch's.
        assert_eq!(step(&landing, &Event::KillSwitch, &ctx()), None);
        let cancelled = go(&landing, &Event::User(UserCommand::Cancel), &ctx());
        assert!(!kinds(&cancelled).contains(&EffectKind::Kill));
    }

    #[test]
    fn a_rebase_under_the_kill_switch_waits_to_verify() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let attempt = cur(&landing).0;
        let kill = Ctx {
            hold: Some(Hold::Kill),
            ..ctx()
        };
        let held = go(&landing, &rebased(&landing), &kill);
        assert!(parked_to_verify(&held.item), "{:?}", held.item.state);
        assert!(
            !kinds(&held).iter().any(is_run_verdict),
            "{:?}",
            kinds(&held)
        );
        assert_eq!(
            held.item.attempt.as_ref().unwrap().state,
            AttemptState::VerdictRunning,
            "the verdict before the rebase is gone"
        );
        let woke = go(&held.item, &Event::Tick, &ctx());
        assert_eq!(woke.item.state, State::Verifying);
        assert!(kinds(&woke).iter().any(is_run_verdict));
        let passed = go(&woke.item, &verdict(&woke.item, RunResult::Pass), &ctx());
        assert!(kinds(&passed).contains(&EffectKind::Land { attempt }));

        let pause = Ctx {
            hold: Some(Hold::Pause),
            ..ctx()
        };
        let verifying = go(&landing, &rebased(&landing), &pause);
        assert_eq!(verifying.item.state, State::Landing);
        assert!(kinds(&verifying).iter().any(is_run_verdict));
    }

    #[test]
    fn a_flaky_attempt_gets_the_re_run_rule_on_its_rebased_commit() {
        let flaky = attempt(running(), &[RunResult::Fail, RunResult::Pass]);
        assert_eq!(flaky.attempt.as_ref().unwrap().state, AttemptState::Flaky);
        let attempt = cur(&flaky).0;
        let item = go(&flaky, &rebased(&flaky), &ctx()).item;
        let rerun = go(&item, &verdict(&item, RunResult::Fail), &ctx());
        assert_eq!(rerun.item.state, State::Landing, "re-run once first");
        assert_eq!(rerun.item.failed, 0);
        assert!(kinds(&rerun).iter().any(is_run_verdict));

        let passed = go(&rerun.item, &verdict(&rerun.item, RunResult::Pass), &ctx());
        assert_eq!(passed.item.state, State::Landing);
        assert_eq!(
            passed.item.attempt.as_ref().unwrap().state,
            AttemptState::Flaky
        );
        assert!(kinds(&passed).contains(&EffectKind::Land { attempt }));
        let done = go(&passed.item, &Event::Landed, &ctx()).item;
        assert_eq!(done.state, State::Done(Outcome::Flaky));

        let failed = go(&rerun.item, &verdict(&rerun.item, RunResult::Fail), &ctx()).item;
        assert_eq!(failed.state, State::Running);
        assert_eq!((failed.failed, cur(&failed).0), (1, attempt + 1));

        // A first pass on the rebased commit is a pass.
        let pass = go(&item, &verdict(&item, RunResult::Pass), &ctx()).item;
        assert_eq!(pass.attempt.as_ref().unwrap().state, AttemptState::Pass);
    }

    #[test]
    fn a_conflict_from_an_earlier_landing_pass_is_ignored() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let first_event = conflict(&landing);
        let first = go(&landing, &first_event, &ctx()).item;
        assert_eq!((first.state, first.conflicts), (State::Running, 1));

        // The session exits, verifies under a new run and reaches landing again.
        let again = attempt(first, &[RunResult::Pass]);
        assert_eq!(again.state, State::Landing);
        assert_eq!(step(&again, &first_event, &ctx()), None, "the earlier pass");
        assert_eq!(again.conflicts, 1);

        let second = go(&again, &conflict(&again), &ctx()).item;
        assert_eq!(second.state, State::NeedsYou(Reason::LandingRefused));

        // A re-verdict in flight is not a landing pass that can conflict.
        let reverifying = go(&again, &rebased(&again), &ctx()).item;
        assert_eq!(step(&reverifying, &conflict(&reverifying), &ctx()), None);
    }

    #[test]
    fn a_rebased_commit_is_verified_again_before_it_lands() {
        let item = attempt(running(), &[RunResult::Pass]);
        assert_eq!(item.state, State::Landing);
        let (attempt, _, old_run) = cur(&item);
        let again = go(&item, &rebased(&item), &ctx());
        assert_eq!(again.transition.cause, "rebased");
        assert_eq!(again.item.state, State::Landing);
        let run = cur(&again.item).2;
        assert_eq!(run, old_run + 1, "a new verdict run");
        let effects = kinds(&again);
        assert!(effects.contains(&EffectKind::RunVerdict { attempt, run }));
        assert!(
            !effects.contains(&EffectKind::Land { attempt }),
            "the verdict before the rebase is not a verdict on the target"
        );
        let item = again.item;
        assert_eq!(step(&item, &rebased(&item), &ctx()), None, "rebased once");
        let old = Event::Verdict {
            attempt,
            run: old_run,
            result: RunResult::Pass,
        };
        assert_eq!(step(&item, &old, &ctx()), None, "the old run is ignored");
        assert_eq!(
            step(&item, &Event::Landed, &ctx()),
            None,
            "not landed before the rebased commit passes"
        );

        let passed = go(&item, &verdict(&item, RunResult::Pass), &ctx());
        assert_eq!(passed.item.state, State::Landing);
        assert!(kinds(&passed).contains(&EffectKind::Land { attempt }));
        let done = go(&passed.item, &Event::Landed, &ctx()).item;
        assert_eq!(done.state, State::Done(Outcome::Pass));
    }

    #[test]
    fn a_rebased_commit_that_fails_its_verdict_is_a_failed_attempt() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let item = go(&landing, &rebased(&landing), &ctx()).item;
        let rerun = go(&item, &verdict(&item, RunResult::Fail), &ctx());
        assert_eq!(rerun.item.state, State::Landing, "re-run once first");
        assert!(
            kinds(&rerun)
                .iter()
                .any(|k| matches!(k, EffectKind::RunVerdict { .. }))
        );
        let failed = go(&rerun.item, &verdict(&rerun.item, RunResult::Fail), &ctx());
        assert_eq!(failed.item.state, State::Running);
        assert_eq!((failed.item.failed, cur(&failed.item).0), (1, 2));
        assert!(kinds(&failed).iter().any(|k| matches!(
            k,
            EffectKind::Spawn {
                attempt: 2,
                mode: SpawnMode::Fresh,
                ..
            }
        )));

        // With the limit reached, the failed re-verdict is Needs you.
        let last = Ctx {
            rules: Rules {
                max_attempts: 1,
                ..Rules::default()
            },
            ..ctx()
        };
        let v = go(&rerun.item, &verdict(&rerun.item, RunResult::Fail), &last).item;
        assert_eq!(v.state, State::NeedsYou(Reason::AttemptsExhausted));
        assert_eq!(v.failed, 1);

        // Fail then pass is flaky and lands.
        let flaky = go(&rerun.item, &verdict(&rerun.item, RunResult::Pass), &ctx());
        assert_eq!(flaky.item.state, State::Landing);
        assert_eq!(
            flaky.item.attempt.as_ref().unwrap().state,
            AttemptState::Flaky
        );
        assert!(kinds(&flaky).contains(&EffectKind::Land { attempt: 1 }));
        assert_eq!(flaky.item.failed, 0);
    }

    #[test]
    fn a_second_conflict_needs_the_user() {
        let landing = attempt(running(), &[RunResult::Pass]);
        let (number, spawn, _) = cur(&landing);
        let first = go(&landing, &conflict(&landing), &ctx());
        assert_eq!(first.transition.cause, "conflict");
        assert_eq!(first.item.state, State::Running);
        assert_eq!(first.item.conflicts, 1);
        assert_eq!(cur(&first.item).0, number, "the same attempt");
        assert!(kinds(&first).contains(&EffectKind::Spawn {
            attempt: number,
            spawn: spawn + 1,
            mode: SpawnMode::Reset,
        }));
        assert_eq!(step(&first.item, &conflict(&first.item), &ctx()), None);

        // The session exits, its commits verify and reach landing again.
        let again = attempt(first.item, &[RunResult::Pass]);
        assert_eq!(again.state, State::Landing);
        let second = go(&again, &conflict(&again), &ctx());
        assert_eq!(second.item.state, State::NeedsYou(Reason::LandingRefused));
        assert!(kinds(&second).contains(&EffectKind::Comment(Reason::LandingRefused)));
        assert!(
            !kinds(&second)
                .iter()
                .any(|k| matches!(k, EffectKind::Spawn { .. }))
        );
        // A requeue resets the conflict counter with the others.
        let requeued = go(&second.item, &Event::User(UserCommand::Requeue), &ctx()).item;
        assert_eq!(requeued.conflicts, 0);
    }

    /// #8: a pull request closed unmerged moves an item Done with a passing verdict to Needs
    /// you once, with a comment; a second sighting, a card the user closed and an item still in
    /// flight take no arrow.
    #[test]
    fn a_closed_pull_request_moves_a_done_item_to_needs_you_once() {
        let passed = attempt(running(), &[RunResult::Pass]);
        let done = go(&passed, &Event::Landed, &ctx()).item;
        assert_eq!(done.state, State::Done(Outcome::Pass));
        let closed = go(&done, &Event::PullRequestClosed, &ctx());
        assert_eq!(closed.transition.cause, "pr_closed");
        assert_eq!(closed.item.state, State::NeedsYou(Reason::LandingRefused));
        let effects = kinds(&closed);
        assert!(effects.contains(&EffectKind::Comment(Reason::LandingRefused)));
        assert!(effects.contains(&EffectKind::Project(Column::NeedsYou)));
        assert_eq!(step(&closed.item, &Event::PullRequestClosed, &ctx()), None);

        let flaky = Item {
            state: State::Done(Outcome::Flaky),
            ..done.clone()
        };
        assert_eq!(
            go(&flaky, &Event::PullRequestClosed, &ctx()).item.state,
            State::NeedsYou(Reason::LandingRefused)
        );
        let by_hand = Item {
            state: State::Done(Outcome::UserClosed),
            ..done
        };
        assert_eq!(step(&by_hand, &Event::PullRequestClosed, &ctx()), None);
        assert_eq!(step(&running(), &Event::PullRequestClosed, &ctx()), None);
    }

    #[test]
    fn a_stale_event_moves_every_unfinished_item_to_needs_you() {
        let ready = Item::new(ItemId::new("local:demo#1"));
        let claimed = go(&ready, &Event::Claim, &ctx()).item;
        let verifying = go(&running(), &exit(&running(), Exit::Ok), &ctx()).item;
        assert_eq!(verifying.state, State::Verifying);
        let parked = Item {
            state: State::Parked(PARKED),
            ..running()
        };
        let landing = landing();
        assert_eq!(landing.state, State::Landing);
        for item in [
            ready.clone(),
            claimed,
            running(),
            parked,
            verifying,
            landing,
        ] {
            let next = go(&item, &Event::Stale, &ctx());
            assert_eq!(next.transition.cause, "stale");
            assert_eq!(
                next.item.state,
                State::NeedsYou(Reason::Stale),
                "from {:?}",
                item.state
            );
            let effects = kinds(&next);
            assert!(effects.contains(&EffectKind::Comment(Reason::Stale)));
            assert!(effects.contains(&EffectKind::Project(Column::NeedsYou)));
            if item.state == State::Running {
                assert!(effects.contains(&EffectKind::Kill), "{effects:?}");
            }
        }
        let done = Item {
            state: State::Done(Outcome::Pass),
            ..running()
        };
        let cancelled = Item {
            state: State::Cancelled,
            ..ready.clone()
        };
        let linted = go(&ready, &Event::LintFailed, &ctx()).item;
        assert_eq!(linted.state, State::NeedsYou(Reason::Lint));
        for item in [done, cancelled, linted] {
            assert_eq!(step(&item, &Event::Stale, &ctx()), None, "{:?}", item.state);
        }
    }

    #[test]
    fn a_setup_failure_needs_the_user_uncounted() {
        let claimed = go(
            &Item::new(ItemId::new("local:demo#3")),
            &Event::Claim,
            &ctx(),
        )
        .item;
        let mut crashed = go(&running(), &exit(&running(), Exit::Crash), &ctx()).item;
        assert_eq!(crashed.uncounted, 1);
        crashed.failed = 1;
        for item in [claimed, running(), crashed] {
            let next = go(&item, &Event::SetupFailed, &ctx());
            assert_eq!(next.transition.cause, "setup_failed");
            assert_eq!(next.item.state, State::NeedsYou(Reason::Setup));
            assert_eq!(
                (next.item.failed, next.item.uncounted),
                (item.failed, item.uncounted),
                "from {:?}",
                item.state
            );
            let effects = kinds(&next);
            assert!(effects.contains(&EffectKind::Comment(Reason::Setup)));
            assert!(effects.contains(&EffectKind::Project(Column::NeedsYou)));
            assert_eq!(
                effects.contains(&EffectKind::Kill),
                item.state == State::Running,
                "a running child is killed"
            );
            assert_eq!(step(&next.item, &Event::SetupFailed, &ctx()), None);
        }
    }

    #[test]
    fn the_checkout_guard_is_needs_you() {
        let item = attempt(running(), &[]);
        let refused = Event::Refused {
            attempt: cur(&item).0,
            guard: Guard::Checkout,
        };
        let next = go(&item, &refused, &ctx()).item;
        assert_eq!(next.state, State::NeedsYou(Reason::Guard(Guard::Checkout)));
        assert_eq!(next.failed, 0);
    }

    // ---- Properties over random event sequences ----

    /// One step of a generated sequence: which event (chosen among those that fit the state,
    /// so sequences make progress), whether it is addressed to a stale number, the hold.
    type Pick = (u8, bool, u8);

    #[allow(clippy::too_many_lines)] // one list of every event, by state
    fn events_for(item: &Item, stale: bool) -> Vec<Event> {
        let (attempt, spawn, run) = item
            .attempt
            .as_ref()
            .map_or((0, 0, 0), |a| (a.number, a.spawn, a.run));
        let back = u32::from(stale);
        let (attempt, spawn, run) = (
            attempt.saturating_sub(back),
            spawn.saturating_sub(back),
            run.saturating_sub(back),
        );
        let exited = |exit, text: &str| Event::Exited {
            attempt,
            spawn,
            exit,
            text: text.to_owned(),
        };
        let mut events = vec![
            Event::Tick,
            Event::Summary { attempt },
            Event::Note { attempt },
            Event::Restarted {
                boot: item.boot + 1,
                tail: None,
            },
            Event::Reattached {
                boot: item.boot + 1,
            },
        ];
        match item.state {
            State::Ready => events.extend([Event::Claim, Event::Claim, Event::LintFailed]),
            State::Claimed => events.extend([
                Event::ClaimConfirmed,
                Event::ClaimConfirmed,
                Event::ClaimLost,
            ]),
            State::Running => events.extend([
                exited(Exit::Ok, "done"),
                exited(Exit::Ok, "All tasks are done; the verdict passes."),
                exited(Exit::Ok, ""),
                exited(Exit::Limit { reset_ms: None }, ""),
                exited(Exit::Crash, "done"),
                Event::ResumeFailed { attempt, spawn },
                Event::FastDeath { attempt, spawn },
                Event::IdentityRefused { attempt, spawn },
                Event::KillSwitch,
            ]),
            State::Verifying => events.extend([
                Event::Preconditions {
                    attempt,
                    passed: true,
                },
                Event::Preconditions {
                    attempt,
                    passed: false,
                },
                Event::Verdict {
                    attempt,
                    run,
                    result: RunResult::Fail,
                },
                Event::Verdict {
                    attempt,
                    run,
                    result: RunResult::Timeout,
                },
            ]),
            State::Landing => events.extend([
                Event::Landed,
                Event::Landed,
                Event::Rebased { attempt },
                Event::Conflict { attempt, run },
                Event::KillSwitch,
                Event::Verdict {
                    attempt,
                    run,
                    result: RunResult::Pass,
                },
                Event::Verdict {
                    attempt,
                    run,
                    result: RunResult::Fail,
                },
            ]),
            State::Done(_) => events.extend([Event::PullRequestClosed, Event::PullRequestClosed]),
            _ => {}
        }
        // Rare: these end the item or need the user.
        events.extend([
            Event::PullRequestClosed,
            Event::Question { attempt },
            Event::Refused {
                attempt,
                guard: Guard::IgnoredFile,
            },
            Event::BudgetExceeded,
            Event::SetupFailed,
            Event::Stale,
            Event::User(UserCommand::Requeue),
            Event::User(UserCommand::Restart),
            Event::User(UserCommand::ManualDone),
        ]);
        events
    }

    fn hold(choice: u8) -> Option<Hold> {
        match choice % 16 {
            0 => Some(Hold::Pause),
            1 => Some(Hold::Kill),
            2 => Some(Hold::DailyCap),
            _ => None,
        }
    }

    fn picks() -> impl Strategy<Value = Vec<Pick>> {
        prop::collection::vec((any::<u8>(), prop::bool::weighted(0.1), any::<u8>()), 1..80)
    }

    /// Runs a generated sequence from a new item; calls `check` after every step that took an
    /// arrow with the item before, the event, the context and the result.
    fn run(picks: &[Pick], check: impl FnMut(&Item, &Event, &Ctx, &Stepped)) {
        run_from(Item::new(ItemId::new("local:demo#7")), picks, check);
    }

    /// The starts of the generated sequences besides a new item: one that has passed its
    /// verdict and is landing, which the generator alone never reaches (it produces no passing
    /// verdict outside landing).
    fn landing() -> Item {
        attempt(running(), &[RunResult::Pass])
    }

    /// Runs a generated sequence from `start`.
    fn run_from(start: Item, picks: &[Pick], mut check: impl FnMut(&Item, &Event, &Ctx, &Stepped)) {
        let mut item = start;
        for (i, &(choice, stale, h)) in picks.iter().enumerate() {
            let events = events_for(&item, stale);
            let event = &events[usize::from(choice) % events.len()];
            let now = T0 + i64::try_from(i).unwrap() * 3_600_000;
            let ctx = Ctx {
                hold: hold(h),
                ..Ctx::at(now)
            };
            if let Some(stepped) = step(&item, event, &ctx) {
                check(&item, event, &ctx, &stepped);
                item = stepped.item;
            }
        }
    }

    proptest! {
        // No regression files: a mutant run fails these tests on purpose and must not write
        // into the source tree.
        #![proptest_config(ProptestConfig {
            cases: 512,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// Invariant 2 of #1. The generator never produces a passing verdict run; whatever the
        /// agent says (in a question, a summary, a note or the text of its exit), the only way
        /// into Done is the user's own move.
        #[test]
        fn no_sequence_without_a_passing_verdict_reaches_done(picks in picks()) {
            let mut bad = None;
            run(&picks, |_, event, _, stepped| {
                if let State::Done(outcome) = stepped.item.state
                    && outcome != Outcome::UserClosed
                    && bad.is_none()
                {
                    bad = Some((event.clone(), outcome));
                }
            });
            prop_assert_eq!(bad, None);
        }

        /// An agent-originated event moves an item to Needs you or nowhere.
        #[test]
        fn agent_events_lead_to_needs_you_only(picks in picks()) {
            for start in [Item::new(ItemId::new("local:demo#7")), landing()] {
                let mut bad = None;
                run_from(start, &picks, |before, event, _, stepped| {
                    if event.actor() == Actor::Agent
                        && !matches!(stepped.item.state, State::NeedsYou(_))
                        && bad.is_none()
                    {
                        bad = Some((before.state, event.clone(), stepped.item.state));
                    }
                });
                prop_assert_eq!(bad, None);
            }
        }

        /// Applying the same event twice yields the same keys and no new effect; every key is
        /// unique over a sequence.
        #[test]
        fn the_same_event_twice_adds_nothing(picks in picks()) {
            for start in [Item::new(ItemId::new("local:demo#7")), landing()] {
                let mut problems = Vec::new();
                let mut transitions = BTreeSet::new();
                let mut effects = BTreeSet::new();
                run_from(start, &picks, |before, event, ctx, stepped| {
                    if step(before, event, ctx).as_ref() != Some(stepped) {
                        problems.push(format!("not deterministic: {event:?}"));
                    }
                    if let Some(again) = step(&stepped.item, event, ctx) {
                        problems.push(format!(
                            "{event:?} applied twice: {:?} -> {:?}",
                            stepped.item.state, again.item.state
                        ));
                    }
                    if !transitions.insert(stepped.transition.key.to_string()) {
                        problems.push(format!("duplicate key {}", stepped.transition.key));
                    }
                    for effect in &stepped.transition.effects {
                        if !effects.insert(effect.key.to_string()) {
                            problems.push(format!("duplicate key {}", effect.key));
                        }
                    }
                });
                prop_assert!(problems.is_empty(), "{problems:#?}");
            }
        }

        /// The text of an exit never changes what the machine does.
        #[test]
        fn exit_text_is_never_read(picks in picks(), text in "[a-z ]{0,12}") {
            let mut bad = None;
            run(&picks, |before, event, ctx, stepped| {
                if let Event::Exited { attempt, spawn, exit, .. } = event {
                    let other = Event::Exited {
                        attempt: *attempt,
                        spawn: *spawn,
                        exit: *exit,
                        text: format!("{text} done"),
                    };
                    let alt = step(before, &other, ctx).map(|s| s.item);
                    if alt.as_ref() != Some(&stepped.item) && bad.is_none() {
                        bad = Some(other);
                    }
                }
            });
            prop_assert_eq!(bad, None);
        }
    }
}
