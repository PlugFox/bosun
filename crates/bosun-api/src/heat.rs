//! The types of `heat.metrics` (#11, part D): the generic metrics of a repository that the
//! heat detectors test, computed at query time from the stored rows (#3).
//!
//! A metric is one figure of one group: a repository, a provider and, for the metrics of
//! sessions, a session kind. Two providers' rows are never folded into one figure: a group's
//! key always holds its provider, and a before/after comparison is made within one provider.
//! Only messages in a repository count. A metric whose sample is empty has no row.
//!
//! Each row says what it is: `value` is the normalized figure in `unit` (integer division,
//! rounded down; a share is in parts per million, `total * 1_000_000 / sample`), `total` its
//! numerator in its base unit, `sample` its denominator in `sample_unit`. `estimate` is `true`
//! for a figure guessed from bytes, and `runner_comparable` says whether it can be compared
//! across providers.
//!
//! A root session is a provider's session id with its sub-agents; it belongs to every
//! repository it has a message in, and its figures there count the messages and tool calls of
//! that repository. An item is closed when its verdict passed (`pass` or `flaky`; a card the
//! user closed has no verdict) and its last arrow to `done` is in the range; its provider is
//! that of its last session.
//!
//! | `id` | `unit` | `sample_unit` | What it is | Estimate | Comparable |
//! | --- | --- | --- | --- | --- | --- |
//! | `eqv_per_commit` | `eqv_tokens_per_commit` | `commit` | The eqv of the provider's `run` messages, rounded down to a whole token, over the distinct commits its sessions' commit ranges list. Not the report's eqv per commit, which folds every message and commit of the repository. | no | yes |
//! | `context_at_start` | `tokens` | `session` | The prompt size of the first message of each session's main transcript: `value` is their lower median, `total` their sum. | no | no |
//! | `rereads_per_session` | `reads_per_session` | `session` | Per session and file, the reads after its first one. Exec, unknown and search touches are not reads. | no | yes |
//! | `finding_bytes_per_session` | `bytes_per_session` | `session` | The result bytes of the read and search calls before the session's first write call (every one without a write). | no | yes |
//! | `finding_tokens_per_session` | `estimated_tokens_per_session` | `session` | The same calls, each `ceil(result bytes / bytes_per_token)` tokens. | **yes** | no |
//! | `cache_read_share` | `ppm` | `prompt_token` | Cache-read tokens over prompt tokens (input, cache reads and writes). | no | no |
//! | `tool_error_rate` | `ppm` | `call` | Tool calls whose result is an error, over tool calls. | no | yes |
//! | `parked_ms_per_item` | `ms_per_closed_item` | `closed_item` | The time the closed items spent parked, for any reason. | no | yes |
//! | `flaky_verdict_rate` | `ppm` | `closed_item` | Closed items whose verdict was flaky, over closed items. | no | yes |
//!
//! The item and commit metrics have kind `run`; the session metrics come for `run` and
//! `interactive` sessions both. Interactive sessions are shown, never acted on.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RepoRef;

/// Result of `heat.metrics`. Its params are those of `report.exact` ([`crate::ReportParams`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatMetrics {
    pub identity: String,
    /// The range asked for.
    pub from_ms: i64,
    pub to_ms: i64,
    /// The repository asked for; `null` for every repository of the identity.
    pub repo: Option<RepoRef>,
    /// The global ratio of bytes to tokens of the estimates: `b` bytes are `ceil(b / ratio)`
    /// tokens.
    pub bytes_per_token: u64,
    /// By repository path, provider, kind and id.
    pub metrics: Vec<HeatMetric>,
}

/// One metric of one group (see the module documentation for each `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatMetric {
    pub id: String,
    pub repo: RepoRef,
    pub provider: String,
    /// The session kind, `run` or `interactive`; `run` for the item and commit metrics.
    pub kind: Option<String>,
    /// The normalized figure, in `unit`, rounded down.
    pub value: u64,
    /// The numerator, in its base unit.
    pub total: u64,
    /// The denominator, in `sample_unit`; never zero.
    pub sample: u64,
    pub unit: String,
    pub sample_unit: String,
    /// `true` for a figure guessed from bytes: never added to an exact one.
    pub estimate: bool,
    /// The ratio an estimate was made with; `null` for an exact figure.
    pub bytes_per_token: Option<u64>,
    /// Whether the figure can be compared between providers.
    pub runner_comparable: bool,
}

/// Params of `heat.evaluate`: an identity and, optionally, one of its repositories by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatEvaluateParams {
    pub identity: String,
    /// A repository's name in the configuration; every repository of the identity when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Result of `heat.evaluate`: what the evaluation did, per repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatEvaluation {
    pub identity: String,
    /// The evaluation's time: the window is the 14 days before it.
    pub at_ms: i64,
    /// By repository, in configuration order.
    pub repos: Vec<HeatRepoEvaluation>,
}

/// One repository's evaluation, in counts. A finding seen once is a sighting, not listed by
/// `heat.list`; seen at the next evaluation again it is promoted to a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatRepoEvaluation {
    pub repo: RepoRef,
    /// The detectors' findings.
    pub findings: u64,
    /// New sightings stored.
    pub sightings: u64,
    /// Sightings found again and promoted to proposals.
    pub promoted: u64,
    /// Sightings found again the rate limit held back (3 per evaluation, 10 active).
    pub held_back: u64,
    /// Proposals found again.
    pub seen: u64,
    /// Sightings not found again, forgotten.
    pub forgotten: u64,
    /// Open proposals not found for two windows, expired.
    pub expired: u64,
    /// Findings of a subject in its 90-day cooldown, dropped.
    pub cooled_down: u64,
    /// Findings of no known subject kind, or repeated.
    pub skipped: u64,
    /// The detectors silent below the minimum sample.
    pub insufficient: Vec<HeatInsufficient>,
    /// Why this repository's evaluation failed; its counts are then zero and nothing was
    /// written for it.
    pub error: Option<String>,
}

/// A detector silent in one provider's group below the minimum sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatInsufficient {
    pub detector: String,
    pub provider: String,
    pub closed_items: u64,
    pub run_sessions: u64,
}

/// Params of `heat.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatListParams {
    pub identity: String,
    /// A repository's name in the configuration; every repository of the identity when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Every state, not only the active ones (`open`, `published`, `landed`).
    #[serde(default)]
    pub all: bool,
}

/// Result of `heat.list`: the identity's proposals (findings seen in two evaluations in a row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatProposals {
    pub identity: String,
    /// By creation, oldest first.
    pub proposals: Vec<ProposalView>,
}

/// One proposal: a metric of one subject over a window, its value, threshold and target, all in
/// exact units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalView {
    /// The proposal's key: what a user names it by.
    pub key: String,
    pub repo: RepoRef,
    pub provider: String,
    pub detector: String,
    pub detector_version: u32,
    /// The class of remedy proposed.
    pub remedy: String,
    /// `file`, `dir`, `test`, `tool_target`, `command` or `repo`.
    pub subject_kind: String,
    /// A repository-relative path for a file or a directory.
    pub subject: String,
    pub metric: String,
    pub unit: String,
    /// `true` for a metric guessed from bytes; a trigger is always exact.
    pub estimate: bool,
    /// The metric's value over the window, in `unit`.
    pub baseline_value: i64,
    /// The threshold the value reached.
    pub threshold: i64,
    /// The value the remedy aims at.
    pub target_value: i64,
    pub window_from_ms: i64,
    pub window_to_ms: i64,
    /// The observations the value is computed from.
    pub sample: u64,
    /// An ESTIMATE of the tokens at stake, for ranking and display only: never added to an
    /// exact figure.
    pub impact_eqv_estimate: Option<u64>,
    /// `open`, `published`, `landed`, `measured`, `dismissed` or `expired`.
    pub state: String,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    /// The evaluations that found it.
    pub seen_count: u64,
    /// Why the user dismissed it; `null` unless it is `dismissed`.
    #[serde(default)]
    pub dismiss_reason: Option<String>,
    /// The evidence, redacted lines.
    pub evidence: Vec<String>,
}

/// Params of `heat.dismiss`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeatDismissParams {
    /// The operator token the daemon wrote at start (see [`crate::control`]). Missing, it is
    /// refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    pub identity: String,
    /// The proposal's key (16 hex digits), as `heat.list` shows it.
    pub key: String,
    /// Why it is dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
