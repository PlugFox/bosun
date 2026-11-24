//! Item worktrees (#8 "Worktrees"): one git worktree per item, on its own branch, under the
//! repository's worktree root.
//!
//! - [`create`] makes the worktree at a short path named by the item id (Windows path limits),
//!   on a new branch from the head of a base ref, and records the base commit. Below the disk
//!   floor it makes nothing: free space is read through [`FreeSpace`], injected because this
//!   crate may not depend on `bosun-proc` (#12).
//! - [`remove`] runs `git worktree remove --force`, retried with a short backoff (on Windows a
//!   file held open by an editor, an indexer or a scanner blocks deletion for a while), then
//!   deletes the branch. A worktree that still cannot be removed is quarantined: a directory
//!   with a locked file cannot be renamed either, so it is left in place with a marker next to
//!   it, its path is never used again, git's entry for it is pruned when possible, and the
//!   caller gets a [`Quarantine`] report naming the file that blocked. A worktree is never left
//!   behind silently.
//! - Nothing is deleted that is not an item worktree: [`remove`] deletes only a directory that
//!   git itself lists as a worktree (`git worktree list --porcelain`), other than the
//!   repository's own checkout, strictly under the worktree root once links are resolved, and
//!   not a link itself; anything else is refused ([`Error::Refused`]) before git runs.
//!
//! Every git command goes through [`Git::command`], so no repository hook runs.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Git;

/// The prefix of an item's branch name.
pub const BRANCH_PREFIX: &str = "bosun/";

/// The suffix of the marker file written next to a quarantined worktree.
pub const QUARANTINE_SUFFIX: &str = ".quarantined";

/// The longest worktree directory name taken from an item id, in characters.
const MAX_NAME: usize = 40;

/// How many times `remove` tries before it quarantines, and the first pause between tries
/// (doubled after each).
const ATTEMPTS: u32 = 4;
const FIRST_PAUSE: Duration = Duration::from_millis(100);

/// Reads the free space of the volume a path is on.
pub trait FreeSpace {
    /// The bytes free to the user on the volume holding `path`.
    ///
    /// # Errors
    ///
    /// The free space cannot be read.
    fn free(&self, path: &Path) -> std::io::Result<u64>;
}

/// The free space below which no worktree is made.
#[derive(Clone, Copy)]
pub struct Floor<'a> {
    /// The floor, in bytes.
    pub bytes: u64,
    /// Where the free space is read.
    pub space: &'a dyn FreeSpace,
}

impl fmt::Debug for Floor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Floor").field("bytes", &self.bytes).finish()
    }
}

/// An item's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Its directory.
    pub path: PathBuf,
    /// The short name of its branch (`bosun/<name>`).
    pub branch: String,
    /// The commit the branch was made at: the head of the base ref at creation.
    pub base: String,
}

/// What [`remove`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// The worktree and its branch are gone.
    Removed,
    /// The worktree could not be removed and is quarantined.
    Quarantined(Quarantine),
}

/// A worktree left in place because it could not be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    /// The worktree's directory, which is never used again.
    pub path: PathBuf,
    /// The marker written next to it.
    pub marker: PathBuf,
    /// The file that could not be deleted, when one was found.
    pub blocking: Option<PathBuf>,
    /// Why it could not be deleted.
    pub error: String,
}

/// Why a worktree was not made, or not removed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The free space under the root is below the floor.
    BelowFloor {
        /// The worktree root.
        root: PathBuf,
        /// The free bytes.
        free: u64,
        /// The floor, in bytes.
        floor: u64,
    },
    /// A file system operation failed.
    Io {
        /// What was operated on.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
    },
    /// The path is not one [`remove`] may delete: it is not an item worktree git lists under
    /// the root. Nothing was touched.
    Refused {
        /// The path.
        path: PathBuf,
        /// Why it is not an item worktree.
        reason: &'static str,
    },
    /// A git command failed.
    Git(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BelowFloor { root, free, floor } => write!(
                f,
                "{}: {free} bytes free, below the disk floor of {floor} bytes; no worktree made",
                root.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Refused { path, reason } => {
                write!(f, "{}: not removed: {reason}", path.display())
            }
            Self::Git(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BelowFloor { .. } | Self::Refused { .. } => None,
            Self::Io { source, .. } => Some(source),
            Self::Git(error) => Some(error),
        }
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Git(error)
    }
}

/// Makes `item`'s worktree under `root`, on a new branch from the head of `base_ref` of `repo`.
///
/// The directory is the item id with every character other than an ASCII letter, digit, `-`,
/// `_` or `.` replaced by `-`, cut to 40 characters; when that path, its quarantine marker or
/// its branch already exists, `-2`, `-3`, ... is appended, so a path is never reused.
///
/// # Errors
///
/// The free space under `root` cannot be read or is below `floor` (nothing is made then), the
/// root cannot be made, `base_ref` names no commit, or git fails.
pub fn create(
    repo: &Git,
    root: &Path,
    item: &str,
    base_ref: &str,
    floor: Floor<'_>,
) -> Result<Worktree, Error> {
    std::fs::create_dir_all(root).map_err(|source| Error::Io {
        path: root.to_owned(),
        source,
    })?;
    let free = floor.space.free(root).map_err(|source| Error::Io {
        path: root.to_owned(),
        source,
    })?;
    // Mutant (`mutant_git_worktree_skip_disk_floor`): the floor is not checked.
    if free < floor.bytes && !cfg!(mutant_git_worktree_skip_disk_floor) {
        return Err(Error::BelowFloor {
            root: root.to_owned(),
            free,
            floor: floor.bytes,
        });
    }

    let base = repo
        .run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{base_ref}^{{commit}}"),
            ],
            None,
        )?
        .trim()
        .to_owned();
    let stem = name(item);
    let mut n = 1u32;
    let (path, branch) = loop {
        let name = if n == 1 {
            stem.clone()
        } else {
            format!("{stem}-{n}")
        };
        let path = root.join(&name);
        let branch = format!("{BRANCH_PREFIX}{name}");
        let taken = path.exists()
            || marker(&path).exists()
            || repo
                .optional(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])?
                .is_some();
        if !taken {
            break (path, branch);
        }
        n += 1;
    };
    let target = path.to_string_lossy();
    repo.run(
        &["worktree", "add", "--quiet", "-b", &branch, &target, &base],
        None,
    )?;
    Ok(Worktree { path, branch, base })
}

/// Removes `worktree` of `repo`, made under `root`, and its branch, or quarantines it (see the
/// module documentation).
///
/// # Errors
///
/// [`Error::Refused`] (nothing touched) when the path is not an item worktree git lists under
/// `root`; otherwise the worktree is gone but its branch cannot be deleted, or the quarantine
/// marker cannot be written.
pub fn remove(repo: &Git, root: &Path, worktree: &Worktree) -> Result<Removal, Error> {
    // Checked once, before git drops its entry: after that the path is no longer listed.
    let present = guard(repo, root, &worktree.path)?;
    let target = worktree.path.to_string_lossy();
    let mut failure = None;
    let mut pause = FIRST_PAUSE;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(pause);
            pause *= 2;
        }
        let Err(git) = repo.run(
            &["worktree", "remove", "--force", "--end-of-options", &target],
            None,
        ) else {
            failure = None;
            break;
        };
        // Git drops its own entry even when files are left behind; the rest is deleted here,
        // which also finds the file that blocks. A path that was not there has nothing to
        // delete.
        let cleared = if present {
            clear(&worktree.path)
        } else {
            Ok(())
        };
        match cleared {
            Ok(()) => {
                failure = None;
                break;
            }
            Err((file, error)) => failure = Some((git, file, error)),
        }
    }
    // An entry whose directory is gone (or whose `.git` file is) goes; a failure here leaves
    // only a stale entry, which the next prune drops.
    let _ = repo.run(&["worktree", "prune"], None);

    let Some((git, file, error)) = failure else {
        repo.run(
            &[
                "branch",
                "--quiet",
                "-D",
                "--end-of-options",
                &worktree.branch,
            ],
            None,
        )?;
        return Ok(Removal::Removed);
    };
    // Mutant (`mutant_git_worktree_unremoved_is_removed`): a failed removal is reported as
    // removed.
    if cfg!(mutant_git_worktree_unremoved_is_removed) {
        return Ok(Removal::Removed);
    }
    // The branch goes when git no longer counts it as checked out; otherwise it stays, and
    // its name, like the path, is never used again.
    let _ = repo.run(
        &[
            "branch",
            "--quiet",
            "-D",
            "--end-of-options",
            &worktree.branch,
        ],
        None,
    );
    let error = format!("{}: {error} ({git})", file.display());
    let marker = marker(&worktree.path);
    let text = format!(
        "This worktree could not be removed and is quarantined: bosun never uses this path again.\n\
         branch: {}\nbase: {}\nblocking: {}\nerror: {error}\n",
        worktree.branch,
        worktree.base,
        file.display()
    );
    std::fs::write(&marker, text).map_err(|source| Error::Io {
        path: marker.clone(),
        source,
    })?;
    Ok(Removal::Quarantined(Quarantine {
        path: worktree.path.clone(),
        marker,
        blocking: Some(file).filter(|f| f != &worktree.path),
        error,
    }))
}

/// Whether `path` may be deleted as an item worktree of `repo` under `root`: `Ok(true)` when
/// it is a directory, not a link, strictly under `root` once links are resolved, listed by git
/// as a worktree other than the repository's own checkout, and holding neither that checkout
/// nor another worktree nor the git directory; `Ok(false)` when there is nothing at `path`.
///
/// # Errors
///
/// [`Error::Refused`] for anything else, or the paths or git's list cannot be read.
fn guard(repo: &Git, root: &Path, path: &Path) -> Result<bool, Error> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    // Mutant (`mutant_git_worktree_clear_unlisted`): any directory is deleted.
    if cfg!(mutant_git_worktree_clear_unlisted) {
        return Ok(true);
    }
    let refuse = |reason| {
        Err(Error::Refused {
            path: path.to_owned(),
            reason,
        })
    };
    // A symbolic link or a junction (the standard library counts a junction as a link) points
    // somewhere else.
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return refuse("it is a link or a file, not a worktree directory");
    }
    let real = canonical(path)?;
    let root = canonical(root)?;
    if real == root || !real.starts_with(&root) {
        return refuse("it is not inside the worktree root");
    }
    let common = repo
        .run(
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
        )?
        .trim()
        .to_owned();
    if canonical(Path::new(&common))?.starts_with(&real) {
        return refuse("it holds the repository's git directory");
    }
    let list = repo.run(&["worktree", "list", "--porcelain"], None)?;
    let mut listed = false;
    // The first entry is the repository's own checkout (or the bare repository).
    for (i, entry) in list
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .enumerate()
    {
        // An entry whose directory is gone can be neither the directory at `path` nor in it.
        let Ok(entry) = std::fs::canonicalize(entry) else {
            continue;
        };
        if i == 0 && entry.starts_with(&real) {
            return refuse("it is or holds the repository's own checkout");
        }
        if entry == real {
            listed = true;
        } else if entry.starts_with(&real) {
            return refuse("it holds another worktree");
        }
    }
    if !listed {
        return refuse("git does not list it as a worktree");
    }
    Ok(true)
}

/// `path` with every link resolved.
fn canonical(path: &Path) -> Result<PathBuf, Error> {
    std::fs::canonicalize(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

/// The quarantine marker of the worktree at `path`.
fn marker(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(QUARANTINE_SUFFIX);
    path.with_file_name(name)
}

/// The directory name for `item`.
fn name(item: &str) -> String {
    let name: String = item
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(MAX_NAME)
        .collect();
    // `.` and `..` name other directories, and Windows drops trailing dots.
    let name = name.trim_matches('.');
    if name.is_empty() {
        "item".to_owned()
    } else {
        name.to_owned()
    }
}

/// Deletes `path` and everything under it, file by file; returns the first entry that cannot
/// be deleted and why. A path that does not exist is cleared.
fn clear(path: &Path) -> Result<(), (PathBuf, std::io::Error)> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err((path.to_owned(), e)),
    };
    if meta.file_type().is_symlink() {
        // The link goes, never what it points at; on Windows a link to a directory is removed
        // as a directory.
        return std::fs::remove_file(path)
            .or_else(|_| std::fs::remove_dir(path))
            .map_err(|e| (path.to_owned(), e));
    }
    if meta.is_dir() {
        let entries = std::fs::read_dir(path).map_err(|e| (path.to_owned(), e))?;
        let mut first = None;
        for entry in entries {
            let entry = entry.map_err(|e| (path.to_owned(), e))?;
            if let Err(failure) = clear(&entry.path()) {
                first.get_or_insert(failure);
            }
        }
        if let Some(failure) = first {
            return Err(failure);
        }
        std::fs::remove_dir(path).map_err(|e| (path.to_owned(), e))
    } else {
        std::fs::remove_file(path).or_else(|e| {
            // A read-only file cannot be deleted on Windows until it is made writable.
            if meta.permissions().readonly() {
                let mut writable = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                writable.set_readonly(false);
                std::fs::set_permissions(path, writable)
                    .and_then(|()| std::fs::remove_file(path))
                    .map_err(|e| (path.to_owned(), e))
            } else {
                Err((path.to_owned(), e))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::NO_HOOKS;

    /// A [`FreeSpace`] that reports a fixed number of bytes.
    struct Fixed(u64);

    impl FreeSpace for Fixed {
        fn free(&self, _: &Path) -> std::io::Result<u64> {
            Ok(self.0)
        }
    }

    /// Runs `git args` in `dir` through the invoker, with no user or system configuration and
    /// no signing. Returns the trimmed output.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Git::new(dir)
            .command()
            .args(["-c", "commit.gpgSign=false"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "user.email=test@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Writes `file` and commits it. Returns the sha.
    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "--quiet", "-m", text]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// A temporary directory holding a repository on `main` with one commit (`repo`) and a
    /// worktree root next to it (`wt`).
    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        commit(&repo, "a.txt", "first");
        let root = dir.path().join("wt");
        (dir, repo, root)
    }

    const PLENTY: Fixed = Fixed(u64::MAX);

    fn floor(space: &Fixed, bytes: u64) -> Floor<'_> {
        Floor { bytes, space }
    }

    /// The worktree paths git knows of, the main checkout included.
    fn worktrees(repo: &Path) -> usize {
        git(repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    #[test]
    fn no_worktree_is_created_below_the_disk_floor() {
        let (_dir, repo, root) = setup();
        let low = Fixed(1_000);

        let error = create(
            &Git::new(&repo),
            &root,
            "repo#7",
            "main",
            floor(&low, 2_000),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                Error::BelowFloor {
                    free: 1_000,
                    floor: 2_000,
                    ..
                }
            ),
            "{error:?}"
        );
        let text = error.to_string();
        assert!(
            text.contains("1000 bytes free") && text.contains("2000 bytes"),
            "{text}"
        );
        assert!(!root.join("repo-7").exists());
        assert_eq!(worktrees(&repo), 1);
        assert_eq!(git(&repo, &["branch", "--list", "bosun/*"]), "");

        // At the floor it is made.
        let made = create(
            &Git::new(&repo),
            &root,
            "repo#7",
            "main",
            floor(&low, 1_000),
        )
        .unwrap();
        assert!(made.path.is_dir());
    }

    #[test]
    fn a_worktree_is_made_on_its_own_branch_at_the_base_and_removed_with_it() {
        let (_dir, repo, root) = setup();
        let head = commit(&repo, "b.txt", "second");
        git(&repo, &["checkout", "--quiet", "-b", "other"]);
        commit(&repo, "c.txt", "elsewhere");
        let git_repo = Git::new(&repo);

        let made = create(&git_repo, &root, "repo#12", "main", floor(&PLENTY, 1)).unwrap();
        assert_eq!(made.path, root.join("repo-12"));
        assert_eq!(made.branch, "bosun/repo-12");
        assert_eq!(made.base, head);
        assert_eq!(git(&made.path, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(&made.path, &["symbolic-ref", "--short", "HEAD"]),
            "bosun/repo-12"
        );
        assert!(made.path.join("b.txt").is_file());
        assert!(!made.path.join("c.txt").exists());
        assert_eq!(worktrees(&repo), 2);

        assert_eq!(remove(&git_repo, &root, &made).unwrap(), Removal::Removed);
        assert!(!made.path.exists());
        assert!(!marker(&made.path).exists());
        assert_eq!(worktrees(&repo), 1);
        assert_eq!(git(&repo, &["branch", "--list", "bosun/*"]), "");
    }

    #[test]
    fn a_base_ref_that_names_no_commit_makes_nothing() {
        let (_dir, repo, root) = setup();
        let error = create(&Git::new(&repo), &root, "x", "missing", floor(&PLENTY, 1)).unwrap_err();
        assert!(matches!(error, Error::Git(_)), "{error:?}");
        assert_eq!(worktrees(&repo), 1);
    }

    #[test]
    fn a_taken_path_is_never_reused() {
        let (_dir, repo, root) = setup();
        std::fs::create_dir_all(root.join("a-1")).unwrap();
        std::fs::write(root.join("a-1-2.quarantined"), "").unwrap();
        let made = create(&Git::new(&repo), &root, "a/1", "main", floor(&PLENTY, 1)).unwrap();
        assert_eq!(made.path, root.join("a-1-3"));
        assert_eq!(made.branch, "bosun/a-1-3");
    }

    /// Makes `link` a link to the directory `to`: a junction on Windows (which needs no
    /// privilege), a symbolic link elsewhere.
    fn link_dir(to: &Path, link: &Path) {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(to)
                .output()
                .expect("cmd runs");
            assert!(
                out.status.success(),
                "mklink: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(to, link).unwrap();
    }

    #[test]
    fn only_a_worktree_git_lists_under_the_root_is_deleted() {
        let (dir, repo, root) = setup();
        let git_repo = Git::new(&repo);
        let made = create(&git_repo, &root, "repo#1", "main", floor(&PLENTY, 1)).unwrap();
        let as_worktree = |path: &Path| Worktree {
            path: path.to_owned(),
            branch: made.branch.clone(),
            base: made.base.clone(),
        };
        let refused = |root: &Path, path: &Path| {
            let error = remove(&git_repo, root, &as_worktree(path)).unwrap_err();
            assert!(
                matches!(error, Error::Refused { .. }),
                "{path:?}: {error:?}"
            );
        };

        // A directory under the root that git does not list.
        let stray = root.join("stray");
        std::fs::create_dir(&stray).unwrap();
        std::fs::write(stray.join("keep.txt"), "mine").unwrap();
        refused(&root, &stray);
        assert!(stray.join("keep.txt").is_file());

        // The repository's own checkout, and a parent of it, with the root above them.
        refused(dir.path(), &repo);
        refused(dir.path().parent().unwrap(), dir.path());
        assert!(repo.join("a.txt").is_file());

        // The root itself, which holds the worktree.
        refused(&root, &root);

        // A link under the root to a listed worktree: deleting through it would reach the
        // worktree, which is not what the caller named.
        let link = root.join("link");
        link_dir(&made.path, &link);
        refused(&root, &link);
        assert!(made.path.join("a.txt").is_file());

        // The worktree, from a root it is not under.
        refused(&stray, &made.path);
        assert!(made.path.join("a.txt").is_file());

        // Nothing above touched the worktree or its branch.
        assert_eq!(worktrees(&repo), 2);
        assert_eq!(
            git(
                &repo,
                &["branch", "--list", "--format=%(refname:short)", "bosun/*"]
            ),
            "bosun/repo-1"
        );
        std::fs::remove_dir(&link)
            .or_else(|_| std::fs::remove_file(&link))
            .unwrap();
        assert_eq!(remove(&git_repo, &root, &made).unwrap(), Removal::Removed);
        assert!(stray.join("keep.txt").is_file());
    }

    #[test]
    fn an_item_id_becomes_a_short_safe_name() {
        assert_eq!(name("owner/repo#123"), "owner-repo-123");
        assert_eq!(name(".."), "item");
        assert_eq!(name(&"x".repeat(100)).len(), MAX_NAME);
    }

    #[cfg(windows)]
    #[test]
    fn a_worktree_with_a_file_held_open_is_quarantined_and_reported() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let (_dir, repo, root) = setup();
        let git_repo = Git::new(&repo);
        let made = create(&git_repo, &root, "repo#1", "main", floor(&PLENTY, 1)).unwrap();
        let held = made.path.join("a.txt");
        // Held as an editor holds it: no sharing at all, deletion included.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .unwrap();

        let removal = remove(&git_repo, &root, &made).unwrap();
        let Removal::Quarantined(report) = removal else {
            panic!("a worktree with a file held open was reported {removal:?}");
        };
        assert_eq!(report.path, made.path);
        assert_eq!(report.blocking.as_deref(), Some(held.as_path()));
        assert!(report.error.contains("a.txt"), "{}", report.error);
        assert!(report.marker.is_file());
        assert!(made.path.exists());

        // The next item gets its worktree; the same item never gets the quarantined path.
        let next = create(&git_repo, &root, "repo#2", "main", floor(&PLENTY, 1)).unwrap();
        assert!(next.path.join("a.txt").is_file());
        let again = create(&git_repo, &root, "repo#1", "main", floor(&PLENTY, 1)).unwrap();
        assert_ne!(again.path, made.path);

        drop(handle);
        assert_eq!(remove(&git_repo, &root, &next).unwrap(), Removal::Removed);
        assert_eq!(remove(&git_repo, &root, &again).unwrap(), Removal::Removed);
    }
}
