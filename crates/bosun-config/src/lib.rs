//! Parsing and validation of the bosun configuration file (#4, #18).
//!
//! The file is TOML and is the registry of identities and repositories: there is no global
//! database (#3). The daemon reads it at start; the CLI reads it too, so `bosun doctor` works
//! with the daemon down (#12). This crate only reads: it opens no database and no credential
//! store.
//!
//! # Location
//!
//! [`default_path`]: the variable `BOSUN_CONFIG` when set, otherwise
//!
//! | Platform | File |
//! | --- | --- |
//! | Windows | `%LOCALAPPDATA%\bosun\config.toml` |
//! | macOS | `~/Library/Application Support/bosun/config.toml` |
//! | Linux and others | `$XDG_CONFIG_HOME/bosun/config.toml`, else `~/.config/bosun/config.toml` |
//!
//! On Windows the file is in the local, not the roaming, application data: it names paths of
//! this machine, and a roaming profile would copy it to others.
//!
//! # Format (stage 0)
//!
//! ```toml
//! [daemon]                      # accepted; stage 0 reads no key from it
//!
//! [identity.personal]
//! claude_config_dir = 'C:\Users\me\.claude'               # CLAUDE_CONFIG_DIR of the identity
//! database = 'C:\Users\me\AppData\Local\bosun\personal.db' # its own SQLite file
//!
//! [identity.work]
//! claude_config_dir = 'C:\Users\me\.claude-work'
//! database = 'C:\Users\me\AppData\Local\bosun\work.db'
//!
//! [repo.bosun]
//! path = 'C:\git\bosun'
//! identity = 'personal'
//!
//! [retention]
//! reingest_window = "30d"       # whole hours, days or weeks: "12h", "30d", "6w"; default 30d
//! ```
//!
//! - Paths are absolute; TOML literal strings (`'...'`) keep Windows backslashes as written.
//! - Identity and repository names are ASCII letters, digits, `-` and `_`.
//! - The other sections of #4 (`budget`, `notify`) and the keys of a stage-0 section that a
//!   later stage reads (an identity's GitHub login, a repository's landing, ...) are accepted
//!   and ignored, each with a [`Notice`]. Any other section is an error.
//!
//! # Validation
//!
//! - A repository binds an identity that is configured, and two repositories never share a
//!   path.
//! - Two identities never share a Claude configuration directory or a database file (#2). Paths
//!   are compared by their canonical key ([`bosun_core::canonical_path`]), so on Windows case
//!   and separators do not matter. Links and junctions are not resolved for this comparison.
//! - The directory of each database is on a local disk, as a [`DiskProbe`] sees it.
//!
//! Every problem carries its line and column when it has a place in the file.

mod disk;
mod error;
mod read;
mod validate;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use disk::{Disk, DiskProbe, SystemProbe};
pub use error::{Error, Location, Notice, Problem};

/// A valid configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// Identities by name.
    pub identities: BTreeMap<String, Identity>,
    /// Repositories by name.
    pub repos: BTreeMap<String, Repo>,
    /// `[retention]`.
    pub retention: Retention,
    /// What the file holds that this version does not read, in file order.
    pub notices: Vec<Notice>,
}

/// `[identity.<name>]`: the stage-0 part of an identity bundle (#2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Identity {
    /// `claude_config_dir`: the identity's `CLAUDE_CONFIG_DIR`, whose transcripts it owns.
    pub claude_config_dir: PathBuf,
    /// `database`: the identity's SQLite file (#3).
    pub database: PathBuf,
}

/// `[repo.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Repo {
    /// `path`, as written.
    pub path: PathBuf,
    /// The canonical key of `path` on this platform ([`bosun_core::canonical_path`]): the form
    /// working directories are matched against.
    pub key: String,
    /// `identity`: the name of the one identity the repository binds.
    pub identity: String,
}

/// `[retention]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Retention {
    /// `reingest_window`: how far back `bosun reingest` is expected to reach; `bosun doctor`
    /// warns when a provider deletes transcripts sooner (#3).
    pub reingest_window: Duration,
}

/// Parses and validates the text of a configuration file. `probe` tells whether each
/// database directory is on a local disk; [`SystemProbe`] is the real one.
///
/// # Errors
///
/// [`Error::Invalid`] with every problem found: malformed TOML (one problem, with its line
/// and column), a value of the wrong shape, or a failed validation.
pub fn parse(text: &str, probe: &dyn DiskProbe) -> Result<Config, Error> {
    let mut problems = Vec::new();
    let Some(draft) = read::read(text, &mut problems) else {
        return Err(Error::Invalid(problems));
    };
    validate::validate(&draft, probe, &mut problems);
    if !problems.is_empty() {
        problems.sort_by_key(|p| p.location.map(|at| (at.line, at.column)));
        return Err(Error::Invalid(problems));
    }

    let identities = draft
        .identities
        .into_iter()
        .filter_map(|i| {
            Some((
                i.name,
                Identity {
                    claude_config_dir: i.claude_config_dir?.path,
                    database: i.database?.path,
                },
            ))
        })
        .collect();
    let repos = draft
        .repos
        .into_iter()
        .filter_map(|r| {
            let path = r.path?;
            Some((
                r.name,
                Repo {
                    path: path.path,
                    key: path.key,
                    identity: r.identity?.0,
                },
            ))
        })
        .collect();
    Ok(Config {
        identities,
        repos,
        retention: Retention {
            reingest_window: draft.reingest_window,
        },
        notices: draft.notices,
    })
}

/// Reads and parses the file at `path`; see [`parse`].
///
/// # Errors
///
/// [`Error::Read`] when the file cannot be read, otherwise as [`parse`].
pub fn load(path: &Path, probe: &dyn DiskProbe) -> Result<Config, Error> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&text, probe)
}

/// Where the configuration file is on this platform (see the crate documentation), or `None`
/// when the environment names no home directory.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    default_path_in(&|name| std::env::var_os(name))
}

fn default_path_in(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(path) = var("BOSUN_CONFIG") {
        return Some(path);
    }
    let absolute = |name: &str| var(name).filter(|path| path.is_absolute());
    let dir = if cfg!(windows) {
        absolute("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        absolute("HOME")?
            .join("Library")
            .join("Application Support")
    } else {
        absolute("XDG_CONFIG_HOME").or_else(|| Some(absolute("HOME")?.join(".config")))?
    };
    Some(dir.join("bosun").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Local;

    impl DiskProbe for Local {
        fn probe(&self, _: &Path) -> Disk {
            Disk::Local
        }
    }

    fn abs(rest: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{}", rest.replace('/', "\\"))
        } else {
            format!("/{rest}")
        }
    }

    fn problems(text: &str) -> Vec<String> {
        match parse(text, &Local) {
            Ok(config) => panic!("accepted: {config:?}"),
            Err(Error::Invalid(problems)) => problems.iter().map(ToString::to_string).collect(),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_malformed_file_is_refused_with_its_line_and_column() {
        let found = problems("[daemon]\n\n[identity.personal]\ndatabase = 'unterminated\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("line 4, column "), "{found:?}");

        let found = problems("[repo.a]\npath = 'x'\npath = 'y'\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("line 3, column 1:"), "{found:?}");
    }

    #[test]
    fn the_stage_0_subset_is_read() {
        let text = format!(
            "[daemon]\n\
             [identity.personal]\n\
             claude_config_dir = '{}'\n\
             database = '{}'\n\
             [repo.bosun]\n\
             path = '{}'\n\
             identity = 'personal'\n\
             [retention]\n\
             reingest_window = \"2w\"\n",
            abs("home/.claude"),
            abs("data/personal.db"),
            abs("git/bosun"),
        );
        let config = parse(&text, &Local).unwrap();
        assert_eq!(config.notices, []);
        let personal = &config.identities["personal"];
        assert_eq!(
            personal.claude_config_dir,
            PathBuf::from(abs("home/.claude"))
        );
        assert_eq!(personal.database, PathBuf::from(abs("data/personal.db")));
        assert_eq!(config.repos["bosun"].identity, "personal");
        assert_eq!(
            config.retention.reingest_window,
            Duration::from_hours(14 * 24)
        );

        let empty = parse("", &Local).unwrap();
        assert!(empty.identities.is_empty() && empty.repos.is_empty());
        assert_eq!(
            empty.retention.reingest_window,
            read::DEFAULT_REINGEST_WINDOW
        );
    }

    #[test]
    fn later_sections_and_keys_are_ignored_with_a_notice() {
        let text = format!(
            "[daemon]\nhttp_port = 8080\n\
             [identity.personal]\n\
             claude_config_dir = '{}'\n\
             database = '{}'\n\
             github_login = 'someone'\n\
             [repo.bosun]\npath = '{}'\nidentity = 'personal'\n\
             landing = {{ mode = 'pr' }}\n\
             [budget]\ndaily = 1\n\
             [notify.telegram]\nchat = 1\n",
            abs("home/.claude"),
            abs("data/personal.db"),
            abs("git/bosun"),
        );
        let config = parse(&text, &Local).unwrap();
        let notices: Vec<String> = config.notices.iter().map(ToString::to_string).collect();
        assert_eq!(
            notices,
            [
                "line 2, column 1: `daemon.http_port` is not read in this version; ignored",
                "line 6, column 1: `identity.personal.github_login` is not read in this \
                 version; ignored",
                "line 10, column 1: `repo.bosun.landing` is not read in this version; ignored",
                "line 11, column 2: section `budget` is not read in this version; ignored",
                "line 13, column 2: section `notify` is not read in this version; ignored",
            ]
        );
    }

    #[test]
    fn shapes_are_checked() {
        let found = problems(
            "[identity.personal]\nclaude_config_dir = 'relative/dir'\n\
             [repo.x]\npath = 1\n\
             [retention]\nreingest_window = '30 days'\n\
             [unknown]\n",
        );
        assert_eq!(found.len(), 6, "{found:?}");
        assert!(
            found[0].contains("`identity.personal` has no `database`"),
            "{found:?}"
        );
        assert!(found[1].contains("must be absolute"), "{found:?}");
        assert!(found[2].contains("`repo.x` has no `identity`"), "{found:?}");
        assert!(
            found[3].contains("`repo.x.path` must be a string"),
            "{found:?}"
        );
        assert!(
            found[4].contains("expected a positive whole number"),
            "{found:?}"
        );
        assert!(found[5].contains("unknown section `unknown`"), "{found:?}");
    }

    #[test]
    fn load_reports_an_unreadable_file() {
        let missing = std::env::temp_dir().join("bosun-config-test-missing.toml");
        assert!(matches!(load(&missing, &Local), Err(Error::Read { .. })));
    }

    #[test]
    fn the_default_location_follows_the_platform() {
        let root = PathBuf::from(abs("profile"));
        let env = |name: &str| -> Option<OsString> {
            match name {
                "LOCALAPPDATA" | "HOME" => Some(root.clone().into()),
                _ => None,
            }
        };
        let expected = if cfg!(windows) {
            root.join("bosun")
        } else if cfg!(target_os = "macos") {
            root.join("Library")
                .join("Application Support")
                .join("bosun")
        } else {
            root.join(".config").join("bosun")
        };
        assert_eq!(default_path_in(&env), Some(expected.join("config.toml")));

        let explicit = PathBuf::from(abs("elsewhere/bosun.toml"));
        let env = |name: &str| (name == "BOSUN_CONFIG").then(|| explicit.clone().into());
        assert_eq!(default_path_in(&env), Some(explicit.clone()));

        assert_eq!(default_path_in(&|_| None), None);
    }
}
