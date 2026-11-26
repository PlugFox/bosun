//! The `queue.*` methods (#4, #5, #70): the board of each repository as the Queue tab and
//! `bosun queue` show it, and the card lint as `bosun lint` asks for it.
//!
//! The daemon reads the board itself, as the loop does, so a client never links a queue
//! backend (#12). Only a repository of the identity the request names is read (#2, rule 4).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params of `queue.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueParams {
    pub identity: String,
    /// One repository of the identity, by its name in the configuration; every repository of
    /// the identity when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Result of `queue.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueView {
    pub identity: String,
    /// The repositories asked for, by name.
    pub repos: Vec<RepoQueue>,
}

/// The board of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoQueue {
    /// Its name in the configuration.
    pub repo: String,
    /// The queue's kind: `local_file`, `projects_v2`, or `none`.
    pub queue: String,
    /// The hash of the board as read: the `hash` of the `queue_changed` event that announced
    /// it. Absent when the board could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Why the board could not be read; `columns` is empty then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Every column in board order (`Backlog`, `Ready`, `In progress`, `Verify`, `Done`,
    /// `Needs you`), empty ones included.
    pub columns: Vec<ColumnView>,
}

/// One column of a board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ColumnView {
    /// The column's name on the board.
    pub column: String,
    /// Top to bottom.
    pub cards: Vec<CardView>,
}

/// One card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardView {
    /// The card's number on the board (the issue's, or the local file's `id`).
    pub card: u64,
    /// The item id the daemon gives it: `<repo>#<card>`.
    pub item: String,
    pub title: String,
    pub labels: Vec<String>,
    pub draft: bool,
    /// The item's state in the store (`ready`, `running`, `needs_you`, ...); absent when the
    /// daemon has never recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The hash of the snapshot the item runs from, once claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_hash: Option<String>,
    /// The card was edited since its snapshot was taken: the edit applies at the item's next
    /// claim (a requeue or a restart), not to the attempt in flight.
    pub edit_pending: bool,
    /// The static lint of a card in Ready (empty elsewhere): errors keep it from being
    /// claimed, warnings do not.
    pub lint: Vec<LintFinding>,
    /// While it waits in Ready on open blockers (#5): their item ids, in its `blocked-by`
    /// order. Absent when it does not wait.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<String>,
}

/// One lint finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LintFinding {
    /// `error` or `warning`.
    pub severity: String,
    pub message: String,
}

/// Params of `queue.lint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LintParams {
    pub identity: String,
    /// The repository of the identity, by its name in the configuration.
    pub repo: String,
    /// The card's number on the board.
    pub card: u64,
}

/// Result of `queue.lint`: the static lint of one card, read fresh from the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LintResult {
    pub repo: String,
    pub card: u64,
    /// `<repo>#<card>`.
    pub item: String,
    /// No finding is an error: the card may be claimed.
    pub claimable: bool,
    /// Every finding, errors and warnings, in check order.
    pub findings: Vec<LintFinding>,
}
