//! The loop's tables (#7, #57): items, snapshots, attempts, run sessions, verdict runs, the
//! transition log, the outbox, the agent's calls, board comments and flaky tests.
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
#[cfg(test)]
mod tests;
pub(crate) mod write;

pub use read::{
    AgentCallRow, AttemptRow, OutboxOp, OutboxRow, RecordedChild, RunRow, StoredItem,
    TransitionRow, VerdictRow,
};

use bosun_core::{Exit, ItemId, RunResult, SpawnMode};

/// A new item, or new metadata for a known one. Keyed by the item id: writing it again updates
/// the card key and leaves the state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItem {
    pub item: ItemId,
    /// The repository's canonical path key; registered if it is not yet.
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
