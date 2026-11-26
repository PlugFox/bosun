//! The `control.*` methods (#4, #7, #67): pause, resume, the kill switch, and the control state.
//!
//! # The operator token
//!
//! An agent session runs as the same OS user as the daemon, so it could open a connection of
//! its own to the endpoint. The methods that change the control state therefore take an
//! operator token ([`PauseParams::operator`], [`KillParams::operator`]) that the agent's
//! environment does not have: the daemon writes a fresh one at every start to a user-only file
//! outside every working tree (`bosun_client::operator`), which the CLI and the TUI read. A
//! request without the current token is refused with [`crate::rpc::code::UNAUTHORIZED`].
//! `control.status` only reads, and takes no token.
//!
//! # Holds
//!
//! - A pause (of an identity, or of one repository) lets the current attempt finish and starts
//!   no new session: neither a new attempt, nor a resume, nor a new claim.
//! - A repository is paused automatically while an interactive session (one bosun did not
//!   spawn) works in its checkout, until that session has been idle for the configured time
//!   (`daemon.auto_pause_idle`). `control.resume` of the repository lifts it until the next
//!   interactive activity.
//! - The kill switch stops every child now, parks every item and stops every board write; it
//!   holds until it is released. It is daemon-wide and names no identity (#4).
//! - The daily cap (`budget.daily_eqv` less `budget.reserve_eqv`) parks the queue until local
//!   midnight.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params of `control.pause` and `control.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PauseParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    /// The identity whose repositories are paused or resumed.
    pub identity: String,
    /// One repository of the identity, by its name in the configuration; every repository of
    /// the identity when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Params of `control.kill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KillParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    /// Release the kill switch instead of throwing it: parked items resume on their next tick.
    #[serde(default)]
    pub release: bool,
}

/// Result of `control.kill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KillResult {
    /// The kill switch is thrown.
    pub killed: bool,
}

/// The control state of one identity: result of `control.status`, `control.pause` and
/// `control.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlStatus {
    pub identity: String,
    /// The kill switch is thrown (daemon-wide).
    pub killed: bool,
    /// Every repository of the identity is paused by hand.
    pub paused: bool,
    /// The daily cap parks the identity's queue until this time (local midnight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_cap_until_ms: Option<i64>,
    /// Each repository of the identity, by name.
    pub repos: Vec<RepoControl>,
}

/// The control state of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoControl {
    /// The repository's name in the configuration.
    pub repo: String,
    /// Paused by hand.
    pub paused: bool,
    /// Paused automatically by an interactive session in its checkout, until this time unless
    /// the session goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_paused_until_ms: Option<i64>,
    /// The hold in force now, strongest first: `kill`, `pause`, `daily_cap`; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    /// Paused by the fast-death breaker (#7): why, until `bosun resume`. Absent when not
    /// tripped; a daemon restart lifts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tripped: Option<String>,
}
