//! The records a provider parser emits: the event model of `bosun-core`, which also holds the
//! few provider-neutral events that are not a message (#3, the schema).

use bosun_core::{Message, WindowKey};

pub use bosun_core::{FileTouch, ModelTotals, SessionCheck, ToolResult, TouchKind, WindowMeta};

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
