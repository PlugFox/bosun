//! The types of `report.exact` (#6, "The report"; #26): the exact sections of the report.
//!
//! Every number here is exact: a raw token count, a sum of them, or a cost and eqv computed from
//! them with the stored prices. Estimates (cost by tool and by file, the cost of finding, the
//! context curve) are another method (#27) and never appear here. The report prints numbers; it
//! draws no conclusions.
//!
//! Scopes differ by section, and each type says which it has:
//!
//! - **range and repository**: the messages with `from_ms <= ts < to_ms`, and, with a
//!   repository given, only those whose working directory is in it: [`Report::total`],
//!   [`Report::by_category`], [`Report::by_model`], [`Report::by_kind`],
//!   [`Report::old_method`], [`Report::per_commit`];
//! - **range only**: the sessions the agreement compares ([`Agreement`]);
//! - **the whole store**: the ingest counts of [`Health`].

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
    /// The ingest's counts and the agreement with the provider's own totals.
    pub health: Health,
    /// Models with no price at the time of some messages of the scope: their tokens are in
    /// every count, their cost and eqv in none.
    pub unpriced: Vec<UnpricedModel>,
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
/// of each session): for each root session that has them and a message in the range, the
/// token sums of all its messages, main transcript and sub-agents, over all time (the
/// provider's totals are cumulative), against the provider's.
///
/// A session is within the tolerance when `|transcripts - provider| * 100 <= tolerance *
/// provider` on the total of the four categories; the categories are shown for reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Agreement {
    /// The tolerance, in percent.
    pub tolerance_percent: u64,
    /// Sessions compared.
    pub sessions: u64,
    /// Sessions within the tolerance.
    pub within: u64,
    /// The sums over every compared session.
    pub total: Comparison,
    /// One row per compared session, sorted by session.
    pub rows: Vec<SessionAgreement>,
}

/// One session of [`Agreement`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionAgreement {
    /// The provider's session id.
    pub session: String,
    /// Context windows with messages: the main transcript and each sub-agent.
    pub windows: u64,
    /// Messages over those windows.
    pub messages: u64,
    pub comparison: Comparison,
}

/// The transcripts' sums against the provider's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Comparison {
    pub transcripts: CheckTokens,
    pub provider: CheckTokens,
    /// Whether the totals agree within the tolerance.
    pub within: bool,
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
