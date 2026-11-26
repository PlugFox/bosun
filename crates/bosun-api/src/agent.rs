//! The `agent.*` methods (#7, #63): what an agent session calls through `bosun-mcp`.
//!
//! Every `agent.*` request carries the session's credential, which the daemon issued at spawn
//! and bound to one identity, item, attempt and session; it names no identity of its own, the
//! credential does. The credential admits `agent.*` only, and `agent.*` admits nothing else: a
//! request without a live credential, or naming another item than the credential's, is refused
//! with [`crate::rpc::code::UNAUTHORIZED`]. The credential is revoked when its session ends.
//! While a daemon that has just started has not re-admitted every session that went on under
//! its shim (#8, E10), an unknown credential is refused with
//! [`crate::rpc::code::RECOVERING`] instead: the call may be made again, and `bosun-mcp`
//! retries it.
//!
//! There is no `done` and no `report_verdict`: nothing the agent says moves its item to Done
//! (#1, invariant 2). A call is recorded with the attempt and the commit HEAD resolves to when
//! it is made; a call over its tool's per-attempt cap is refused with
//! [`crate::rpc::code::RATE_CAPPED`] and recorded as refused.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params of `agent.hello`: the handshake `bosun-mcp` makes after it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentHelloParams {
    /// The session's credential, from its MCP configuration.
    pub credential: String,
    /// The item the session works on, as the daemon named it.
    pub item: String,
}

/// Result of `agent.hello`: what the credential is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentHelloResult {
    /// The API version of the daemon.
    pub api_version: u32,
    pub identity: String,
    pub item: String,
    pub attempt: u32,
    /// The provider's session id.
    pub session: String,
}

/// Params of `agent.ask_question`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AskQuestionParams {
    pub credential: String,
    pub item: String,
    /// The question, for the user.
    pub text: String,
    /// The agent cannot go on without an answer: the item goes to Needs you and the session is
    /// ended after a short grace period. A question that does not block goes into the item
    /// report.
    pub blocking: bool,
}

/// Params of `agent.submit_summary`. It may be called more than once; the last call of an
/// attempt wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubmitSummaryParams {
    pub credential: String,
    pub item: String,
    /// The summary of the work.
    pub body: String,
    /// Decisions taken on the way.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Follow-ups for later items.
    #[serde(default)]
    pub followups: Vec<String>,
}

/// Params of `agent.note`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteParams {
    pub credential: String,
    pub item: String,
    pub text: String,
}

/// Result of a recorded `agent.ask_question`, `agent.submit_summary` or `agent.note` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentAck {
    /// The attempt the call is bound to.
    pub attempt: u32,
    /// The commit HEAD resolved to at the time of the call; `null` when it could not be read.
    pub head_sha: Option<String>,
}
