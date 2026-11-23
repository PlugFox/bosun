//! A repository's permission policy (#2, #7): the paths an unattended session may not edit and
//! whether it may push.

use std::collections::BTreeSet;

/// The file that is protected whatever the policy says: a line added to it can hide the file
/// that makes a verdict pass (#7).
pub const ALWAYS_PROTECTED: &str = ".gitignore";

/// `[repo.<name>.policy]`, effective: [`ALWAYS_PROTECTED`] is in [`Policy::protected`] even when
/// the file does not list it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Policy {
    /// `protected`: repository-relative entries, `/`-separated, sorted and without duplicates.
    /// An entry with no `/` names a file or directory at any depth (`.gitignore` protects
    /// `sub/.gitignore` too); an entry with a `/` names that path and, if it is a directory,
    /// everything under it. There are no wildcards.
    pub protected: BTreeSet<String>,
    /// `push`: whether a session may push. Default false (#2).
    pub push: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(BTreeSet::new(), false)
    }
}

impl Policy {
    /// The effective policy for the listed entries: [`ALWAYS_PROTECTED`] is added.
    pub(crate) fn new(mut protected: BTreeSet<String>, push: bool) -> Self {
        if !cfg!(mutant_config_gitignore_unprotected) {
            protected.insert(ALWAYS_PROTECTED.to_owned());
        }
        Self { protected, push }
    }

    /// Whether `path` (repository-relative, `/`-separated, as git prints it) is protected.
    ///
    /// Comparison ignores ASCII case: on Windows and macOS `.GITIGNORE` is the same file as
    /// `.gitignore`, and over-protecting on Linux fails closed.
    #[must_use]
    pub fn protects(&self, path: &str) -> bool {
        let path = path.trim_start_matches("./");
        self.protected.iter().any(|entry| matches(entry, path))
    }
}

/// Whether `entry` (see [`Policy::protected`]) covers `path`.
fn matches(entry: &str, path: &str) -> bool {
    let entry = entry.trim_end_matches('/');
    if entry.contains('/') {
        let lower = path.to_ascii_lowercase();
        let entry = entry.to_ascii_lowercase();
        lower == entry
            || lower
                .strip_prefix(&entry)
                .is_some_and(|rest| rest.starts_with('/'))
    } else {
        path.split('/')
            .any(|segment| segment.eq_ignore_ascii_case(entry))
    }
}

/// Checks a repository-relative entry (a protected path or a build directory) and returns it
/// normalised (no trailing `/`), or why it is refused.
pub(crate) fn relative_entry(text: &str) -> Result<String, &'static str> {
    if text.contains('\\') {
        return Err("use `/` as the separator");
    }
    if text.starts_with('/') || text.as_bytes().get(1) == Some(&b':') {
        return Err("it must be relative to the repository");
    }
    if text.contains(['*', '?', '[']) {
        return Err("wildcards are not supported; name the file or directory");
    }
    let trimmed = text.trim_start_matches("./").trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("it names the whole repository");
    }
    if trimmed
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("it has an empty, `.` or `..` segment");
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> Policy {
        Policy::new(entries.iter().map(|&e| e.to_owned()).collect(), false)
    }

    #[test]
    fn a_policy_that_omits_gitignore_still_protects_it() {
        for policy in [Policy::default(), policy(&["tests/harness.rs"])] {
            assert!(policy.protected.contains(ALWAYS_PROTECTED), "{policy:?}");
            assert!(policy.protects(".gitignore"));
            assert!(policy.protects("crates/x/.gitignore"));
            assert!(policy.protects("crates/x/.GitIgnore"));
            assert!(!policy.protects("src/main.rs"));
        }
    }

    #[test]
    fn entries_match_files_and_directories() {
        let policy = policy(&["tests/harness", "xtask/src/lint.rs", "fixtures"]);
        assert!(policy.protects("tests/harness"));
        assert!(policy.protects("tests/harness/run.rs"));
        assert!(policy.protects("./Tests/Harness/run.rs"));
        assert!(!policy.protects("tests/harness2/run.rs"));
        assert!(!policy.protects("other/tests/harness/run.rs"));
        assert!(policy.protects("xtask/src/lint.rs"));
        assert!(!policy.protects("xtask/src/lint.rs.bak"));
        assert!(policy.protects("crates/a/fixtures/one.json"));
        assert!(!policy.protects("crates/a/fixtures2/one.json"));
    }

    #[test]
    fn relative_entries() {
        assert_eq!(relative_entry("tests/"), Ok("tests".to_owned()));
        assert_eq!(relative_entry("./a/b"), Ok("a/b".to_owned()));
        for bad in [
            "", "/", "./", "/abs", "C:/x", r"a\b", "a/*.rs", "a//b", "a/../b", "..",
        ] {
            assert!(relative_entry(bad).is_err(), "{bad:?}");
        }
    }
}
