//! The provider-neutral event model (#3, "A normalised event model").
//!
//! A provider parser turns transcript lines into these types; the store turns them into rows.
//! Nothing here names a provider: a second provider adds a parser, not a field.

use std::collections::BTreeMap;

/// The key of a context window: one transcript file of one provider session.
///
/// A sub-agent has its own window under the parent's session id, told apart by `agent_id`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowKey {
    /// The provider's session id.
    pub session_id: String,
    /// The sub-agent id; `None` for the main context of the session.
    pub agent_id: Option<String>,
}

/// Whether bosun spawned the session for a run or a person drove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowKind {
    /// Spawned by bosun for an item.
    Run,
    /// Anything else: a person's session, or a session bosun did not start.
    Interactive,
}

/// A context window: the unit whose prompt grows and is compacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWindow {
    pub key: WindowKey,
    /// The window that spawned this one (a sub-agent's parent); `None` for a root.
    pub parent: Option<WindowKey>,
    pub kind: WindowKind,
}

/// The five token counts of one API message, as the provider reported them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tokens {
    /// Uncached input.
    pub input: u64,
    /// Input written to the 5-minute cache.
    pub cache_write_5m: u64,
    /// Input written to the 1-hour cache.
    pub cache_write_1h: u64,
    /// Input read from the cache.
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// The size of the prompt: input plus cache reads and writes. This is what a model priced
    /// by prompt length is priced by, and what a compaction drops.
    #[must_use]
    pub fn prompt_size(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

/// The usage of one API message: the counts and the provider's usage object as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub tokens: Tokens,
    /// The raw usage JSON, kept for fields that are not columns (#3).
    pub raw: serde_json::Value,
}

/// Where a record of a message was seen. A message's origin is the earliest of its records'.
///
/// Ordered by timestamp, then file, then window: the derived order is the ownership rule and
/// its tiebreak (see [`crate::merge`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Origin {
    /// The record's timestamp, milliseconds since the Unix epoch, UTC.
    pub ts_ms: i64,
    /// An opaque, stable key of the source file (its file id, not its path, #3).
    pub file: String,
    /// The context window that file holds.
    pub window: WindowKey,
}

/// One tool call of a message. Its `tool_use` id is the key it is stored under in
/// [`Message::tool_calls`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    /// The tool's name.
    pub name: String,
    /// The normalised target: a file path, or the head (first word) of a command.
    pub target: Option<String>,
    /// Size of the tool's input, in bytes.
    pub input_bytes: u64,
    /// Size of the tool's result, in bytes; zero until the result is seen.
    pub result_bytes: u64,
    /// Whether the result was an error.
    pub is_error: bool,
}

/// One API message, assembled from one or more transcript records (#3, "Dedup and assembly").
///
/// A parser builds one `Message` per record; [`crate::merge`] folds the records of one message
/// into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The provider, for the key (provider, message id).
    pub provider: String,
    /// The provider's message id.
    pub id: String,
    /// Where the message was first seen; its timestamp is the message's.
    pub origin: Origin,
    /// The model id as the transcript names it.
    pub model: String,
    /// The working directory of the record the usage came from.
    pub cwd: String,
    pub usage: Usage,
    /// Tool calls keyed by `tool_use` id.
    pub tool_calls: BTreeMap<String, ToolCall>,
}

/// A line that could not be read as expected. Metadata only, never the line's content (#3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// Byte offset of the line in its source file.
    pub offset: u64,
    /// Length of the line in bytes.
    pub length: u64,
    /// The record type, when the line had one.
    pub record_type: Option<String>,
    /// The names of the line's top-level keys.
    pub top_level_keys: Vec<String>,
    /// Why the line is a gap.
    pub reason: String,
}
