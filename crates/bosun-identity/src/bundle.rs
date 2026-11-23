//! The identity bundle (#2): what one identity holds, as the daemon hands it to this crate.

use std::ffi::OsString;
use std::path::PathBuf;

/// One identity's bundle (#2, "The identity bundle"). The daemon builds it from the
/// configuration; this crate only reads it. The GitHub token is not a field: `gh` reads it under
/// [`Bundle::gh_config_dir`] (D5 of #7), so it never enters the daemon's memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The identity's name, as in `[identity.<name>]`.
    pub identity: String,
    /// The GitHub host, lower case (`github.com`, or an Enterprise host).
    pub host: String,
    /// The login the identity's token must belong to.
    pub login: String,
    /// The identity's `gh` configuration directory (`GH_CONFIG_DIR`). The daemon's own `gh`
    /// only; never an agent's.
    pub gh_config_dir: PathBuf,
    /// The identity's git configuration file (`GIT_CONFIG_GLOBAL`): author, committer, signing.
    pub git_config: PathBuf,
    /// The SSH key the daemon's git pushes with, if the identity uses SSH.
    pub ssh_key: Option<PathBuf>,
    /// The author every commit of the identity must carry.
    pub author: Author,
    /// Owners and organisations whose repositories the identity may touch.
    pub owners: Vec<String>,
    /// Each runner's configuration directory variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...)
    /// and its value. Agent sessions get them; the daemon's git and `gh` do not.
    pub runner_dirs: Vec<(String, PathBuf)>,
    /// Variables the identity itself supplies to its runner sessions, credentials included (a
    /// provider API key). GitHub and git variables are refused here too: an agent never holds a
    /// GitHub credential.
    pub runner_env: Vec<(String, OsString)>,
}

/// A git author: the name and email of an identity's commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// `user.name`.
    pub name: String,
    /// `user.email`.
    pub email: String,
}

impl Author {
    /// Parses git's ident line (`git var GIT_AUTHOR_IDENT`: `Name <email> 1700000000 +0000`);
    /// the timestamp is optional. `None` if there is no `<email>`.
    #[must_use]
    pub fn from_ident(line: &str) -> Option<Self> {
        let line = line.trim();
        let open = line.find('<')?;
        let close = open + line[open..].find('>')?;
        Some(Self {
            name: line[..open].trim().to_owned(),
            email: line[open + 1..close].trim().to_owned(),
        })
    }

    /// The same author: the name exactly, the email ignoring ASCII case.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.name == other.name && self.email.eq_ignore_ascii_case(&other.email)
    }
}

#[cfg(test)]
mod tests {
    use super::Author;

    fn author(name: &str, email: &str) -> Author {
        Author {
            name: name.to_owned(),
            email: email.to_owned(),
        }
    }

    #[test]
    fn ident_lines_parse_with_and_without_a_timestamp() {
        let expected = author("Test Person", "test@example.test");
        assert_eq!(
            Author::from_ident("Test Person <test@example.test> 1700000000 +0000\n"),
            Some(expected.clone())
        );
        assert_eq!(
            Author::from_ident("Test Person <test@example.test>"),
            Some(expected)
        );
        assert_eq!(Author::from_ident("Test Person"), None);
        assert_eq!(Author::from_ident("Test Person <unclosed"), None);
    }

    #[test]
    fn authors_match_on_name_and_email_case_insensitively() {
        let a = author("Test Person", "Test@Example.test");
        assert!(a.matches(&author("Test Person", "test@example.test")));
        assert!(!a.matches(&author("test person", "test@example.test")));
        assert!(!a.matches(&author("Test Person", "other@example.test")));
    }
}
