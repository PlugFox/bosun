//! The records a provider parser emits: the event model of `bosun-core` plus the few
//! provider-neutral records that are not a message (#3, the schema).

use std::collections::BTreeMap;

use bosun_core::{ContextWindow, Message, Tokens, WindowKey};

/// The file a line comes from, as the tailer knows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    /// An opaque, stable key of the file (its file id, not its path, #3).
    pub file: String,
    /// The context window the file holds, from the provider's path layout.
    pub window: WindowKey,
}

/// One thing a line says. A line yields zero or more records.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// One record of an API message: its usage and the tool calls of this record's blocks.
    /// The records of one message are folded with [`bosun_core::merge`].
    Message(Message),
    /// The result of a tool call, matched to its call by `tool_use` id.
    ToolResult(ToolResult),
    /// A file a tool call read, wrote or searched.
    FileTouch(FileTouch),
    /// The provider's own totals for a session: a cross-check, never a source of the bill.
    SessionCheck(SessionCheck),
}

/// The result of a tool call. It arrives in a later line than the call, so it is a record of
/// its own and is applied to the call by `tool_use` id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub provider: String,
    /// The `tool_use` id of the call this is the result of.
    pub tool_use_id: String,
    /// Size of the result, in bytes.
    pub result_bytes: u64,
    /// Whether the result was an error.
    pub is_error: bool,
}

/// How a tool call touched a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TouchKind {
    Read,
    Write,
    Search,
}

/// A file (or, for a search, a directory) a tool call touched. The path is canonical and
/// absolute; attributing it to a repository is the tailer's job (#21).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTouch {
    pub provider: String,
    /// The message whose tool call this is.
    pub message_id: String,
    pub tool_use_id: String,
    /// The canonical absolute path ([`bosun_core::canonical_path`]).
    pub path: String,
    pub kind: TouchKind,
}

/// A provider's totals for one model in a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTotals {
    pub tokens: Tokens,
    /// The provider's own cost, in USD, when it gives one.
    pub cost_usd: Option<f64>,
}

/// The provider's own per-model totals for a session (Claude Code's `cost-state`). A session
/// may hold several; the last one, by offset, is the cumulative total (#3).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCheck {
    pub provider: String,
    pub session_id: String,
    /// The offset of the line, so that the last record of a file wins.
    pub offset: u64,
    pub total_cost_usd: Option<f64>,
    pub models: BTreeMap<String, ModelTotals>,
}

/// What a provider says about a context window beyond its key: for a sub-agent, its parent,
/// its type and depth, and the tool call that spawned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMeta {
    pub window: ContextWindow,
    pub agent_type: Option<String>,
    pub spawn_depth: Option<u32>,
    /// The `tool_use` id of the call in the parent that spawned this window.
    pub spawned_by: Option<String>,
}
