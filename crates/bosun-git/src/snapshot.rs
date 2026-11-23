//! The ignored-file and git-configuration snapshots (#7 "Ignored files", "Git's own
//! configuration", #59).
//!
//! An agent that adds a line to `.gitignore` can leave behind the file that makes the verdict
//! pass, and the clean-tree precondition does not see it. An agent that plants a hook, edits
//! `.git/config` or an `include.path` target, or edits `info/exclude` or `info/attributes`
//! changes what git does without a commit. Both are compared with a snapshot before the
//! verdict, and any difference is Needs you.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::refusal::{Refusal, covers};
use crate::{Error, Git};

/// The ignored files of a working tree, recorded before a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignored {
    /// Every ignored file, repository-relative and `/`-separated (git's spelling). Files inside
    /// an ignored directory are listed one by one.
    pub paths: BTreeSet<String>,
}

impl Ignored {
    /// Records the ignored files of the repository of `git`.
    ///
    /// # Errors
    ///
    /// A git command fails.
    pub fn capture(git: &Git) -> Result<Self, Error> {
        // `--untracked-files=all` lists every file inside an ignored directory (and overrides a
        // `status.showUntrackedFiles` the repository may set), so a new file in a directory
        // that was already ignored counts too.
        let out = git.run(
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--ignored=traditional",
                "--untracked-files=all",
                "--no-renames",
            ],
            None,
        )?;
        let paths = out
            .split('\0')
            .filter_map(|entry| entry.strip_prefix("!! "))
            .map(str::to_owned)
            .collect();
        Ok(Self { paths })
    }
}

/// The ignored files that appeared since `before` outside the `build_dirs` (repository-relative
/// directories where new ignored files are expected, `repo.<name>.build_dirs`); one
/// [`Refusal::NewIgnored`] listing them, or nothing. Nothing is deleted.
///
/// # Errors
///
/// A git command fails.
pub fn ignored(
    git: &Git,
    before: &Ignored,
    build_dirs: &BTreeSet<String>,
) -> Result<Vec<Refusal>, Error> {
    let now = Ignored::capture(git)?;
    let paths: Vec<String> = now
        .paths
        .difference(&before.paths)
        .filter(|path| !build_dirs.iter().any(|dir| covers(dir, path)))
        .cloned()
        .collect();
    // Mutant: a new ignored file is a warning nobody reads, not a refusal.
    if paths.is_empty() || cfg!(mutant_git_ignored_is_warning) {
        return Ok(Vec::new());
    }
    Ok(vec![Refusal::NewIgnored { paths }])
}

/// Git's own configuration of a repository, recorded at claim: every file that changes what git
/// does without a commit, with the SHA-256 of its content (`None` for a file that a
/// configuration names but that does not exist, so creating it counts as a change).
///
/// The files are `.git/config`, `config.worktree`, every other configuration file the local and
/// worktree scopes read (the `include.path` and `includeIf.*.path` targets, followed), every
/// such target named whether it exists or not, `info/exclude`, `info/attributes`, and every file
/// in the hooks directories: `.git/hooks` and the directory a repository's own `core.hooksPath`
/// names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitConfig {
    /// Each file and the SHA-256 (lowercase hex) of its content.
    pub files: BTreeMap<PathBuf, Option<String>>,
}

impl GitConfig {
    /// Records git's configuration of the repository of `git`.
    ///
    /// # Errors
    ///
    /// A git command fails or prints something unexpected, or a file cannot be read.
    pub fn capture(git: &Git) -> Result<Self, Error> {
        let dirs = git.run(
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--git-dir",
                "--show-toplevel",
            ],
            None,
        )?;
        let mut lines = dirs.lines().map(PathBuf::from);
        let (Some(common), Some(own), Some(top)) = (lines.next(), lines.next(), lines.next())
        else {
            return Err(git.unexpected(format!("the git directories {dirs:?}")));
        };

        let mut paths = BTreeSet::new();
        paths.insert(common.join("config"));
        paths.insert(own.join("config.worktree"));
        paths.insert(common.join("info").join("exclude"));
        paths.insert(common.join("info").join("attributes"));

        let mut hooks = vec![common.join("hooks")];
        for entry in config_entries(git)? {
            if !matches!(entry.scope.as_str(), "local" | "worktree") {
                continue;
            }
            let origin = entry
                .origin
                .strip_prefix("file:")
                .map(|file| absolute(git.repo(), Path::new(file)));
            if let Some(origin) = &origin {
                paths.insert(origin.clone());
            }
            let key = entry.key.to_ascii_lowercase();
            let include = key == "include.path"
                || (key.starts_with("includeif.")
                    && key.rsplit_once('.').is_some_and(|(_, last)| last == "path"));
            match (include, entry.value, &origin) {
                (true, Some(value), Some(origin)) => {
                    let base = origin.parent().unwrap_or(Path::new(""));
                    paths.insert(expand(base, &value));
                }
                (false, Some(value), _) if key == "core.hookspath" => {
                    hooks.push(expand(&top, &value));
                }
                _ => {}
            }
        }
        for dir in hooks {
            // A missing hooks directory holds no hook; a planted one appears as new files.
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    paths.insert(path);
                }
            }
        }

        let mut files = BTreeMap::new();
        for path in paths.into_iter().map(|p| slashes(&p)) {
            let digest = digest(git, &path)?;
            files.insert(path, digest);
        }
        Ok(Self { files })
    }
}

/// The files of git's configuration that differ from `before`: one
/// [`Refusal::ConfigChanged`] per created, changed or deleted file.
///
/// # Errors
///
/// A git command fails or prints something unexpected, or a file cannot be read.
pub fn config(git: &Git, before: &GitConfig) -> Result<Vec<Refusal>, Error> {
    if cfg!(mutant_git_skip_config_snapshot) {
        return Ok(Vec::new());
    }
    let mut now = GitConfig::capture(git)?;
    // A file named at claim and no longer named now (its include line was removed) is still
    // compared: the removal shows in the file that held the line, and the target's own state
    // is read again here.
    for path in before.files.keys() {
        if !now.files.contains_key(path.as_path()) {
            let digest = digest(git, path)?;
            now.files.insert(path.clone(), digest);
        }
    }
    let mut refusals = Vec::new();
    for (path, after) in &now.files {
        let before = before.files.get(path).cloned().flatten();
        if &before != after {
            refusals.push(Refusal::ConfigChanged {
                path: path.clone(),
                before,
                after: after.clone(),
            });
        }
    }
    Ok(refusals)
}

/// One configuration entry, as `git config --list --show-scope --show-origin -z` prints it.
struct Entry {
    scope: String,
    origin: String,
    key: String,
    value: Option<String>,
}

/// Every configuration entry git reads in the repository, includes followed.
fn config_entries(git: &Git) -> Result<Vec<Entry>, Error> {
    let out = git.run(
        &["config", "--list", "--show-scope", "--show-origin", "-z"],
        None,
    )?;
    let mut fields = out.split('\0');
    let mut entries = Vec::new();
    while let Some(scope) = fields.next() {
        if scope.is_empty() {
            continue;
        }
        let (Some(origin), Some(pair)) = (fields.next(), fields.next()) else {
            return Err(git.unexpected(format!("a configuration entry in scope {scope:?}")));
        };
        // A key with no `=` in the file has no value and no newline.
        let (key, value) = match pair.split_once('\n') {
            Some((key, value)) => (key, Some(value.to_owned())),
            None => (pair, None),
        };
        entries.push(Entry {
            scope: scope.to_owned(),
            origin: origin.to_owned(),
            key: key.to_owned(),
            value,
        });
    }
    Ok(entries)
}

/// `path` with `/` separators on Windows, where git prints `C:/...` and `Path::join` adds `\`:
/// one spelling per file, so the same file is not compared twice under two names.
fn slashes(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().replace('\\', "/"))
    } else {
        path.to_owned()
    }
}

/// `path` made absolute against `base`.
fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

/// A pathname value of git's configuration resolved as git does: `~/` is the home directory,
/// a relative path is relative to `base`.
fn expand(base: &Path, value: &str) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let (Some(rest), Some(home)) = (value.strip_prefix("~/"), home) {
        return PathBuf::from(home).join(rest);
    }
    absolute(base, Path::new(value))
}

/// The SHA-256 of the file at `path`, `None` when there is no file there.
fn digest(git: &Git, path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hex = String::with_capacity(64);
            for byte in Sha256::digest(&bytes) {
                let _ = write!(hex, "{byte:02x}");
            }
            Ok(Some(hex))
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::NotADirectory =>
        {
            Ok(None)
        }
        // A directory where a file is expected: not a configuration git reads.
        Err(_) if path.is_dir() => Ok(None),
        Err(e) => Err(git.unexpected(format!("cannot read {}: {e}", path.display()))),
    }
}
