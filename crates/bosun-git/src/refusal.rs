//! Why an item goes to Needs you before its verdict runs (#7 "Verdict", #59): a new ignored
//! file, a change to git's own configuration, or a protected path its commits touched.
//!
//! These are not failed attempts, unlike a [`Reason`](crate::Reason): retrying cannot help, and
//! nothing is undone (a new ignored file is never deleted; it may be the user's build output).

use std::fmt;
use std::path::PathBuf;

/// One condition that sends the item to Needs you without running the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Ignored files appeared during the session outside the declared build directories.
    NewIgnored {
        /// The new ignored paths, repository-relative, `/`-separated, sorted.
        paths: Vec<String>,
    },
    /// A file of git's own configuration (a hook, `.git/config`, `config.worktree`, an
    /// `include.path` target, `info/exclude`, `info/attributes`) was created, changed or
    /// deleted since the snapshot at claim.
    ConfigChanged {
        /// The file.
        path: PathBuf,
        /// The SHA-256 of its content at claim; `None` when it did not exist.
        before: Option<String>,
        /// The SHA-256 of its content now; `None` when it no longer exists.
        after: Option<String>,
    },
    /// The item's commits touched protected paths that the snapshot does not allow.
    Protected {
        /// The touched protected paths, repository-relative, `/`-separated, sorted.
        paths: Vec<String>,
        /// The diff stat of those paths over the item's commits, for the Needs you comment.
        stat: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "(none)".to_owned());
        match self {
            Self::NewIgnored { paths } => write!(
                f,
                "new ignored files outside the build directories: {}",
                paths.join(", ")
            ),
            Self::ConfigChanged {
                path,
                before,
                after,
            } => write!(
                f,
                "git configuration changed: {} ({} -> {})",
                path.display(),
                opt(before),
                opt(after)
            ),
            Self::Protected { paths, stat } => write!(
                f,
                "the commits touch protected paths: {}\n{}",
                paths.join(", "),
                stat.trim_end()
            ),
        }
    }
}

/// Whether the repository-relative `entry` (a build directory or an `allow-protected` entry,
/// `/`-separated) covers `path`: the path itself or anything under it. ASCII case is ignored,
/// as in the permission policy: on Windows and macOS the two spellings are one file.
pub(crate) fn covers(entry: &str, path: &str) -> bool {
    let entry = entry
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let path = path
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    !entry.is_empty()
        && (path == entry
            || path
                .strip_prefix(&entry)
                .is_some_and(|rest| rest.starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::covers;

    #[test]
    fn entries_cover_the_path_and_what_is_under_it() {
        assert!(covers("target", "target"));
        assert!(covers("target/", "target/debug/x.o"));
        assert!(covers("./Target", "target/x"));
        assert!(!covers("target", "target2/x"));
        assert!(!covers("target", "sub/target/x"));
        assert!(!covers("", "x"));
        assert!(covers("tests/h.rs", "tests/h.rs"));
        assert!(!covers("tests/h.rs", "tests/h.rs.bak"));
    }
}
