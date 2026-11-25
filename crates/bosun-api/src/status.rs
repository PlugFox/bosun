//! `status.overview` (#4, #70): what the Overview tab and `bosun status` show of one identity.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Result of `status.overview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Overview {
    pub identity: String,
    pub daemon_version: String,
    /// When the daemon started.
    pub started_ms: i64,
    /// The daemon's clock when it answered: the `since_ms` of each item is against it.
    pub now_ms: i64,
    /// The kill switch is thrown (daemon-wide).
    pub killed: bool,
    /// Every repository of the identity is paused by hand.
    pub paused: bool,
    /// The daily cap parks the identity's queue until this time (local midnight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_cap_until_ms: Option<i64>,
    /// The identity's event stream: a client that subscribes with this epoch and
    /// `from_seq = head + 1` misses nothing published after this answer.
    pub events: StreamStatus,
    /// Each repository of the identity, by name.
    pub repos: Vec<RepoOverview>,
}

/// Where an identity's event stream stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StreamStatus {
    /// The daemon's epoch (#69).
    pub epoch: String,
    /// The sequence of the last event published; 0 before the first.
    pub head: u64,
    /// Subscribers connected now.
    pub subscribers: u64,
}

/// One repository of the Overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoOverview {
    /// Its name in the configuration.
    pub repo: String,
    /// Its checkout as configured.
    pub path: String,
    /// Its queue's kind: `local_file`, `projects_v2`, or `none` when it has no queue.
    pub queue: String,
    /// `working` while an item of the repository is in flight, else `idle`.
    pub state: String,
    /// The hold in force on the repository, strongest first: `kill`, `pause`, `daily_cap`;
    /// absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    /// The item in flight (claimed, running, parked, verifying or landing), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<CurrentItem>,
    /// The last verdict run that finished in the repository: of the item in flight, else of
    /// the item updated last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<LastVerdict>,
}

/// The item a repository is working on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CurrentItem {
    pub item: String,
    /// Its state (`claimed`, `running`, `parked`, `verifying`, `landing`).
    pub state: String,
    /// The park reason, for `parked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The current attempt; 0 before the first.
    pub attempt: u32,
    /// When it entered its state: the time in state is `now_ms - since_ms`.
    pub since_ms: i64,
    /// While its lease request is not granted: the full resource instances it waits for, each
    /// with its holders (another identity's holder is `busy`, see [`crate::lease`]). Absent
    /// when it waits for nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<crate::WaitingFor>,
}

/// A verdict run that finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LastVerdict {
    pub item: String,
    pub attempt: u32,
    pub run: u32,
    /// `pass`, `fail` or `timeout`.
    pub result: String,
    pub ended_ms: i64,
}
