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
/// Ordered by timestamp, then window (session id, then agent id, the main transcript first),
/// then file: the order is the ownership rule and its tiebreak (see [`crate::merge`], #3).
///
/// The window comes before the file because a fork copies records with their timestamps, and
/// the file key (a file id) is arbitrary and changes when the tree is copied; the session id
/// is the same wherever the file is. The file key stays last so that two files of one window
/// (one session under two project directories) still compare unequal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    /// The record's timestamp, milliseconds since the Unix epoch, UTC.
    pub ts_ms: i64,
    /// An opaque, stable key of the source file (its file id, not its path, #3).
    pub file: String,
    /// The context window that file holds.
    pub window: WindowKey,
}

impl Ord for Origin {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let by_time = self.ts_ms.cmp(&other.ts_ms);
        if cfg!(mutant_core_tie_by_file) {
            // Mutant: the file key breaks ties before the window.
            return by_time
                .then_with(|| self.file.cmp(&other.file))
                .then_with(|| self.window.cmp(&other.window));
        }
        by_time
            .then_with(|| self.window.cmp(&other.window))
            .then_with(|| self.file.cmp(&other.file))
    }
}

impl PartialOrd for Origin {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
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
    /// Size of the tool's result, in bytes; zero until the result is seen. It counts text and
    /// media alike; [`ToolCall::result_media_bytes`] is the media part of it.
    pub result_bytes: u64,
    /// Whether the result was an error.
    pub is_error: bool,
    /// The fingerprint of a shell command: the lowercase hex SHA-256 (64 digits) of its
    /// whitespace-normalised, scrubbed text, so that two spellings of one command share it.
    /// `None` for a call that is not a shell command, or whose provider does not give one.
    pub command_hash: Option<String>,
    /// The text of a shell command, whitespace-normalised, scrubbed
    /// (`bosun_core::publish::scrub`) and cut to at most 200 bytes at a char boundary, after
    /// scrubbing. Only a `Bash` or `PowerShell` call's `command` input ever gets one, and the
    /// store keeps it (and [`ToolCall::command_hash`]) only for a message attributed to a
    /// repository: for a message in no repository nothing of a command's text reaches the
    /// database. The text may name a path outside the repository; that is accepted (#210).
    /// Nothing of any other input (a description, file content, a prompt) and no byte of a
    /// result's content is ever kept.
    pub command_text: Option<String>,
    /// When the call was made: the timestamp of its record, milliseconds since the Unix epoch,
    /// UTC; `None` when unknown.
    pub call_ts_ms: Option<i64>,
    /// When the call's result arrived: the timestamp of the result's own record, milliseconds
    /// since the Unix epoch, UTC; `None` until the result is seen, or when it had no time.
    pub result_ts_ms: Option<i64>,
    /// The part of [`ToolCall::result_bytes`] that is media (base64 image data), in bytes.
    pub result_media_bytes: u64,
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

/// The result of a tool call. It arrives in a later record than the call, so it is an event of
/// its own and is applied to the call by `tool_use` id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub provider: String,
    /// The `tool_use` id of the call this is the result of.
    pub tool_use_id: String,
    /// Size of the result, in bytes: text and media.
    pub result_bytes: u64,
    /// Whether the result was an error.
    pub is_error: bool,
    /// The timestamp of the result's own record, milliseconds since the Unix epoch, UTC;
    /// `None` when the record had none.
    pub ts_ms: Option<i64>,
    /// The part of `result_bytes` that is media (base64 image data), in bytes.
    pub media_bytes: u64,
}

/// How a tool call touched a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TouchKind {
    Read,
    Write,
    Search,
    /// A script run by an interpreter (`python x.py`): neither a read nor a write, but the
    /// file is used (#44 A).
    Exec,
    /// A file named by a command whose effect on it is not known: a gap, never a read.
    Unknown,
}

impl TouchKind {
    /// The name the store keeps.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Search => "search",
            Self::Exec => "exec",
            Self::Unknown => "unknown",
        }
    }
}

/// A file (or, for a search, a directory) a tool call touched. The path is canonical and
/// absolute; the store makes it relative to the repository of the message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTouch {
    pub provider: String,
    /// The message whose tool call this is.
    pub message_id: String,
    pub tool_use_id: String,
    /// The canonical absolute path ([`crate::canonical_path`]).
    pub path: String,
    pub kind: TouchKind,
    /// The path was taken out of free text (a shell command) rather than a tool's path
    /// field: it is stored only when it lies inside the message's repository, and dropped
    /// otherwise, so that nothing else of the text reaches the database (#2, #76).
    pub confined: bool,
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
///
/// The totals cover a window of the session, not all of it: the provider's counter starts
/// again when a session is resumed in a new process, and records written after the line are
/// not in it (#46). The window is `start_ms ..= end_ms`; a bound that is `None` is unknown and
/// leaves that side open.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCheck {
    pub provider: String,
    pub session_id: String,
    /// The offset of the line, so that the last record of a file wins.
    pub offset: u64,
    /// When the provider's counter started (Claude Code's `startTime`), milliseconds since the
    /// Unix epoch, UTC.
    pub start_ms: Option<i64>,
    /// The timestamp of the last timestamped record before the line in the same file, of any
    /// type. A line parser leaves it `None`; the reader of the file, which sees its lines in
    /// order, fills it.
    pub end_ms: Option<i64>,
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
