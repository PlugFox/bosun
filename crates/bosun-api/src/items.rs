//! The loop's sections of the report (#7, #72): items, attempts, retries, flaky verdicts,
//! refused protected-path edits, time parked, and the commits of the runs.
//!
//! An item's cost is two numbers that are never summed (#3, "The bill"):
//!
//! - **runs**, exact: the messages of the sessions bosun spawned for the item, each session with
//!   its sub-agents, over the item's whole history ([`Items`], in `report.exact`);
//! - **inferred sessions**, an estimate: interactive sessions linked to the item by a rule
//!   ([`InferredItems`], in `report.estimates`). Their token counts are exact; that they belong
//!   to the item is not.
//!
//! The link of a commit to its session and item is exact: a commit is a run's when the
//! session's recorded commit range (`base..head`, as git listed it when the session ended)
//! holds it, never by time.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ModelRow, RepoRef, TokenSums};

/// The exact loop sections of `report.exact`.
///
/// An item is in them when one of its transitions is in the range (and, with a repository
/// given, it is that repository's). Its numbers cover its whole history, not only the range:
/// an item's cost is not cut at the range's edges. The commits are those of the range.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Items {
    /// One row per item, sorted by item id.
    pub rows: Vec<ItemRow>,
    /// The items with more than one attempt, sorted by item id.
    pub retried: Vec<RetriedItem>,
    /// The flaky verdicts (fail, then pass on the re-run) with their failing tests (#44 D), by
    /// item, then attempt.
    pub flaky: Vec<FlakyVerdict>,
    /// The attempts a guard refused because their commits touched a protected path, by time.
    pub refused_edits: Vec<RefusedEdit>,
    /// The time parked over every row, by reason (sorted by reason).
    pub parked: Vec<ParkedTime>,
    /// The commits of the range that a run's commit range holds, oldest first: each maps to
    /// one session and one item.
    pub commits: Vec<RunCommit>,
    /// The commits of `commits` that lack one of the trailers `Item:`, `Run:`, `Session:`, in
    /// the same order. The link above stays exact without them.
    pub trailer_missing: Vec<TrailerMissing>,
    /// The commits of the range, in the report's repositories, that no run's range holds:
    /// made outside the runs.
    pub outside_runs: u64,
    /// The granted lease requests filed in the range, per resource instance, longest wait
    /// first (#8: the waiting time is the measured cost of contention). With a repository
    /// given, its items' requests only.
    #[serde(default)]
    pub lease_waits: Vec<LeaseWaits>,
    /// The worktrees on disk or quarantined now, whatever the range (of the report's
    /// repository when it names one), in the order they were made.
    #[serde(default)]
    pub worktrees: Vec<WorktreeUse>,
}

/// The granted waits for one resource instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LeaseWaits {
    /// The instance: `<name>`, `<name>/<identity>` or `<name>/<repo>`.
    pub resource: String,
    /// Granted requests.
    pub requests: u64,
    /// `granted_ms - filed_ms`, summed.
    pub total_ms: u64,
    /// The longest of them.
    pub longest_ms: u64,
}

/// A worktree on disk or quarantined, with its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeUse {
    pub item: String,
    pub path: String,
    /// `item` or `verdict`.
    pub kind: String,
    /// `created` or `quarantined`.
    pub state: String,
    /// Its size on disk when it was last measured; `null` when it was never measured.
    pub bytes: Option<u64>,
    /// A quarantined worktree: the file that blocked its removal.
    pub blocking: Option<String>,
}

/// One item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ItemRow {
    /// The item's id.
    pub item: String,
    pub repo: RepoRef,
    /// Its state (`bosun_core::State::as_str`) now.
    pub state: String,
    /// The state's park reason, outcome or needs-you reason.
    pub detail: Option<String>,
    /// Attempts that failed and counted.
    pub failed: u32,
    /// Runs, exact: the messages of every session bosun spawned for the item, main transcripts
    /// and sub-agents. Equal to the sum of its attempts' `sums`.
    pub runs: TokenSums,
    /// Distinct run sessions (a resume keeps its session).
    pub sessions: u64,
    /// The wall-clock time of its attempts that ended, summed.
    pub wall_clock_ms: u64,
    /// Its time parked by reason, sorted by reason.
    pub parked: Vec<ParkedTime>,
    /// By number.
    pub attempts: Vec<AttemptReport>,
}

/// One attempt of an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AttemptReport {
    pub number: u32,
    /// `bosun_core::AttemptState::as_str`.
    pub state: String,
    pub started_ms: i64,
    /// When it reached pass, fail, flaky or timeout; `null` before.
    pub ended_ms: Option<i64>,
    /// `ended_ms - started_ms`; `null` while it has not ended.
    pub wall_clock_ms: Option<u64>,
    /// Fresh sessions after a crash within the attempt.
    pub crash_retries: u32,
    /// Calls of the agent refused over their rate cap.
    pub refused_calls: u64,
    /// The messages of its sessions, exact.
    pub sums: TokenSums,
    /// `sums` by model, sorted by model.
    pub models: Vec<ModelRow>,
    /// Its sessions, in order.
    pub sessions: Vec<RunSession>,
}

/// One session bosun spawned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunSession {
    /// The session's number within the item.
    pub spawn: u32,
    /// `fresh`, `resume` or `reset`.
    pub mode: String,
    pub provider: String,
    /// The provider session id bosun assigned.
    pub session: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    /// `ok`, `limit`, `auth` or `crash`; `null` while it runs.
    pub exit: Option<String>,
    /// HEAD when it started and ended, once its range is recorded.
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// The messages of this spawn's session id. A resume shares the id of the session it
    /// resumes, so its messages are counted once, under the first spawn of the id.
    pub sums: TokenSums,
}

/// An item that took more than one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetriedItem {
    pub item: String,
    pub attempts: u32,
    pub failed: u32,
}

/// A flaky verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlakyVerdict {
    pub item: String,
    pub attempt: u32,
    /// The tests that failed on the first run, sorted; empty when the parser found none.
    pub tests: Vec<String>,
}

/// An attempt refused because its commits touched a protected path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RefusedEdit {
    pub item: String,
    pub attempt: u32,
    pub ts_ms: i64,
}

/// Time parked for one reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ParkedTime {
    /// `limit`, `paused`, `killed` or `daily_cap`.
    pub reason: String,
    /// Times parked.
    pub parks: u64,
    /// From each park to the next transition; a park still open counts up to the report's
    /// `to_ms`.
    pub ms: u64,
    /// Whether a park is still open.
    pub ongoing: bool,
}

/// A commit of a run, exactly linked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunCommit {
    pub repo: RepoRef,
    pub sha: String,
    /// Committer time.
    pub ts_ms: i64,
    pub item: String,
    pub attempt: u32,
    /// The session's number within the item.
    pub spawn: u32,
    /// The provider session id of the session that made it.
    pub session: String,
    /// Which of the daemon's trailers it carries.
    pub trailers: Trailers,
}

/// The daemon's trailers on a commit: present or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Trailers {
    pub item: bool,
    pub run: bool,
    pub session: bool,
}

/// A run's commit without some of the daemon's trailers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrailerMissing {
    pub repo: RepoRef,
    pub sha: String,
    pub item: String,
    /// The missing trailer names: `Item`, `Run`, `Session`.
    pub missing: Vec<String>,
}

/// The inferred sessions of items, in `report.estimates`: **an estimate**, never added to an
/// item's runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InferredItems {
    /// Always `true`.
    pub estimate: bool,
    /// The rule that links a session to an item, as text.
    pub rule: String,
    /// One row per item with an inferred session, sorted by item id.
    pub rows: Vec<InferredItem>,
}

impl Default for InferredItems {
    fn default() -> Self {
        Self {
            estimate: true,
            rule: String::new(),
            rows: Vec::new(),
        }
    }
}

/// The interactive sessions inferred to have worked on one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InferredItem {
    pub item: String,
    pub repo: RepoRef,
    /// Sorted by session.
    pub sessions: Vec<InferredSession>,
    /// The sums over `sessions`. A session inferred for several items counts in each.
    pub sums: TokenSums,
}

/// One interactive session inferred for an item, with the evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InferredSession {
    pub provider: String,
    pub session: String,
    pub evidence: Vec<Evidence>,
    /// Every message of the session, main transcript and sub-agents, over all time.
    pub sums: TokenSums,
}

/// Why a session is inferred to belong to an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Evidence {
    /// The session is linked by time and repository (the inferred commit link of #3: one of
    /// its windows spans the commit's time, from its first to its last message, and has
    /// messages in the commit's repository) to a commit outside every run's range whose
    /// `Item:` trailer names the item.
    CommitTrailer { sha: String },
}
