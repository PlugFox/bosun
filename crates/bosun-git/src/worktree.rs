//! The worktree of an item (#8, section Worktrees): one worktree per item at a short path under
//! the repository's worktree root, on a branch of its own, behind a disk floor, and a quarantine
//! for a worktree that cannot be removed.
//!
//! # Creation
//!
//! [`create`] refuses to start when the disk holding the root has less free space than the
//! floor (read through [`FreeSpace`], which the caller supplies: this crate may not depend on
//! `bosun-proc`). Otherwise it adds `<root>/<item>` on the branch `bosun/<item>`, started from
//! the head of the base ref, and reports the commit it started from.
//!
//! # Removal and quarantine
//!
//! [`remove`] runs `git worktree remove --force` a few times with a short backoff: on Windows a
//! file held open by an editor, an indexer or a scanner blocks deletion for a while. Then the
//! branch is deleted. When the directory still cannot be removed it is not renamed (a locked
//! file blocks a rename too): it stays in place, a marker `<root>/<item>.quarantined` is written
//! beside it, the path is never reused, and [`Removal::Quarantined`] reports the directory, the
//! file that blocked it and the error. The worktree stays registered with git and its branch
//! stays checked out in it, so neither is removed.
//!
//! All git runs through [`Git::command`], hooks disabled.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::Git;

/// The branch of an item: `bosun/<item>`.
const BRANCH_PREFIX: &str = "bosun/";
/// The extension of the marker a quarantined worktree leaves beside it.
const QUARANTINED: &str = "quarantined";
/// Removal attempts before a worktree is quarantined.
const ATTEMPTS: u32 = 4;
/// The pause before the second attempt; each following pause doubles.
const BACKOFF: Duration = Duration::from_millis(50);

/// The free space of a disk. A trait so the floor can be tested, and so the caller supplies the
/// platform's disk query.
pub trait FreeSpace {
    /// The bytes free on the disk holding `path`.
    ///
    /// # Errors
    ///
    /// The disk cannot be queried.
    fn free(&self, path: &Path) -> io::Result<u64>;
}

/// The worktree of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The worktree's directory: `<root>/<item>`.
    pub path: PathBuf,
    /// The branch the item's commits land on: `bosun/<item>`.
    pub branch: String,
    /// The commit the branch started from: the head of the base ref at creation.
    pub base: String,
}

/// What [`remove`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Removal {
    /// The directory and the branch are gone.
    Removed,
    /// The directory could not be removed and stays in place; see [`Quarantine`].
    Quarantined(Quarantine),
}

/// A worktree left in place because its directory could not be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    /// The worktree's directory, still there.
    pub path: PathBuf,
    /// The first file in the directory that cannot be opened, which is what blocks it; `None`
    /// when none could be found.
    pub blocked_by: Option<PathBuf>,
    /// What the last removal attempt reported.
    pub error: String,
    /// The marker written beside the directory: `<root>/<item>.quarantined`.
    pub marker: PathBuf,
}

/// What can go wrong creating or removing a worktree.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The item id cannot name a directory and a branch: letters, digits, `-` and `_`, up to 64.
    BadItem {
        /// The id given.
        item: String,
    },
    /// The base ref names no commit.
    UnknownBase {
        /// The ref given.
        base: String,
    },
    /// The disk under the root has less free space than the floor. No worktree is created.
    BelowFloor {
        /// The root whose disk was read.
        root: PathBuf,
        /// The free bytes on that disk.
        free: u64,
        /// The floor in bytes.
        floor: u64,
    },
    /// The path is taken: a directory is there, or a quarantine marker for it. A quarantined
    /// path is never reused.
    PathTaken {
        /// The path.
        path: PathBuf,
    },
    /// A file or directory could not be read or written.
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// A git command failed.
    Git(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadItem { item } => write!(
                f,
                "item {item:?} cannot name a worktree: use letters, digits, `-` and `_`, up to 64"
            ),
            Self::UnknownBase { base } => write!(f, "base {base:?} names no commit"),
            Self::BelowFloor { root, free, floor } => write!(
                f,
                "{}: {free} bytes free, below the disk floor of {floor} bytes; no worktree was created",
                root.display()
            ),
            Self::PathTaken { path } => write!(
                f,
                "{} is taken: a worktree or a quarantine marker is there",
                path.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Git(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Git(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Git(error)
    }
}

/// Creates the worktree of `item` at `<root>/<item>` on the branch `bosun/<item>`, started from
/// the head of `base_ref`, in the repository `git`.
///
/// # Errors
///
/// The item id or the base ref is unusable, the disk under `root` has less than `floor` bytes
/// free, the path is taken (also by a quarantined worktree), the disk cannot be read, or a git
/// command fails. On a failure no worktree is created.
pub fn create(
    git: &Git,
    root: &Path,
    item: &str,
    base_ref: &str,
    space: &dyn FreeSpace,
    floor: u64,
) -> Result<Worktree, Error> {
    if !valid_item(item) {
        return Err(Error::BadItem {
            item: item.to_owned(),
        });
    }
    let path = root.join(item);
    if path.exists() || path.with_extension(QUARANTINED).exists() {
        return Err(Error::PathTaken { path });
    }
    fs::create_dir_all(root).map_err(|source| io_error(root, source))?;
    // Mutant (`mutant_git_worktree_skip_disk_floor`): the floor is never read, so a full disk
    // gets a worktree.
    if !cfg!(mutant_git_worktree_skip_disk_floor) {
        let free = space.free(root).map_err(|source| io_error(root, source))?;
        if free < floor {
            return Err(Error::BelowFloor {
                root: root.to_owned(),
                free,
                floor,
            });
        }
    }
    if base_ref.starts_with('-') {
        return Err(Error::UnknownBase {
            base: base_ref.to_owned(),
        });
    }
    let revision = format!("{base_ref}^{{commit}}");
    let base = git
        .optional(&["rev-parse", "--verify", "--quiet", revision.as_str()])?
        .ok_or_else(|| Error::UnknownBase {
            base: base_ref.to_owned(),
        })?;
    let branch = format!("{BRANCH_PREFIX}{item}");
    git.run(
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            branch.as_str(),
            text(&path)?,
            base.as_str(),
        ],
        None,
    )?;
    Ok(Worktree { path, branch, base })
}

/// Removes `worktree`: `git worktree remove --force`, retried with a backoff, then its branch.
/// A directory that still cannot be removed is quarantined instead of removed.
///
/// # Errors
///
/// A git command fails after the directory is gone, or the quarantine marker cannot be written.
pub fn remove(git: &Git, worktree: &Worktree) -> Result<Removal, Error> {
    let path = text(&worktree.path)?;
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            thread::sleep(BACKOFF * 2u32.pow(attempt - 1));
        }
        match git.run(&["worktree", "remove", "--force", path], None) {
            Err(error) => last = error.to_string(),
            Ok(_) => {
                "git reported success, but the directory is still there".clone_into(&mut last);
            }
        }
        if !worktree.path.exists() {
            break;
        }
    }
    if !worktree.path.exists() {
        // Prune the entry of a directory that is gone, so the branch is no longer checked out.
        git.run(&["worktree", "prune"], None)?;
        git.run(&["branch", "-D", worktree.branch.as_str()], None)?;
        return Ok(Removal::Removed);
    }
    // Mutant (`mutant_git_worktree_unremoved_is_removed`): a failed removal is reported as
    // removed, and the directory is left behind with nothing said about it.
    if cfg!(mutant_git_worktree_unremoved_is_removed) {
        return Ok(Removal::Removed);
    }
    quarantine(worktree, last)
}

/// Leaves the directory of `worktree` in place, writes its marker and reports it.
fn quarantine(worktree: &Worktree, error: String) -> Result<Removal, Error> {
    let marker = worktree.path.with_extension(QUARANTINED);
    let blocked_by = blocker(&worktree.path);
    let blocked = blocked_by
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), |file| file.display().to_string());
    let note = format!(
        "path: {}\nbranch: {}\nblocked by: {blocked}\nerror: {error}\n",
        worktree.path.display(),
        worktree.branch,
    );
    fs::write(&marker, note).map_err(|source| io_error(&marker, source))?;
    Ok(Removal::Quarantined(Quarantine {
        path: worktree.path.clone(),
        blocked_by,
        error,
        marker,
    }))
}

/// The first file under `dir` that cannot be opened: a file held open without sharing, as an
/// editor's or indexer's handle is.
fn blocker(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if let Some(found) = blocker(&path) {
                return Some(found);
            }
        } else if kind.is_file() && fs::File::open(&path).is_err() {
            return Some(path);
        }
    }
    None
}

/// An item id that is safe as a directory name and in a branch name.
fn valid_item(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= 64
        && item
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `path` as the text git takes.
fn text(path: &Path) -> Result<&str, Error> {
    path.to_str().ok_or_else(|| Error::Io {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "the path is not UTF-8"),
    })
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    /// Identity and signing for the commits the tests make; the invoker keeps hooks off.
    const IDENTITY: [&str; 8] = [
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgSign=false",
        "-c",
        "tag.gpgSign=false",
    ];

    /// A free-space probe that reports a fixed number of bytes.
    struct Fixed(u64);

    impl FreeSpace for Fixed {
        fn free(&self, _: &Path) -> io::Result<u64> {
            Ok(self.0)
        }
    }

    /// A repository on `main` with one commit, in a temporary directory.
    fn repository() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "--quiet", "--initial-branch=main"], None)
            .unwrap();
        commit(&git, "a.txt", "Initial commit");
        dir
    }

    /// Appends a line to `file` and commits everything with `message`. Returns the sha.
    fn commit(git: &Git, file: &str, message: &str) -> String {
        let path = git.repo().join(file);
        let old = fs::read_to_string(&path).unwrap_or_default();
        fs::write(&path, format!("{old}{file}\n")).unwrap();
        git.run(&["add", "--all"], None).unwrap();
        let mut args = IDENTITY.to_vec();
        args.extend(["commit", "--quiet", "-m", message]);
        git.run(&args, None).unwrap();
        git.run(&["rev-parse", "HEAD"], None)
            .unwrap()
            .trim()
            .to_owned()
    }

    /// How many worktrees git lists, the repository itself included.
    fn worktrees(git: &Git) -> usize {
        git.run(&["worktree", "list", "--porcelain"], None)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count()
    }

    #[test]
    fn no_worktree_is_created_below_the_disk_floor() {
        let repo = repository();
        let root = tempfile::tempdir().unwrap();
        let git = Git::new(repo.path());

        let err = create(&git, root.path(), "card-1", "main", &Fixed(10), 100).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("10 bytes free"), "{text}");
        assert!(text.contains("floor of 100 bytes"), "{text}");
        assert!(!root.path().join("card-1").exists());
        assert_eq!(worktrees(&git), 1);

        // At the floor, the disk is enough.
        let made = create(&git, root.path(), "card-1", "main", &Fixed(100), 100).unwrap();
        assert!(made.path.exists());
    }

    #[test]
    fn a_worktree_is_created_from_the_base_ref_and_removed_with_its_branch() {
        let repo = repository();
        let root = tempfile::tempdir().unwrap();
        let git = Git::new(repo.path());
        let second = commit(&git, "b.txt", "Second commit");

        let made = create(&git, root.path(), "card-1", "main", &Fixed(u64::MAX), 0).unwrap();
        assert_eq!(made.path, root.path().join("card-1"));
        assert_eq!(made.branch, "bosun/card-1");
        assert_eq!(made.base, second, "the head of the base ref is recorded");
        assert!(made.path.join("b.txt").exists());
        assert!(
            git.optional(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/bosun/card-1"
            ])
            .unwrap()
            .is_some()
        );

        assert_eq!(remove(&git, &made).unwrap(), Removal::Removed);
        assert!(!made.path.exists());
        assert!(
            git.optional(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/bosun/card-1"
            ])
            .unwrap()
            .is_none()
        );
        assert_eq!(worktrees(&git), 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_worktree_with_a_file_held_open_is_quarantined_and_reported() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let repo = repository();
        let root = tempfile::tempdir().unwrap();
        let git = Git::new(repo.path());
        let held_wt = create(&git, root.path(), "card-1", "main", &Fixed(u64::MAX), 0).unwrap();
        let held = held_wt.path.join("held.txt");
        fs::write(&held, "open\n").unwrap();
        // No sharing at all: any other open of the file fails, as an editor's handle would.
        let handle = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&held)
            .unwrap();

        let removal = remove(&git, &held_wt).unwrap();
        let Removal::Quarantined(report) = removal else {
            panic!("the worktree with a held file was removed: {removal:?}");
        };
        assert_eq!(report.path, held_wt.path);
        assert_eq!(report.blocked_by.as_deref(), Some(held.as_path()));
        assert_ne!(report.error, "");
        assert!(
            held_wt.path.exists(),
            "a quarantined directory stays in place"
        );
        assert!(report.marker.exists());

        // The quarantined path is never reused, and the next item still gets a worktree.
        assert!(matches!(
            create(&git, root.path(), "card-1", "main", &Fixed(u64::MAX), 0),
            Err(Error::PathTaken { .. })
        ));
        let next = create(&git, root.path(), "card-2", "main", &Fixed(u64::MAX), 0).unwrap();
        assert!(next.path.exists());

        drop(handle);
    }
}
