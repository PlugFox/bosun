//! The types of `report.estimates` (#6, "The report"; #3, "Tool and file cost"; #27): the
//! estimate sections of the report.
//!
//! **Every number here is an estimate**, and none of them is ever added to a figure of
//! `report.exact` ([`crate::Report`]). Each section carries `estimate: true` and its formula
//! as text, so a reader of the JSON alone sees what it is.
//!
//! All the sections price one thing: the result of a tool call as it sits in the context
//! window ([`ResultCost`]). The provider reports no per-call usage, so a result's tokens are
//! estimated from its stored size in bytes with one global ratio
//! ([`Estimates::bytes_per_token`]), and its cost follows the cache: written once by the first
//! message after the call, then read from the cache by every later message of the window up to
//! the next compaction (#3: most of the bill is cache read).
//!
//! The scope is that of the range sections of the exact report: the messages with
//! `from_ms <= ts < to_ms`, and, with a repository given, those in it. A result carried past
//! the end of the range is carried only to the end of the range.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Picodollars, RepoRef};

/// Result of `report.estimates`. Its params are those of `report.exact`
/// ([`crate::ReportParams`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Estimates {
    pub identity: String,
    /// The range asked for.
    pub from_ms: i64,
    pub to_ms: i64,
    /// The repository asked for; `null` for every message of the range.
    pub repo: Option<RepoRef>,
    /// The model every eqv is measured against.
    pub reference_model: String,
    /// Always `true`: every number of this result is an estimate.
    pub estimate: bool,
    /// The global ratio of bytes to tokens: a result of `b` bytes is `ceil(b / ratio)` tokens.
    pub bytes_per_token: u64,
    /// How a result is priced, as text.
    pub formula: String,
    /// What the estimates are known to get wrong, as text.
    pub known_gaps: Vec<String>,
    pub by_tool: ByTool,
    pub by_file: ByFile,
    pub repeated_reads: RepeatedReads,
    pub finding: Finding,
}

/// The estimated cost of a set of tool results in the context.
///
/// For one call made by message `i` of a window, with `t = ceil(result_bytes /
/// bytes_per_token)`: the messages after `i` up to the first compaction after `i` (a drop of
/// the prompt size, as in [`crate::ContextCurve`]) carry the result. The first of them writes
/// it to the cache at its cache-write rate (the 1 h rate when that message wrote more 1 h than
/// 5 min tokens, else the 5 min rate); each later one reads it at its cache-read rate:
///
/// `cost = t * write rate(i + 1) + sum over carrying messages j > i + 1 of t * cache read rate(j)`
///
/// A result with no message after it before a compaction costs nothing here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResultCost {
    /// Tool calls.
    pub calls: u64,
    /// Their results' stored size.
    pub result_bytes: u64,
    /// Their results' estimated tokens, `ceil(bytes / bytes_per_token)` per call, summed.
    pub tokens: u64,
    /// The messages that read the results from the cache, summed over calls: how long they
    /// stayed in context.
    pub carry_turns: u64,
    /// The first writes, in eqv.
    pub write_eqv: f64,
    /// The cache reads after the first write, in eqv.
    pub carry_eqv: f64,
    /// The cost of both.
    pub cost: Picodollars,
    /// `cost` in eqv.
    pub eqv: f64,
}

/// Cost by tool: the results of every tool call of the scope, per tool name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ByTool {
    /// Always `true`.
    pub estimate: bool,
    pub formula: String,
    /// Every call of the scope.
    pub total: ResultCost,
    /// One row per tool name, by cost descending, then name.
    pub rows: Vec<ToolRow>,
}

/// One tool of [`ByTool`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolRow {
    pub name: String,
    /// Calls whose result was an error.
    pub errors: u64,
    pub results: ResultCost,
}

/// Cost by file: the results of the calls that touched each file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ByFile {
    /// Always `true`.
    pub estimate: bool,
    pub formula: String,
    /// One row per file, by cost descending, then repository and path.
    pub rows: Vec<FileRow>,
}

/// One file of [`ByFile`]. A search touches the directory it searched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileRow {
    /// The repository of the calling message; `null` outside every repository.
    pub repo: Option<RepoRef>,
    /// Relative to the repository (`.` for its root); absolute outside one.
    pub path: String,
    /// Calls that read, searched and wrote it.
    pub reads: u64,
    pub searches: u64,
    pub writes: u64,
    /// Root sessions with a call that touched it; a sub-agent counts with its session.
    pub sessions: u64,
    /// The results of every call that touched it.
    pub results: ResultCost,
}

/// Repeated reads: files read more than once, within a session and across sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepeatedReads {
    /// Always `true`.
    pub estimate: bool,
    pub formula: String,
    /// Every row's `within`, summed.
    pub within: ResultCost,
    /// Every row's `across`, summed.
    pub across: ResultCost,
    /// One row per file read more than once, by the cost of its repeats descending, then
    /// repository and path.
    pub rows: Vec<RepeatedRead>,
}

/// One file of [`RepeatedReads`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RepeatedRead {
    pub repo: Option<RepoRef>,
    pub path: String,
    /// Read calls.
    pub reads: u64,
    /// Root sessions that read it.
    pub sessions: u64,
    /// Every read of a session after that session's first read of the file.
    pub within: ResultCost,
    /// The first read of the file in each session after the first session that read it.
    pub across: ResultCost,
}

/// The cost of finding: the read and search calls of a root session before its first write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// Always `true`.
    pub estimate: bool,
    pub formula: String,
    /// Sessions with a write: their reads and searches before it.
    pub sessions_with_write: u64,
    pub before_write: ResultCost,
    /// Sessions with reads or searches and no write: all of them.
    pub sessions_without_write: u64,
    pub without_write: ResultCost,
    /// One row per root session with a read or search, sorted by session.
    pub rows: Vec<FindingRow>,
}

/// One root session of [`Finding`]; its sub-agents' calls count with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindingRow {
    pub session: String,
    /// The time of the message that made its first write call; `null` without one.
    pub first_write_ms: Option<i64>,
    /// Its read and search calls before that write (all of them without one).
    pub results: ResultCost,
}
