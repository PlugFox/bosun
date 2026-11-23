//! A commit of a configured repository, as the commit reader records it (#3, `commit`; #23).

use std::collections::BTreeMap;

/// One commit. Identified within its repository by `sha`; a commit never changes, so storing
/// it again changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The full object name, lower-case hex.
    pub sha: String,
    /// The committer time, milliseconds since the Unix epoch, UTC. Git keeps whole seconds, so
    /// the last three digits are zero.
    pub ts_ms: i64,
    /// `Name <email>` of the author.
    pub author: String,
    /// Lines added over every file of the diff against the first parent (none for a merge).
    pub lines_added: u64,
    /// Lines removed, likewise.
    pub lines_removed: u64,
    /// The trailers of the message: name to every value, in message order.
    pub trailers: BTreeMap<String, Vec<String>>,
}
