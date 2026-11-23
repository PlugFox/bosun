//! The types of `report.exact` (#6, "The report"; #26): the exact sections of the report.
//!
//! Every number here is exact: a raw token count, a sum of them, or a cost and eqv computed from
//! them with the stored prices. Estimates (cost by tool and by file, repeated reads, the cost of
//! finding) are another method, `report.estimates` (#27, [`crate::Estimates`]), and never
//! appear here. The report prints numbers; it draws no conclusions.
//!
//! The context curve ([`ContextCurve`]) is here, not with the estimates: its numbers are the
//! measured prompt sizes of messages (input plus cache reads and writes, from the provider's
//! usage), their maximum, and a count of drops between consecutive messages. Nothing in it is
//! converted from bytes or apportioned, and it holds no cost, so no total includes it.
//!
//! Scopes differ by section, and each type says which it has:
//!
//! - **range and repository**: the messages with `from_ms <= ts < to_ms`, and, with a
//!   repository given, only those whose working directory is in it: [`Report::total`],
//!   [`Report::by_category`], [`Report::by_model`], [`Report::by_kind`],
//!   [`Report::old_method`], [`Report::per_commit`], [`Report::context`];
//! - **range only**: the sessions the agreement compares ([`Agreement`]);
//! - **the whole store**: the ingest counts of [`Health`];
//! - **the items of the range**: the loop's sections ([`Report::items`], [`crate::Items`]),
//!   whose items are counted over their whole history.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ModelRow, Picodollars, RepoRef, TokenSums, UnpricedModel};

/// Params of `report.exact`: an identity, a half-open time range and an optional repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReportParams {
    pub identity: String,
    /// Messages at or after this time. The previous period of the per-commit trend is the
    /// range of the same length that ends here.
    pub from_ms: i64,
    /// Messages before this time. Must be after `from_ms`.
    pub to_ms: i64,
    /// A repository's name in the configuration: the sections of the range and repository
    /// scope count its messages and commits only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Result of `report.exact`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub identity: String,
    /// The range asked for.
    pub from_ms: i64,
    pub to_ms: i64,
    /// The repository asked for; `null` for every message of the range.
    pub repo: Option<RepoRef>,
    /// The model every eqv is measured against.
    pub reference_model: String,
    /// The real total: every message of the scope, each counted once.
    pub total: TokenSums,
    /// What the old method would have reported for the same messages, shown once next to
    /// `total`.
    pub old_method: OldMethod,
    /// `total` by token category, in the order input, cache write 5 min, cache write 1 h,
    /// cache read, output. The categories sum to `total` exactly.
    pub by_category: Vec<CategoryRow>,
    /// `total` by model, sorted by model.
    pub by_model: Vec<ModelRow>,
    /// `total` by the kind of the root session, `interactive` then `run`; both are always
    /// present.
    pub by_kind: Vec<KindRow>,
    /// Eqv per commit, per repository, against the previous period.
    pub per_commit: PerCommit,
    /// The context curve of each context window.
    pub context: ContextCurve,
    /// The ingest's counts and the agreement with the provider's own totals.
    pub health: Health,
    /// Models with no price at the time of some messages of the scope: their tokens are in
    /// every count, their cost and eqv in none.
    pub unpriced: Vec<UnpricedModel>,
    /// The loop's sections (#72): items and attempts with their runs' exact cost, retries,
    /// flaky verdicts, refused edits, time parked and the runs' commits. Their scope is said
    /// in [`crate::Items`]; no other section includes them or is included in them.
    #[serde(default)]
    pub items: crate::Items,
}

/// The old method: per session, only the last usage block of its main transcript, as the
/// scripts bosun replaces summed it. It ignores every earlier message and every sub-agent.
///
/// Over the messages of the scope: for each root session with a main-transcript message in
/// it, the last such message (by time, then message id); their sums.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OldMethod {
    /// Sessions that contributed a last block.
    pub sessions: u64,
    /// The sums of those last blocks; `messages` equals `sessions`.
    pub sums: TokenSums,
}

/// One token category of [`Report::by_category`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CategoryRow {
    /// `input`, `cache_write_5m`, `cache_write_1h`, `cache_read` or `output`.
    pub category: String,
    /// The category's tokens.
    pub tokens: u64,
    /// The category's tokens at the rates of the row that prices each message (the row is
    /// chosen by the message's whole prompt size).
    pub cost: Picodollars,
    /// `cost` in input-equivalent tokens of the reference model, rounded for display.
    pub eqv: f64,
}

/// One session kind of [`Report::by_kind`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KindRow {
    /// `interactive` or `run`: the kind of the root session; a sub-agent counts with its
    /// session.
    pub kind: String,
    /// Root sessions of this kind with a message in the scope.
    pub sessions: u64,
    pub sums: TokenSums,
}

/// Eqv per commit: the eqv of the messages attributed to a repository divided by the number
/// of its commits, both in one period. Both operands are exact; no message is linked to a
/// commit (that link is an estimate).
///
/// Commits are those the commit reader stored: reachable from the repository's local and
/// remote-tracking branches, counted by committer time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PerCommit {
    /// The previous period: the range of the same length that ends at the report's `from_ms`.
    pub previous_from_ms: i64,
    pub previous_to_ms: i64,
    /// One row per repository with messages or commits in either period, sorted by path, then
    /// the messages in no repository (`repo` null, never any commit).
    pub rows: Vec<CommitRow>,
    /// The sums over the rows of repositories, without the messages in no repository.
    pub attributed: Trend,
    /// The commit reader's state for each configured repository of the scope.
    pub scans: Vec<CommitScan>,
}

/// One repository of [`PerCommit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommitRow {
    /// `null` for the messages in no repository.
    pub repo: Option<RepoRef>,
    pub trend: Trend,
}

/// A period and the previous one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Trend {
    pub current: CommitPeriod,
    pub previous: CommitPeriod,
    /// `current.eqv_per_commit / previous.eqv_per_commit - 1`; `null` when either is null.
    pub change: Option<f64>,
}

/// The operands of eqv per commit in one period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommitPeriod {
    pub messages: u64,
    pub commits: u64,
    pub cost: Picodollars,
    pub eqv: f64,
    /// `eqv / commits`; `null` without commits.
    pub eqv_per_commit: Option<f64>,
}

/// The context curve per context window (#6): its prompt size at the start, its peak, and its
/// compactions.
///
/// The prompt size of a message is `input + cache read + cache write 5 min + cache write 1 h`
/// of its usage. A compaction is a drop of the prompt size between consecutive messages of one
/// window (#3): `prompt(next) < prompt(previous)`, messages ordered by time, then id. Over the
/// messages of the scope only: a window's messages before the range are not seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextCurve {
    /// Windows with a message in the scope.
    pub windows: u64,
    /// Compactions over every window.
    pub compactions: u64,
    /// One row per window, sorted by session, then the main transcript before its sub-agents
    /// by agent id.
    pub rows: Vec<WindowCurve>,
}

/// One context window of [`ContextCurve`]: a session's main transcript or one sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowCurve {
    /// The provider's session id.
    pub session: String,
    /// The sub-agent id; `null` for the main transcript.
    pub agent: Option<String>,
    pub messages: u64,
    /// The prompt size of the window's first message in the scope.
    pub start: u64,
    /// The largest prompt size of its messages in the scope.
    pub peak: u64,
    /// The drops of the prompt size between its consecutive messages.
    pub compactions: u64,
}

/// The commit reader's last scan of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitScan {
    pub repo: RepoRef,
    pub state: ScanState,
}

/// The outcome of a repository's last commit scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScanState {
    /// Not scanned since the daemon started.
    Pending,
    /// Scanned; its commits are stored.
    Done,
    /// The scan failed; the commits stored before stay.
    Failed { error: String },
}

/// Ingest health. The counts are over the whole store; the agreement over the sessions of
/// the range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    /// Source files: transcripts, and meta files that could not be read.
    pub files: u64,
    /// Of them, files whose lines were counted. A file parsed before lines were counted
    /// has no count until it is re-read from zero (`bosun reingest`).
    pub files_counted: u64,
    /// The complete lines of the counted files.
    pub lines: u64,
    /// Lines that could not be read as expected (`ingest_gap`).
    pub gaps: u64,
    /// The same counts per parser version, ascending. A file counts under the version of its
    /// last parse, a gap under the version that wrote it.
    pub versions: Vec<VersionRow>,
    pub agreement: Agreement,
}

/// The counts of one parser version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VersionRow {
    pub parser_version: u32,
    pub files: u64,
    pub files_counted: u64,
    pub lines: u64,
    pub gaps: u64,
}

/// The agreement with the provider's own totals (Claude Code's `cost-state`, the last record
/// of each root transcript): for each root session that has them and a message in the range,
/// the token sums of its messages inside the provider's window, main transcript and
/// sub-agents, against the provider's (#46).
///
/// The provider's window runs from when its counter started (`startTime`: the counter starts
/// again when a session is resumed in a new process) to the timestamp of the last record
/// before the `cost-state` line (records after it are not in it), both inclusive. A check
/// stored before the window was (#46) has an open window until `bosun reingest`.
///
/// A session is within the tolerance when `|transcripts - provider| * 100 <= tolerance *
/// provider` on the total of the four categories, and exceeds it when `(transcripts -
/// provider) * 100 > tolerance * provider`. The criterion is one-sided: the provider also
/// counts calls the transcripts never hold, so it may be larger ([`Undercount`]); the
/// transcripts may not. The categories are shown for reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Agreement {
    /// The tolerance, in percent.
    pub tolerance_percent: u64,
    /// Sessions compared.
    pub sessions: u64,
    /// Sessions within the tolerance.
    pub within: u64,
    /// Sessions whose transcripts exceed the provider by more than the tolerance.
    pub exceeding: u64,
    /// Sessions with the provider's totals and a message in the range but no message inside
    /// the provider's window: not compared, and not a mismatch.
    pub excluded: u64,
    /// The sums over every compared session.
    pub total: Comparison,
    /// The models of the provider's totals that no compared session's transcripts hold inside
    /// the window, sorted by model.
    pub absent_models: Vec<AbsentModel>,
    /// One row per compared session, sorted by session.
    pub rows: Vec<SessionAgreement>,
}

/// One session of [`Agreement`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionAgreement {
    /// The provider's session id.
    pub session: String,
    /// Context windows with messages inside the provider's window: the main transcript and
    /// each sub-agent.
    pub windows: u64,
    /// Messages inside the provider's window over those windows.
    pub messages: u64,
    pub comparison: Comparison,
}

/// The transcripts' sums against the provider's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Comparison {
    pub transcripts: CheckTokens,
    pub provider: CheckTokens,
    /// `transcripts.total - provider.total`: negative when the provider is larger.
    pub difference: i64,
    /// Whether the totals agree within the tolerance.
    pub within: bool,
    /// Whether the transcripts exceed the provider by more than the tolerance.
    pub exceeds: bool,
    /// The provider's excess over the transcripts, split by model kind.
    pub undercount: Undercount,
}

/// The provider's excess over the transcripts (`-difference`), split by model kind: the
/// known undercount of the transcript method, which cannot see calls its transcripts never
/// hold. `absent_models + shared_models = provider.total - transcripts.total`.
///
/// Models are matched by name without a bracketed suffix: Claude Code's `[1m]` is a label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Undercount {
    /// The provider's total of the models the transcripts hold no message of inside the
    /// window (side calls such as Haiku's).
    pub absent_models: u64,
    /// The rest: the provider's total of the other models less the transcripts' total. It is
    /// negative when the transcripts are larger on them.
    pub shared_models: i64,
}

/// A model of the provider's totals that the transcripts do not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AbsentModel {
    /// The model id as the provider names it, without a bracketed suffix.
    pub model: String,
    /// Compared sessions whose provider totals have it and whose transcripts do not.
    pub sessions: u64,
    /// The provider's total of the four categories for it over those sessions.
    pub total: u64,
}

/// Token sums in the provider's categories: cache writes are not split by duration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckTokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    /// The four categories summed.
    pub total: u64,
}
