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
