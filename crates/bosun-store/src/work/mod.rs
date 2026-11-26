//! The loop's tables (#7, #57): items, snapshots, attempts, run sessions, verdict runs, the
//! transition log, the outbox, the agent's calls, board comments and flaky tests. And those of
//! stage 2 (#8): the worktrees the daemon made, the session shims it started, the history of
//! waits for leased resources and where each landing stands.
//!
//! The writer records what the item machine ([`bosun_core::step`]) decided:
//! [`Write::Transition`](crate::Write::Transition) stores the arrow under its idempotence key
//! together with the item and attempt it produced and the board writes among its effects, in
//! the same transaction, so a replayed transition is a no-op and an effect never exists without
//! its row. The other writes record facts the machine does not hold: the snapshot, the session
//! ids and child processes, verdict output, the agent's calls and the board's acknowledgements.
//!
//! The reader gives the loop its items back as [`bosun_core::Item`], exactly as recorded, and
//! gives the report and the API the history.

mod codec;
mod read;
mod report;
#[cfg(test)]
mod tests;
pub(crate) mod write;

pub use read::{
    AgentCallRow, AttemptRow, DenialRow, LandingRow, LeaseWaitRow, OutboxOp, OutboxRow, ProcessRow,
    RecordedChild, RunRow, StoredItem, TransitionRow, VerdictRow, WorktreeRow,
};
pub use report::{ItemHistory, RunCommitRow};

use bosun_core::{Exit, ItemId, RunResult, SpawnMode};

/// A new item, or new metadata for a known one. Keyed by the item id: writing it again updates
/// the card key and leaves the state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItem {
    pub item: ItemId,
    /// The repository's configuration name (`repo.<name>`): the item belongs to the row of that
    /// name, wherever its path is.
    pub name: String,
    /// The repository's canonical path key, as configured; registered (or moved to) with the
    /// name as [`crate::Write::Repo`] does.
    pub repo: String,
    /// The queue backend's id of the card, if it has one.
    pub card_key: Option<String>,
    pub at_ms: i64,
}

/// What an item was at claim (#5). Becomes the item's current snapshot; a new attempt runs from
/// the current snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub item: ItemId,
    /// The idempotence key of the claim that took it (its effect key): a replay stores nothing.
    pub key: String,
    pub taken_ms: i64,
    pub hash: String,
    pub body: String,
    /// The parsed contract block, as JSON.
    pub contract_json: String,
    /// The comments that will be inlined into the prompt, as JSON.
    pub comments_json: String,
    pub config_hash: Option<String>,
    pub policy_hash: Option<String>,
    /// The commit the item starts from.
    pub base_sha: Option<String>,
    /// The branch HEAD was on at claim.
    pub branch: Option<String>,
    /// Every ref at claim, full name to object, as a JSON object (#68).
    pub base_refs_json: Option<String>,
    /// The stash entries at claim, newest first, as a JSON array (#68).
    pub base_stash_json: Option<String>,
}

/// A session about to be spawned for an attempt, with the provider session id bosun assigned to
/// it. Written before the spawn, so a transcript of the session, whenever it is ingested, is a
/// run session. Keyed by (attempt, spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    pub item: ItemId,
    pub attempt: u32,
    /// The session's number, as in the `Spawn` effect.
    pub spawn: u32,
    pub mode: SpawnMode,
    pub provider: String,
    /// The pre-assigned provider session id.
    pub session_key: String,
    pub started_ms: i64,
    /// HEAD when the session started, the start of its commit range (#8); `None` when it could
    /// not be read.
    pub start_sha: Option<String>,
    /// The effort level the session asked for (`--effort`); `None` when none was passed. Set on
    /// the attempt: every session of an attempt asks for the same one.
    pub effort: Option<String>,
}

/// A verdict run that started. Keyed by (attempt, run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictStart {
    pub item: ItemId,
    pub attempt: u32,
    /// The run's number, as in the `RunVerdict` effect.
    pub run: u32,
    /// The command, from the snapshot.
    pub command: String,
    /// The child process (#50: pid and start time identify it).
    pub pid: u32,
    pub pid_started_ms: i64,
    pub started_ms: i64,
}

/// A verdict run that finished. Clears its child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictEnd {
    pub item: ItemId,
    pub attempt: u32,
    pub run: u32,
    pub result: RunResult,
    pub exit_code: Option<i32>,
    /// Measured on a monotonic clock.
    pub duration_ms: u64,
    /// The verdict line (#44 B).
    pub verdict_line: Option<String>,
    /// The bounded output tail.
    pub tail: Option<String>,
    pub ended_ms: i64,
}

/// What the agent called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCallKind {
    /// `submit_summary`: the last one of an attempt wins.
    Summary {
        decisions: Vec<String>,
        followups: Vec<String>,
    },
    /// `ask_question`.
    Question { blocking: bool },
    /// `note`.
    Note,
}

/// A call of the agent (#7), bound to its attempt and to HEAD at the time of the call. It is
/// not a transition: a blocking question records its transition separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCall {
    pub item: ItemId,
    pub attempt: u32,
    pub kind: AgentCallKind,
    /// The summary's body, the question's or the note's text.
    pub body: String,
    pub head_sha: Option<String>,
    /// The call was over its rate cap and refused; it is still recorded.
    pub refused: bool,
    pub at_ms: i64,
}

/// A session that ended. Clears the attempt's child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    pub item: ItemId,
    pub attempt: u32,
    pub spawn: u32,
    pub exit: Exit,
    /// HEAD when the session ended: the end of the attempt's commit range so far.
    pub head_sha: Option<String>,
    pub ended_ms: i64,
}

/// The commit range of a session that ended (#72): HEAD when it started and when it ended, and
/// the commits of `base..head` as git lists them. Each listed commit is linked to this session,
/// and through it to its attempt and item, exactly. Keyed by (attempt, spawn): writing it again
/// records the new range and adds the commits not yet linked; a commit another session's range
/// already lists stays with that session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCommits {
    pub item: ItemId,
    pub attempt: u32,
    /// The session's number, as in its [`RunStart`].
    pub spawn: u32,
    /// HEAD when the session started.
    pub base_sha: String,
    /// HEAD when the session ended.
    pub head_sha: String,
    /// The shas of `base..head`, in any order.
    pub commits: Vec<String>,
}

/// Who refused a tool call of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialSource {
    /// The agent CLI, under the permission lists.
    Permission,
    /// A repository's `PreToolUse` hook (reserved: not read yet).
    Hook,
}

impl DenialSource {
    /// Its name as stored: `permission`, `hook`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        codec::denial_source(self)
    }
}

/// One tool call a session's agent CLI refused (#7: refusals per attempt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedCall {
    pub source: DenialSource,
    /// The tool's name.
    pub tool: String,
    /// The call's subject (a command line, a path); `None` when the stream did not say.
    pub target: Option<String>,
    /// Why it was refused, in the CLI's words (`rule`); `None` when the stream did not say.
    pub reason: Option<String>,
}

/// The refused tool calls of a session that ended, in the order its stream listed them. Keyed
/// by (attempt, spawn, position): writing it again adds nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDenials {
    pub item: ItemId,
    pub attempt: u32,
    /// The session's number, as in its [`RunStart`].
    pub spawn: u32,
    pub denials: Vec<DeniedCall>,
}

/// What a worktree is for (#8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// The item's own checkout, where its sessions run.
    Item,
    /// A throwaway checkout a verdict ran in.
    Verdict,
}

/// Where a worktree stands (#8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeState {
    /// On disk.
    Created,
    /// Removed.
    Removed,
    /// Its removal was blocked; it is kept, with the file that blocked it and the error.
    Quarantined,
}

impl WorktreeKind {
    /// Its name as stored: `item`, `verdict`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        codec::worktree_kind(self)
    }
}

impl WorktreeState {
    /// Its name as stored: `created`, `removed`, `quarantined`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        codec::worktree_state(self)
    }
}

/// A pull request's state, as the forge reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// A git worktree the daemon made for an item (#8). Keyed by its path: a path is never reused,
/// so writing it again stores nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorktree {
    pub item: ItemId,
    pub path: String,
    pub branch: String,
    /// The commit it was made from.
    pub base_sha: String,
    pub kind: WorktreeKind,
    /// The user's checkout as it was at claim (E4), as JSON; `None` for a verdict checkout.
    pub user_base_json: Option<String>,
    pub created_ms: i64,
}

/// A session shim the daemon started (#8). Keyed by the path of its record: writing it again
/// stores nothing. Its output offsets start at zero and are the daemon's (E18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProcess {
    pub item: ItemId,
    pub attempt: u32,
    /// The session's number, as in its [`RunStart`].
    pub spawn: u32,
    /// The provider session id the session was given.
    pub session_key: String,
    /// The shim's process (#50: pid and start time identify it).
    pub pid: u32,
    pub pid_started_ms: i64,
    /// Where the shim's record is.
    pub record_path: String,
    /// The digest of the agent credential the session was issued.
    pub credential_digest: Option<Vec<u8>>,
    pub started_ms: i64,
}
