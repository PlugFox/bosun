//! The `item.*` and `session.*` methods (#4, #70): one item's detail, restart-item, and the
//! sessions bosun spawned.
//!
//! Item ids are the daemon's: `<repo>#<card>`, the repository's name in the configuration and
//! the card's number on its board. Session ids are the provider's, as bosun assigned them at
//! spawn.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RepoRef;

/// Params of `item.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ItemParams {
    pub identity: String,
    /// The item id, `<repo>#<card>`.
    pub item: String,
}

/// Result of `item.get`: everything the store holds of one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ItemDetail {
    pub identity: String,
    pub item: String,
    pub repo: RepoRef,
    /// Its state now (`ready`, `claimed`, `running`, `parked`, `verifying`, `landing`, `done`,
    /// `needs_you`, `cancelled`).
    pub state: String,
    /// The park reason, outcome or needs-you reason of the state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The current attempt; 0 before the first.
    pub attempt: u32,
    /// Attempts that failed and counted.
    pub failed: u32,
    pub created_ms: i64,
    /// When it entered its state.
    pub updated_ms: i64,
    /// While its lease request is not granted: the full resource instances it waits for, as
    /// in [`crate::CurrentItem::waiting`]. Absent when it waits for nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<crate::WaitingFor>,
    /// Its open `blocked-by` blockers while it waits in Ready on them (#5). Absent unless it
    /// waits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<crate::OpenBlocker>,
    /// The resources it would lease whose probe fails, while it waits in Ready on them (#205),
    /// as its own identity probed them. Absent unless it waits on a probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unhealthy: Vec<crate::UnhealthyResource>,
    /// The snapshot its next attempt runs from, once claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotView>,
    /// Every arrow it took, in order.
    pub transitions: Vec<TransitionView>,
    /// Its attempts, by number.
    pub attempts: Vec<AttemptView>,
    /// What its agents said through `bosun-mcp` (questions, summaries, notes), in order.
    pub calls: Vec<AgentCallView>,
}

/// The snapshot of an item (#5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotView {
    pub hash: String,
    pub taken_ms: i64,
    /// The normalised body the agent and the verdict were given.
    pub body: String,
    /// The commit the item started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// The branch HEAD was on at claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The model its sessions ask for (`--model`): the card's, else the repository's default
    /// (#133). Absent from a snapshot taken before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One arrow of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TransitionView {
    /// From 1 within the item.
    pub seq: u32,
    pub attempt: u32,
    pub from: String,
    pub to: String,
    /// The park reason, outcome or reason of `to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_to: Option<String>,
    /// The event that caused it.
    pub cause: String,
    pub ts_ms: i64,
}

/// One attempt of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttemptView {
    pub number: u32,
    /// `spawned`, `exited`, `verdict_running`, `pass`, `fail`, ...
    pub state: String,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// The hash of the snapshot it ran from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_hash: Option<String>,
    /// Its sessions, in order.
    pub sessions: Vec<SessionInfo>,
    /// Its verdict runs, in order.
    pub verdicts: Vec<VerdictView>,
    /// Why the daemon refused it before its verdict (#137): its failed preconditions or guard
    /// refusals, in the daemon's words; empty when nothing refused it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusal: Vec<String>,
}

/// One verdict run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerdictView {
    pub run: u32,
    /// The command that ran: the snapshot's verdict.
    pub command: String,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// `pass`, `fail` or `timeout`; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// The line of the verdict's output that says what it found, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_line: Option<String>,
}

/// One call of an item's agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCallView {
    pub attempt: u32,
    /// `question`, `blocking_question`, `summary` or `note`.
    pub kind: String,
    pub body: String,
    /// The call was over its tool's cap and refused.
    pub refused: bool,
    pub ts_ms: i64,
}

/// One session bosun spawned: in [`AttemptView::sessions`], and the result of `session.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub item: String,
    pub attempt: u32,
    /// The session's number within the item: the `spawn` of its events.
    pub spawn: u32,
    /// The provider session id bosun assigned (a resume keeps it).
    pub session: String,
    pub provider: String,
    /// `fresh`, `resume` or `reset`.
    pub mode: String,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// `ok`, `limit`, `auth` or `crash`; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
    /// It has not ended.
    pub running: bool,
}

/// Params of `session.get`: exactly one of `session` and `item`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionParams {
    pub identity: String,
    /// A provider session id; with a resume, its latest spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// An item id: its latest session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// Params of `item.restart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestartParams {
    /// The operator token the daemon wrote at start (see [`crate::control`]). Missing, it is
    /// refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    pub identity: String,
    /// The item id, `<repo>#<card>`; it must be in flight.
    pub item: String,
}

/// Result of `item.restart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestartResult {
    pub item: String,
    /// The label put on the card: the loop kills the session, takes a new snapshot and
    /// starts again, then removes it (#5).
    pub label: String,
}

/// Params of `session.tail` (#74): the live tail of an item's sessions, as the daemon still
/// holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TailParams {
    pub identity: String,
    /// The item id, `<repo>#<card>`.
    pub item: String,
    /// Only the lines with this sequence or a later one: a client that has seen up to `n` asks
    /// for `n + 1`. Absent: every line still held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
}

/// Result of `session.tail`: the item's output lines still in the identity's event ring, its
/// latest session and that session's context size.
///
/// The lines are contiguous: every `session_output` event of the item from the first one
/// returned up to `head` is in [`SessionTail::lines`]. A client that refetches after a resync
/// replaces what it showed with them and appends the events after `head`, with no gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionTail {
    pub identity: String,
    pub item: String,
    /// The daemon's epoch: the sequences are of this run of the daemon.
    pub epoch: String,
    /// The sequence of the last event published when the lines were read; 0 before the first.
    pub head: u64,
    /// The `session_output` events of the item, oldest first.
    pub lines: Vec<crate::Event>,
    /// The ring no longer holds `from_seq`: lines after the client's last were dropped, and
    /// `lines` are every line still held, as with no `from_seq`. The client replaces what it
    /// showed with them rather than appending.
    pub gap: bool,
    /// Earlier lines of the latest session are no longer held: the ring moved past them.
    pub truncated: bool,
    /// The item's latest session; absent before its first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    /// The context size of that session's main transcript, from the ingested transcript;
    /// absent until a message of it is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextSize>,
}

/// The prompt size of a context window at its latest message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextSize {
    /// `input + cache read + cache write 5 min + cache write 1 h` of the latest message, as in
    /// [`crate::ContextCurve`].
    pub prompt: u64,
    /// The largest prompt size of the window so far.
    pub peak: u64,
    /// The model of the latest message.
    pub model: String,
    pub ts_ms: i64,
    /// The provider's nominal context window, when bosun knows it: the gauge's full scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
}
