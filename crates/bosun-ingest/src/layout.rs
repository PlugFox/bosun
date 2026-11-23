//! Where a provider keeps its transcripts: what discovery needs to know of a provider beyond
//! its line parser.

use std::path::Path;

use bosun_core::{Gap, WindowKey, WindowMeta};

use crate::line::{LineParser, gap};

/// What a file in a provider's transcript directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A transcript: one context window, one JSON object per line.
    Transcript,
    /// A sidecar file describing a context window (Claude Code's sub-agent `.meta.json`).
    Meta,
}

/// A file located by a provider's path layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The context window the file belongs to.
    pub window: WindowKey,
    pub kind: FileKind,
}

/// A provider as discovery and tailing see it: a line parser plus its path layout.
pub trait Provider: LineParser + Clone + Send + Sync + 'static {
    /// What the file at `path` (under the provider's directory) is, or `None` for a file that
    /// is neither a transcript nor a meta file.
    fn locate(&self, path: &Path) -> Option<Located>;

    /// Whether discovery descends into the directory named `dir`.
    fn descend(&self, dir: &str) -> bool {
        let _ = dir;
        true
    }

    /// Reads a meta file of `window`.
    ///
    /// # Errors
    ///
    /// A [`Gap`] at offset 0 covering the file, when it is not what the provider writes.
    fn parse_meta(&self, window: &WindowKey, bytes: &[u8]) -> Result<WindowMeta, Gap> {
        let _ = window;
        Err(gap(
            0,
            bytes,
            None,
            Vec::new(),
            "this provider has no meta files",
        ))
    }
}
