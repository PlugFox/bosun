//! The daemon's side of item worktrees (#8 "Worktrees", E5, E14, E19): for a repository whose
//! configuration has `repo.<name>.worktree`, the daemon's own clone, the worktrees made from
//! it, and their rows in the store. Without that key there is no [`Workspace`] and nothing
//! here runs.
//!
//! - The clone ([`Workspace::ensure_clone`]) is a bare clone at `<worktree.root>/.clone` whose
//!   origin is the user's checkout (`repo.path`): worktrees share refs, `refs/stash` and the
//!   git configuration with their repository, so they never hang off the user's own (E5). It
//!   is never `--shared` and never `--reference`: it owns its objects. Anything else found at
//!   its path is an error and is left as it is.
//! - A bare clone copies none of the user's repository-local configuration, and the daemon
//!   copies only one setting: on Windows the clone gets `core.longpaths=true` (at the clone,
//!   and in its own config; an existing clone without it gets it when checked), so a
//!   repository whose files sit deeper than 260 characters can have worktrees even when the
//!   user turned it on in that repository only. Nothing else is copied: not its filters,
//!   hooks, `core.autocrlf`, LFS or submodule settings (filters and hooks run commands, and
//!   git's own configuration is guarded, #59). What the user set globally or for the system
//!   applies to the clone as to any repository, except the line-ending policy; a repository
//!   that needs more prepares its worktrees with `setup`.
//! - The clone's own config pins the line-ending policy to git's defaults
//!   (`core.autocrlf=false`, `core.eol=native`; [`EOL_PIN`]), set the way `core.longpaths`
//!   is. An item's worktree is written by the daemon's git, which reads the machine's system
//!   and global configuration, and judged by the agent's git, which reads neither (the
//!   identity's file is its global one, and `GIT_CONFIG_NOSYSTEM=1`): with Git for Windows'
//!   system `core.autocrlf=true`, the daemon checked out CRLF files that the agent's git saw
//!   as modified, and every rebase and commit in the session failed. The clone's config is
//!   the one scope both read; the repository's own `.gitattributes` still decides.
//! - The one other exception: when `git lfs version` succeeds, the clone's own config pins Git
//!   LFS's filter as `git lfs install` writes it ([`LFS_PIN`]: `filter.lfs.clean`, `smudge`,
//!   `process` and `required`, and the `lfs.repositoryformatversion` git-lfs itself writes on
//!   first use, which would otherwise change the guarded configuration during a session), set
//!   the way the line-ending policy is, on an existing clone too. These are fixed values the daemon writes, never the user's configuration, so the
//!   daemon's git, the agent's and the verdict's clean and smudge the files the repository's
//!   `.gitattributes` routes through Git LFS whatever their global configuration says. Without
//!   git-lfs nothing is pinned, and a checkout of such a file holds its pointer.
//! - Before a worktree or a verdict checkout is made, the Git LFS objects of its commit's
//!   pointers that the user's repository has are carried from the user's store into the
//!   clone's ([`lfs::carry`]), so the smudge needs no remote. One the user's repository lacks
//!   too is asked of a remote by git-lfs, and a checkout that cannot have it fails as git
//!   says. Landing carries the landed commit's objects the other way ([`super::landing`]).
//! - The target is fetched from the user's checkout at claim ([`Workspace::sync_target`]),
//!   fast-forward only: a target the user rewrote is an error, not an overwrite. It is never
//!   checked out in the clone.
//! - An item's worktree ([`Workspace::open`]) is made by [`bosun_git::worktree::create`] from
//!   the clone, below no disk floor, and recorded ([`Write::Worktree`]). The configured
//!   `setup` command runs in it ([`Workspace::setup`]) the way a verdict command runs; a
//!   failure is the caller's Needs you, not a failed attempt, and leaves no worktree behind
//!   ([`Workspace::open_with_setup`]). The disk floor is read through [`super::Deps`]'s
//!   `free_space` ([`Disk`] in the daemon).
//! - `setup` and `teardown` end at once when the task's kill is raised (a cancel, the kill
//!   switch): a killed command is [`Unopened::Killed`], not a failed setup.
//! - Closing it ([`Workspace::close`]) keeps the item's work in the clone when asked (under
//!   [`super::git::aside_ref`]), runs `teardown`, measures it, and removes it with its branch;
//!   one that cannot be removed is quarantined, recorded with the file that blocked, and
//!   reported once: never left behind silently, and its path is never used again.
//! - The child of a running `setup` or `teardown` is recorded on the worktree's row while it
//!   runs (E3), so a daemon that dies meanwhile leaves it to be reaped.
//! - The sweep ([`Workspace::sweep`]), at the end of crash recovery and before every claim,
//!   finds what no item in flight owns: it reaps a recorded `setup` or `teardown` child the
//!   loop did not start itself, closes every `created` worktree no item owns (an item's with
//!   its work kept), retries every quarantined one ([`worktree::retry_quarantine`]), and
//!   removes a worktree git lists on a `bosun/` branch with no row at all (a daemon that died
//!   between making it and recording it), its work kept under the item's aside ref or else
//!   under [`super::git::orphan_ref`]. What it closes is reported once.
//!
//! - With `worktree.shared_build` (E1), every process the daemon starts in one of the
//!   repository's worktrees (setup and teardown, sessions, verdict runs) gets the shared build
//!   directory as its variable ([`build_env`]), and the worktree's tracked files are made newer
//!   than any build first ([`enter`]): the build tools judge freshness by modification time
//!   only, so a worktree older than the last build, made from another worktree's sources,
//!   would otherwise be taken as built (the E1 measurement).
//!
//! The git work is blocking and synchronous; the functions that also write to the store or run
//! a command are async and move their git work to the blocking pool ([`super::blocking`]).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_config::{Identity, Repo};
use bosun_core::{ItemId, PathStyle, canonical_path};
use bosun_git::worktree::{self, Floor, FreeSpace, Quarantine, Removal, Worktree};
use bosun_git::{Git, lfs};
use bosun_proc::ProcessKey;
use bosun_queue::lint::verdict_shell;
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::{NewWorktree, WorktreeKind, WorktreeRow, WorktreeState, Write};
use tokio::sync::{oneshot, watch};

use super::{Shared, Signal, blocking, identity, now_ms};

/// The name of the daemon's clone under the worktree root.
pub(crate) const CLONE: &str = ".clone";

/// What one loop holds of its worktrees for the sweep: the worktrees it opened and has not
/// closed, the `setup` and `teardown` children it runs, and the lock a sweep holds (and
/// [`Workspace::open`] from making a worktree to recording it, so a sweep never takes a
/// worktree being made for one without a row).
#[derive(Debug, Default)]
pub(crate) struct Registry {
    lock: tokio::sync::Mutex<()>,
    open: Mutex<BTreeSet<PathBuf>>,
    children: Mutex<BTreeSet<(u32, u64)>>,
}

impl Registry {
    fn opened(&self, path: &Path) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned());
    }

    fn closed(&self, path: &Path) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path);
    }

    fn open_paths(&self) -> Vec<PathBuf> {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn child(&self, key: ProcessKey, running: bool) {
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        if running {
            children.insert((key.pid, key.start.raw()));
        } else {
            children.remove(&(key.pid, key.start.raw()));
        }
    }

    fn runs(&self, key: ProcessKey) -> bool {
        self.children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(key.pid, key.start.raw()))
    }
}

/// Whether the daemon's clone gets `core.longpaths=true`: on Windows only.
fn long_paths() -> bool {
    // Mutant (`mutant_loop_clone_without_longpaths`): the clone does not get it.
    cfg!(windows) && !cfg!(mutant_loop_clone_without_longpaths)
}

/// The line-ending policy the daemon's clone pins in its own config, and so every item
/// worktree and verdict checkout made from it: git's defaults, whatever the machine's system
/// or global configuration says (see the module documentation).
const EOL_PIN: [(&str, &str); 2] = [("core.autocrlf", "false"), ("core.eol", "native")];

/// Sets [`EOL_PIN`] in the clone's own config where it holds another value or none; a clone
/// already pinned is not written to (its config is part of the guarded git configuration).
fn pin_eol(git: &Git) -> Result<(), Error> {
    // Mutant (`mutant_loop_clone_without_eol_pin`): the clone keeps the machine's policy.
    if cfg!(mutant_loop_clone_without_eol_pin) {
        return Ok(());
    }
    pin(git, &EOL_PIN)
}

/// Git LFS's filter as `git lfs install` writes it, pinned in the clone's own config when
/// git-lfs is installed ([`pin_lfs`]): fixed values, never the user's. With it the key git-lfs
/// itself writes into a repository's config the first time it runs there
/// (`lfs.repositoryformatversion`): written by the agent's first clean, it would change the
/// guarded configuration during the session (#59) and refuse the attempt.
const LFS_PIN: [(&str, &str); 5] = [
    ("filter.lfs.clean", "git-lfs clean -- %f"),
    ("filter.lfs.smudge", "git-lfs smudge -- %f"),
    ("filter.lfs.process", "git-lfs filter-process"),
    ("filter.lfs.required", "true"),
    ("lfs.repositoryformatversion", "0"),
];

/// Whether `git lfs version` succeeds on this machine; asked once.
pub(super) fn git_lfs_installed(git: &Git) -> bool {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(|| git.run(&["lfs", "version"], None).is_ok())
}

/// Sets [`LFS_PIN`] in the clone's own config the way [`pin_eol`] sets its keys, when git-lfs
/// is installed; without it nothing is pinned.
fn pin_lfs(git: &Git) -> Result<(), Error> {
    // Mutant (`mutant_loop_clone_without_lfs_filter`): the clone pins no LFS filter.
    if cfg!(mutant_loop_clone_without_lfs_filter) || !git_lfs_installed(git) {
        return Ok(());
    }
    pin(git, &LFS_PIN)
}

/// Sets each of `keys` in `git`'s own config where it holds another value or none.
fn pin(git: &Git, keys: &[(&str, &str)]) -> Result<(), Error> {
    for &(key, value) in keys {
        let local = git.optional(&["config", "--local", "--get", key])?;
        if local.as_deref().map(str::trim) != Some(value) {
            git.run(&["config", key, value], None)?;
        }
    }
    Ok(())
}

/// The worktrees of one repository configured with `repo.<name>.worktree`.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    /// The user's checkout: the clone's origin.
    user: PathBuf,
    /// The repository's identity's name.
    identity: String,
    /// `repo.<name>.worktree`.
    config: bosun_config::Worktree,
    /// `repo.<name>.verdict.shell`.
    shell: Option<PathBuf>,
    /// `repo.<name>.verdict.timeout`: the bound of `setup` and `teardown`.
    timeout: Duration,
}

/// Why the clone, the target or a worktree is not there.
#[derive(Debug)]
pub(crate) enum Error {
    /// What is at the clone's path is not the daemon's clone of this repository. It is left
    /// as it is.
    NotTheClone {
        /// The clone's path.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// A file system operation failed.
    Io {
        /// What was operated on.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
    },
    /// A git command failed.
    Git(bosun_git::Error),
    /// The Git LFS objects of the commit to check out cannot be carried into the clone.
    Lfs(lfs::Error),
    /// The worktree was not made (below the disk floor among others), or not removed.
    Worktree(worktree::Error),
    /// The worktree's row cannot be written; the worktree was removed again.
    Store(bosun_store::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTheClone { path, reason } => write!(
                f,
                "{}: not the daemon's clone of the repository ({reason}); left as it is",
                path.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Git(e) => e.fmt(f),
            Self::Lfs(e) => write!(
                f,
                "the Git LFS objects cannot be carried into the daemon's clone: {e}"
            ),
            Self::Worktree(e) => e.fmt(f),
            Self::Store(e) => write!(f, "the worktree cannot be recorded: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotTheClone { .. } => None,
            Self::Io { source, .. } => Some(source),
            Self::Git(e) => Some(e),
            Self::Lfs(e) => Some(e),
            Self::Worktree(e) => Some(e),
            Self::Store(e) => Some(e),
        }
    }
}

impl From<bosun_git::Error> for Error {
    fn from(e: bosun_git::Error) -> Self {
        Self::Git(e)
    }
}

impl From<lfs::Error> for Error {
    fn from(e: lfs::Error) -> Self {
        Self::Lfs(e)
    }
}

impl From<worktree::Error> for Error {
    fn from(e: worktree::Error) -> Self {
        Self::Worktree(e)
    }
}

/// The `setup` command failed: the item is the user's to look at (Needs you), not a failed
/// attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupFailed {
    /// The command.
    pub(crate) command: String,
    /// How it failed: its exit, its timeout, or why it could not run.
    pub(crate) reason: String,
    /// The bounded tail of its output, both streams interleaved.
    pub(crate) tail: String,
}

impl SetupFailed {
    /// The command and how it ended, without its output.
    pub(crate) fn cause(&self) -> String {
        format!("`{}` {}", self.command, self.reason)
    }
}

impl fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` {}", self.command, self.reason)?;
        if !self.tail.trim().is_empty() {
            write!(f, "; its output ends:\n{}", self.tail.trim_end())?;
        }
        Ok(())
    }
}

/// Why [`Workspace::open_with_setup`] made no worktree, or why a `setup` did not end well.
#[derive(Debug)]
pub(crate) enum Unopened {
    /// It was not made.
    Error(Error),
    /// It was made, its setup failed, and it was closed again.
    Setup(SetupFailed),
    /// The task's kill was raised while the command ran: it was ended, and the worktree (if
    /// one was made) closed again. Not a failed setup.
    Killed,
}

/// What [`Workspace::close`] did. None of these stops the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Closed {
    /// The worktree and its branch are gone.
    Removed,
    /// The worktree could not be removed: it is quarantined, recorded and reported.
    Quarantined(Quarantine),
    /// Nothing was removed: the item's work could not be kept, or the worktree is not one
    /// that may be removed. Reported; its row stays `created`, so it is found again.
    Kept(String),
}

/// A throwaway checkout of one commit, for a verdict run ([`Workspace::checkout`]): a worktree
/// of kind `verdict`, set up, closed by [`Checkout::close`] once the run is over.
#[derive(Debug)]
pub(crate) struct Checkout {
    workspace: Workspace,
    worktree: Worktree,
}

impl Checkout {
    /// Its directory: the files of the commit, plus what `setup` made.
    pub(crate) fn path(&self) -> &Path {
        &self.worktree.path
    }

    /// Removes it with its branch, or quarantines it (reported by [`Workspace::close`]);
    /// `kill`: the run's, which ends its `teardown`.
    pub(crate) async fn close(
        self,
        shared: &Shared,
        identity: &Identity,
        kill: Option<&mut watch::Receiver<Signal>>,
    ) -> Closed {
        self.workspace
            .close(shared, identity, &self.worktree, None, kill)
            .await
    }
}

/// The volume's free space, as the system reports it: the daemon's [`super::Deps`]
/// `free_space`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Disk;

impl FreeSpace for Disk {
    fn free(&self, path: &Path) -> std::io::Result<u64> {
        bosun_proc::disk::free_space(path)
    }
}

/// No floor: the free space is not read.
struct Unbounded;

impl FreeSpace for Unbounded {
    fn free(&self, _: &Path) -> std::io::Result<u64> {
        Ok(u64::MAX)
    }
}

impl Workspace {
    /// The workspace of `repo`; `None` without `repo.<name>.worktree` (E19).
    pub(crate) fn of(repo: &Repo) -> Option<Self> {
        let config = repo.worktree.clone()?;
        Some(Self {
            user: repo.path.clone(),
            identity: repo.identity.clone(),
            config,
            shell: repo.verdict.shell.as_deref().map(PathBuf::from),
            timeout: repo.verdict.timeout,
        })
    }

    /// The workspace an item claimed in worktree mode was recorded in, for a configuration
    /// that no longer has `repo.<name>.worktree` (#8): `root` (the parent of the item's
    /// worktree) with its clone at `<root>/.clone`, and no `setup`, no `teardown`, no disk
    /// floor. Enough to close the item's worktree with its work kept in the clone.
    pub(crate) fn recorded(repo: &Repo, root: PathBuf) -> Self {
        Self {
            user: repo.path.clone(),
            identity: repo.identity.clone(),
            config: bosun_config::Worktree::at(root),
            shell: repo.verdict.shell.as_deref().map(PathBuf::from),
            timeout: repo.verdict.timeout,
        }
    }

    /// The worktree root: `repo.<name>.worktree.root`.
    pub(crate) fn root(&self) -> &Path {
        &self.config.root
    }

    /// The path of the daemon's clone.
    pub(crate) fn clone_path(&self) -> PathBuf {
        self.config.root.join(CLONE)
    }

    fn clone_git(&self) -> Git {
        Git::new(self.clone_path())
    }

    /// The daemon's bare clone at `<root>/.clone`: made from the user's checkout when missing,
    /// checked when present.
    ///
    /// # Errors
    ///
    /// The root cannot be made, `git clone` fails, or what is at the path is not a bare
    /// repository whose origin is the user's checkout ([`Error::NotTheClone`]; it is left as
    /// it is).
    pub(crate) fn ensure_clone(&self) -> Result<Git, Error> {
        let path = self.clone_path();
        match std::fs::symlink_metadata(&path) {
            Ok(_) => self.check_clone(&path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let root = &self.config.root;
                std::fs::create_dir_all(root).map_err(|source| Error::Io {
                    path: root.clone(),
                    source,
                })?;
                let user = self.user.to_string_lossy();
                let mut args = Vec::new();
                if long_paths() {
                    args.extend(["-c", "core.longpaths=true"]);
                }
                args.extend(["clone", "--bare", "--quiet", "--", &user, CLONE]);
                Git::new(root).run(&args, None)?;
                let git = Git::new(path);
                if long_paths() {
                    git.run(&["config", "core.longpaths", "true"], None)?;
                }
                pin_eol(&git)?;
                pin_lfs(&git)?;
                Ok(git)
            }
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// `path` is the daemon's clone of the user's checkout.
    fn check_clone(&self, path: &Path) -> Result<Git, Error> {
        let not = |reason: String| Error::NotTheClone {
            path: path.to_owned(),
            reason,
        };
        let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(not("it is a link or a file".to_owned()));
        }
        let git = Git::new(path);
        // A plain directory may sit inside some other repository: git would answer for that
        // one, so its git directory must be this very directory.
        let dir = git
            .optional(&["rev-parse", "--absolute-git-dir"])
            .ok()
            .flatten()
            .map(|d| d.trim().to_owned())
            .ok_or_else(|| not("it is not a git repository".to_owned()))?;
        let same = match (std::fs::canonicalize(&dir), std::fs::canonicalize(path)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            return Err(not("it is not a git repository of its own".to_owned()));
        }
        if git
            .run(&["rev-parse", "--is-bare-repository"], None)?
            .trim()
            != "true"
        {
            return Err(not("it is not a bare repository".to_owned()));
        }
        let origin = git
            .optional(&["config", "--get", "remote.origin.url"])?
            .map(|u| u.trim().to_owned())
            .ok_or_else(|| not("it has no origin".to_owned()))?;
        let style = PathStyle::host();
        let user = self.user.to_string_lossy();
        if canonical_path(&origin, style) != canonical_path(&user, style) {
            return Err(not(format!("its origin is {origin}, not {user}")));
        }
        // A clone made before the setting was: it gets it now.
        if long_paths()
            && git
                .optional(&["config", "--local", "--get", "core.longpaths"])?
                .is_none_or(|v| v.trim() != "true")
        {
            git.run(&["config", "core.longpaths", "true"], None)?;
        }
        // A clone made before the pin was: it gets it now, once.
        pin_eol(&git)?;
        pin_lfs(&git)?;
        Ok(git)
    }

    /// Fetches the user's `target` branch into the clone's own `refs/heads/<target>`, fast
    /// forward only, and returns the commit it names.
    ///
    /// # Errors
    ///
    /// The fetch fails: the branch is missing, or the user rewrote it (no forced update).
    pub(crate) fn sync_target(&self, target: &str) -> Result<String, Error> {
        let git = self.clone_git();
        // No leading `+`: a rewritten target is refused, never forced over the clone's.
        let refspec = format!("refs/heads/{target}:refs/heads/{target}");
        git.run(
            &["fetch", "--quiet", "--no-tags", "--", "origin", &refspec],
            None,
        )?;
        Ok(git
            .run(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &format!("refs/heads/{target}^{{commit}}"),
                ],
                None,
            )?
            .trim()
            .to_owned())
    }

    /// The blocking part of [`Workspace::open`]; `space` reads the free space of the disk
    /// floor.
    fn create(&self, name: &str, at: &str, space: &dyn FreeSpace) -> Result<Worktree, Error> {
        // Mutant (`mutant_loop_worktree_in_user_repo`): the worktree hangs off the user's
        // checkout.
        let source = if cfg!(mutant_loop_worktree_in_user_repo) {
            Git::new(&self.user)
        } else {
            self.carry_lfs_in(at)?;
            self.clone_git()
        };
        let made = match self.config.disk_floor {
            Some(bytes) => {
                worktree::create(&source, &self.config.root, name, at, Floor { bytes, space })
            }
            None => worktree::create(
                &source,
                &self.config.root,
                name,
                at,
                Floor {
                    bytes: 0,
                    space: &Unbounded,
                },
            ),
        };
        Ok(made?)
    }

    /// Carries the Git LFS objects of the pointers of `at` (a commit or a ref of the clone)
    /// that the user's repository has from its store into the clone's, so the checkout's
    /// smudge needs no remote. A user's repository without a Git LFS store costs nothing more.
    fn carry_lfs_in(&self, at: &str) -> Result<(), Error> {
        let from = lfs::storage(&Git::new(&self.user))?;
        if !from.is_dir() {
            return Ok(());
        }
        let clone = self.clone_git();
        let pointers = lfs::pointers(&clone, at)?;
        if pointers.is_empty() {
            return Ok(());
        }
        let to = lfs::storage(&clone)?;
        lfs::carry(&from, &to, pointers.iter().map(|p| p.oid.as_str()))?;
        Ok(())
    }

    /// Makes `item`'s worktree, named `name`, at `at` (a commit or a ref of the clone), and
    /// records it, with `user_base`: the user's checkout at claim as JSON (E4), for an item's
    /// worktree.
    ///
    /// # Errors
    ///
    /// Below the disk floor, or git fails: nothing is made and nothing recorded. The row
    /// cannot be written: the worktree is removed again.
    pub(crate) async fn open(
        &self,
        shared: &Shared,
        item: &ItemId,
        name: &str,
        at: &str,
        kind: WorktreeKind,
        user_base: Option<String>,
    ) -> Result<Worktree, Error> {
        // No sweep looks between the worktree's making and its row.
        let _sweeps = shared.worktrees.lock.lock().await;
        // A path a row already names (a worktree removed since) is not made again: its row
        // would not be written twice.
        let recorded: BTreeSet<PathBuf> = shared
            .deps
            .reader
            .worktrees(item)
            .await
            .map_err(Error::Store)?
            .into_iter()
            .map(|r| PathBuf::from(r.path))
            .collect();
        let stem = worktree::dir_name(name);
        let name = (1..=u32::MAX)
            .map(|n| {
                if n == 1 {
                    stem.clone()
                } else {
                    format!("{stem}-{n}")
                }
            })
            .find(|n| !recorded.contains(&self.config.root.join(n)))
            .unwrap_or(stem);
        let this = self.clone();
        let at = at.to_owned();
        let space = shared.deps.free_space.clone();
        let made = blocking(move || this.create(&name, &at, space.as_ref())).await?;
        let row = Write::Worktree(NewWorktree {
            item: item.clone(),
            path: made.path.to_string_lossy().into_owned(),
            branch: made.branch.clone(),
            base_sha: made.base.clone(),
            kind,
            user_base_json: user_base,
            created_ms: now_ms(),
        });
        if let Err(e) = shared.deps.writer.write(vec![row]).await {
            let (this, wt) = (self.clone(), made.clone());
            let removed =
                blocking(move || worktree::remove(&this.clone_git(), &this.config.root, &wt)).await;
            if !matches!(removed, Ok(Removal::Removed)) {
                shared.trouble(format!(
                    "the unrecorded worktree {} was not removed: {removed:?}",
                    made.path.display()
                ));
            }
            return Err(Error::Store(e));
        }
        shared.worktrees.opened(&made.path);
        Ok(made)
    }

    /// Runs the configured `setup` command, if any, in `worktree`, until it ends or `kill` is
    /// raised.
    ///
    /// # Errors
    ///
    /// [`Unopened::Setup`]: it exited non-zero, timed out or could not run.
    /// [`Unopened::Killed`]: `kill` was raised and it was ended.
    pub(crate) async fn setup(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
        kill: &mut watch::Receiver<Signal>,
    ) -> Result<(), Unopened> {
        let Some(command) = &self.config.setup else {
            return Ok(());
        };
        let ran = self
            .run(shared, identity, &worktree.path, command, Some(kill))
            .await;
        // Mutant (`mutant_loop_setup_failure_ignored`): a failing setup counts as done.
        if cfg!(mutant_loop_setup_failure_ignored) && matches!(ran, Err(Unopened::Setup(_))) {
            return Ok(());
        }
        ran
    }

    /// [`Workspace::open`], then [`Workspace::setup`]; a worktree whose setup failed or was
    /// killed is closed again.
    ///
    /// # Errors
    ///
    /// It was not made, or its setup failed or was killed (and it is gone, or quarantined and
    /// reported).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_with_setup(
        &self,
        shared: &Shared,
        identity: &Identity,
        item: &ItemId,
        name: &str,
        at: &str,
        kind: WorktreeKind,
        user_base: Option<String>,
        kill: &mut watch::Receiver<Signal>,
    ) -> Result<Worktree, Unopened> {
        let made = self
            .open(shared, item, name, at, kind, user_base)
            .await
            .map_err(Unopened::Error)?;
        if let Err(unopened) = self.setup(shared, identity, &made, kill).await {
            self.close(shared, identity, &made, None, Some(kill)).await;
            return Err(unopened);
        }
        Ok(made)
    }

    /// A checkout of `item`'s commit `at`, named `name`, for a verdict run: the verdict of an
    /// attempt's head, or of a commit about to land. It has only what is committed at `at`,
    /// plus what the `setup` command makes (a fresh worktree lacks ignored build outputs and
    /// the like). Close it with [`Checkout::close`] however the run ends.
    ///
    /// # Errors
    ///
    /// It was not made, or its setup failed or was killed by `kill` (and it is gone, or
    /// quarantined and reported).
    pub(crate) async fn checkout(
        &self,
        shared: &Shared,
        identity: &Identity,
        item: &ItemId,
        name: &str,
        at: &str,
        kill: &mut watch::Receiver<Signal>,
    ) -> Result<Checkout, Unopened> {
        let worktree = if cfg!(mutant_loop_verdict_checkout_without_setup) {
            // Mutant (`mutant_loop_verdict_checkout_without_setup`): the verdict checkout is
            // not set up.
            self.open(shared, item, name, at, WorktreeKind::Verdict, None)
                .await
                .map_err(Unopened::Error)?
        } else {
            self.open_with_setup(
                shared,
                identity,
                item,
                name,
                at,
                WorktreeKind::Verdict,
                None,
                kill,
            )
            .await?
        };
        Ok(Checkout {
            workspace: self.clone(),
            worktree,
        })
    }

    /// Closes `worktree`: keeps `aside`'s work in the clone first when given, runs `teardown`
    /// (a failure, or its end by `kill`, is reported and the removal goes on), measures it, and
    /// removes it with its branch, or quarantines it. The row is ended either way. Whatever it
    /// did, the loop no longer holds the worktree: one [`Closed::Kept`] is the sweep's to try
    /// again.
    pub(crate) async fn close(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
        aside: Option<&ItemId>,
        kill: Option<&mut watch::Receiver<Signal>>,
    ) -> Closed {
        let closed = self
            .close_held(shared, identity, worktree, aside, kill)
            .await;
        shared.worktrees.closed(&worktree.path);
        closed
    }

    async fn close_held(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
        aside: Option<&ItemId>,
        kill: Option<&mut watch::Receiver<Signal>>,
    ) -> Closed {
        if let Some(item) = aside {
            let (dir, owned) = (worktree.path.clone(), item.clone());
            // The set-aside commit carries the repository identity (E26).
            let author = identity::bundle(&self.identity, identity, &dir)
                .map(|b| super::git::ident(&b.author));
            let kept = match author {
                Ok(author) => blocking(move || super::git::keep_work(&dir, &owned, &author))
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            if let Err(e) = kept {
                let error = format!(
                    "the work of {item} in {} cannot be kept, so the worktree is not removed: {e}",
                    worktree.path.display()
                );
                shared.trouble(error.clone());
                return Closed::Kept(error);
            }
        }
        self.teardown(shared, identity, worktree, kill).await;
        let (this, wt) = (self.clone(), worktree.clone());
        let (bytes, removed) = blocking(move || {
            let bytes = size(&wt.path);
            (
                bytes,
                worktree::remove(&this.clone_git(), &this.config.root, &wt),
            )
        })
        .await;
        let path = worktree.path.to_string_lossy().into_owned();
        let (closed, end) = match removed {
            Ok(Removal::Removed) => (
                Closed::Removed,
                Write::WorktreeEnded {
                    path,
                    state: WorktreeState::Removed,
                    blocking: None,
                    error: None,
                    bytes: Some(bytes),
                    at_ms: now_ms(),
                },
            ),
            Ok(Removal::Quarantined(q)) => {
                shared.trouble(format!(
                    "the worktree {} cannot be removed and is quarantined (blocked by {}): {}",
                    q.path.display(),
                    q.blocking
                        .as_deref()
                        .map_or_else(|| "an unknown file".into(), Path::to_string_lossy),
                    q.error
                ));
                // Mutant (`mutant_loop_quarantine_unrecorded`): the quarantine is recorded as a
                // removal.
                let state = if cfg!(mutant_loop_quarantine_unrecorded) {
                    WorktreeState::Removed
                } else {
                    WorktreeState::Quarantined
                };
                let end = Write::WorktreeEnded {
                    path,
                    state,
                    blocking: q
                        .blocking
                        .as_deref()
                        .map(|b| b.to_string_lossy().into_owned()),
                    error: Some(q.error.clone()),
                    bytes: Some(bytes),
                    at_ms: now_ms(),
                };
                (Closed::Quarantined(q), end)
            }
            Err(e) => {
                let error = format!(
                    "the worktree {} is not removed: {e}",
                    worktree.path.display()
                );
                shared.trouble(error.clone());
                return Closed::Kept(error);
            }
        };
        if let Err(e) = shared.deps.writer.write(vec![end]).await {
            shared.trouble(format!(
                "the end of the worktree {} cannot be recorded: {e}",
                worktree.path.display()
            ));
        }
        closed
    }

    /// Runs `teardown`, if any, in `worktree`; a failure, or its end by `kill`, is reported.
    async fn teardown(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
        kill: Option<&mut watch::Receiver<Signal>>,
    ) {
        if let Some(command) = &self.config.teardown {
            match self
                .run(shared, identity, &worktree.path, command, kill)
                .await
            {
                Ok(()) => {}
                Err(Unopened::Killed) => shared.trouble(format!(
                    "the teardown of the worktree {} was killed, and it is removed anyway",
                    worktree.path.display()
                )),
                Err(Unopened::Setup(failed)) => shared.trouble(format!(
                    "the teardown of the worktree {} failed, and it is removed anyway: {failed}",
                    worktree.path.display()
                )),
                Err(Unopened::Error(e)) => shared.trouble(format!(
                    "the teardown of the worktree {} failed, and it is removed anyway: {e}",
                    worktree.path.display()
                )),
            }
        }
    }

    /// Records `child` as the command running in the worktree at `path`, or none; a failure is
    /// reported.
    async fn record_child(&self, shared: &Shared, path: &str, child: Option<(u32, i64)>) {
        let write = Write::WorktreeChild {
            path: path.to_owned(),
            child,
        };
        if let Err(e) = shared.deps.writer.write(vec![write]).await {
            shared.trouble(format!(
                "the command running in the worktree {path} cannot be recorded: {e}"
            ));
        }
    }

    /// Closes what no item in flight owns (see the module documentation): the worktrees under
    /// this workspace's root other than `owned`, and, with `spare_open`, other than those this
    /// loop opened and has not closed (a task of the item in flight may be using one: at
    /// recovery). Never stops the caller: what fails is reported, and found again next time.
    pub(crate) async fn sweep(
        &self,
        shared: &Shared,
        identity: &Identity,
        owned: &[PathBuf],
        spare_open: bool,
    ) {
        // Mutant (`mutant_loop_no_worktree_sweep`): the sweep does nothing.
        if cfg!(mutant_loop_no_worktree_sweep) {
            return;
        }
        // Without the clone nothing was ever made here.
        if !self.clone_path().is_dir() {
            return;
        }
        let _sweeps = shared.worktrees.lock.lock().await;
        let mut kept: Vec<PathBuf> = owned.to_vec();
        if spare_open {
            kept.extend(shared.worktrees.open_paths());
        }
        let reader = &shared.deps.reader;
        let (created, quarantined) = match (
            reader.open_worktrees().await,
            reader.quarantined_worktrees().await,
        ) {
            (Ok(created), Ok(quarantined)) => (created, quarantined),
            (Err(e), _) | (_, Err(e)) => {
                shared.trouble(format!("the worktrees cannot be read for the sweep: {e}"));
                return;
            }
        };
        let ours = |rows: Vec<WorktreeRow>| -> Vec<WorktreeRow> {
            rows.into_iter()
                .filter(|r| self.holds(Path::new(&r.path)))
                .collect()
        };
        let (created, quarantined) = (ours(created), ours(quarantined));

        // 1. A command a dead daemon started is never resumed.
        for row in created.iter().chain(&quarantined) {
            self.reap_child(shared, row).await;
        }
        // 2. Every worktree no item owns is closed, an item's with its work kept.
        for row in &created {
            if kept.iter().any(|p| same_path(p, Path::new(&row.path))) {
                continue;
            }
            self.close_unowned(shared, identity, row).await;
        }
        // 3. A quarantined worktree is removed once it is free.
        // Mutant (`mutant_loop_quarantine_not_retried`): it is never tried again.
        if !cfg!(mutant_loop_quarantine_not_retried) {
            for row in &quarantined {
                self.retry(shared, row).await;
            }
        }
        // 4. A worktree made but never recorded.
        let recorded: Vec<PathBuf> = created
            .iter()
            .chain(&quarantined)
            .map(|r| PathBuf::from(&r.path))
            .chain(kept)
            .collect();
        self.remove_unrecorded(shared, identity, &recorded).await;
    }

    /// Whether `path` is under this workspace's root (rows carry no repository).
    fn holds(&self, path: &Path) -> bool {
        let root = self.root();
        if path.starts_with(root) {
            return path != root;
        }
        match (
            std::fs::canonicalize(root),
            path.parent().map(std::fs::canonicalize),
        ) {
            (Ok(root), Some(Ok(parent))) => parent.starts_with(root),
            _ => false,
        }
    }

    /// Step 1: kills the child `row` records, if it still runs and this loop did not start it
    /// (by pid and start time, as recovery reaps recorded children), and clears the record.
    async fn reap_child(&self, shared: &Shared, row: &WorktreeRow) {
        let Some((pid, started_ms)) = row.child else {
            return;
        };
        let key = ProcessKey {
            pid,
            start: bosun_proc::StartTime::from_raw(started_ms.cast_unsigned()),
        };
        if shared.worktrees.runs(key) {
            return;
        }
        let alive = shared.deps.alive.clone();
        if blocking(move || alive(key)).await {
            match blocking(move || bosun_proc::reap(key)).await {
                Ok(true) => crate::log::log!(
                    "bosund: repository {}: reaped pid {pid}, a command of a previous daemon in \
                     the worktree {}",
                    shared.options.repo,
                    row.path
                ),
                Ok(false) => {}
                Err(e) => {
                    shared.trouble(format!(
                        "pid {pid}, a command of a previous daemon in the worktree {}, cannot \
                         be reaped: {e}",
                        row.path
                    ));
                    return;
                }
            }
        }
        self.record_child(shared, &row.path, None).await;
    }

    /// Step 2: closes the worktree of `row`, which no item owns.
    async fn close_unowned(&self, shared: &Shared, identity: &Identity, row: &WorktreeRow) {
        let worktree = Worktree {
            path: PathBuf::from(&row.path),
            branch: row.branch.clone(),
            base: row.base_sha.clone(),
        };
        let what = match row.kind {
            WorktreeKind::Item => "the worktree",
            WorktreeKind::Verdict => "the verdict checkout",
        };
        shared.trouble(format!(
            "{what} {} of {} is closed: no item owns it",
            row.path, row.item
        ));
        if !worktree.path.exists() {
            // Already gone: its branch goes, and its row ends.
            let (git, branch) = (self.clone_git(), row.branch.clone());
            blocking(move || drop_branch(&git, &branch)).await;
            let end = Write::WorktreeEnded {
                path: row.path.clone(),
                state: WorktreeState::Removed,
                blocking: None,
                error: None,
                bytes: None,
                at_ms: now_ms(),
            };
            if let Err(e) = shared.deps.writer.write(vec![end]).await {
                shared.trouble(format!(
                    "the end of the worktree {} cannot be recorded: {e}",
                    row.path
                ));
            }
            return;
        }
        let aside = (row.kind == WorktreeKind::Item).then_some(&row.item);
        self.close(shared, identity, &worktree, aside, None).await;
    }

    /// Step 3: tries the quarantined worktree of `row` again; reported only once removed.
    async fn retry(&self, shared: &Shared, row: &WorktreeRow) {
        let (root, path) = (self.config.root.clone(), PathBuf::from(&row.path));
        let (git, branch) = (self.clone_git(), row.branch.clone());
        let retried = blocking(move || {
            let retried = worktree::retry_quarantine(&root, &path);
            if matches!(retried, Ok(Removal::Removed)) {
                drop_branch(&git, &branch);
            }
            retried
        })
        .await;
        match retried {
            Ok(Removal::Removed) => {
                let freed = Write::WorktreeFreed {
                    path: row.path.clone(),
                    at_ms: now_ms(),
                };
                if let Err(e) = shared.deps.writer.write(vec![freed]).await {
                    shared.trouble(format!(
                        "the removal of the quarantined worktree {} cannot be recorded: {e}",
                        row.path
                    ));
                } else {
                    shared.trouble(format!(
                        "the quarantined worktree {} of {} is removed now",
                        row.path, row.item
                    ));
                }
            }
            // Still blocked: it was reported when it was quarantined.
            Ok(Removal::Quarantined(_)) => {}
            Err(e) => crate::log::log!(
                "bosund: repository {}: the quarantined worktree {} is not retried: {e}",
                shared.options.repo,
                row.path
            ),
        }
    }

    /// Step 4: removes every worktree directly under the root that git lists for the clone on
    /// a `bosun/` branch and that no row in `recorded` names, its work kept first.
    async fn remove_unrecorded(&self, shared: &Shared, identity: &Identity, recorded: &[PathBuf]) {
        let (git, root) = (self.clone_git(), self.config.root.clone());
        let recorded = recorded.to_vec();
        let found = blocking(move || unrecorded(&git, &root, &recorded)).await;
        let found = match found {
            Ok(found) => found,
            Err(e) => {
                shared.trouble(format!("the clone's worktrees cannot be listed: {e}"));
                return;
            }
        };
        if found.is_empty() {
            return;
        }
        let items: Vec<ItemId> = match shared.deps.reader.items(false).await {
            Ok(items) => items.into_iter().map(|s| s.item.id).collect(),
            Err(e) => {
                shared.trouble(format!("the items cannot be read for the sweep: {e}"));
                return;
            }
        };
        for worktree in found {
            shared.trouble(format!(
                "the worktree {} on {} is closed: no item owns it (it was never recorded)",
                worktree.path.display(),
                worktree.branch
            ));
            let short = worktree
                .branch
                .strip_prefix(worktree::BRANCH_PREFIX)
                .unwrap_or(&worktree.branch);
            let owner = owner_of(short, &items);
            let (name, label) = match owner {
                Some(item) => (super::git::aside_ref(&item), item),
                None => (
                    super::git::orphan_ref(&worktree.branch),
                    ItemId::new(worktree.branch.clone()),
                ),
            };
            let author = identity::bundle(&self.identity, identity, &worktree.path)
                .map(|b| super::git::ident(&b.author));
            let (dir, kept_as) = (worktree.path.clone(), name.clone());
            let kept = match author {
                Ok(author) => {
                    blocking(move || super::git::keep_work_at(&dir, &kept_as, &label, &author))
                        .await
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(e),
            };
            if let Err(e) = kept {
                shared.trouble(format!(
                    "the work in {} cannot be kept under {name}, so it is not removed: {e}",
                    worktree.path.display()
                ));
                continue;
            }
            let (git, root, wt) = (self.clone_git(), self.config.root.clone(), worktree.clone());
            match blocking(move || worktree::remove(&git, &root, &wt)).await {
                Ok(Removal::Removed) => {}
                Ok(Removal::Quarantined(q)) => shared.trouble(format!(
                    "the unrecorded worktree {} cannot be removed and is quarantined: {}",
                    q.path.display(),
                    q.error
                )),
                Err(e) => shared.trouble(format!(
                    "the unrecorded worktree {} is not removed: {e}",
                    worktree.path.display()
                )),
            }
        }
    }

    /// Runs `command` in `dir` as a verdict command runs: the verdict shell, the agent
    /// environment, the verdict timeout, a bounded output. When `kill` is raised meanwhile the
    /// command's tree is ended ([`Unopened::Killed`]).
    ///
    /// # Errors
    ///
    /// [`Unopened::Setup`]: it exited non-zero, timed out or could not run.
    /// [`Unopened::Killed`]: it was ended by `kill`.
    async fn run(
        &self,
        shared: &Shared,
        identity: &Identity,
        dir: &Path,
        command: &str,
        kill: Option<&mut watch::Receiver<Signal>>,
    ) -> Result<(), Unopened> {
        let failed = |reason: String, tail: String| {
            Unopened::Setup(SetupFailed {
                command: command.to_owned(),
                reason,
                tail,
            })
        };
        let env = identity::bundle(&self.identity, identity, dir)
            .and_then(|b| {
                identity::agent_env(&b, &shared.options.work_dir).map_err(|e| e.to_string())
            })
            .map_err(|e| failed(format!("cannot run: {e}"), String::new()))?;
        let shell = verdict_shell(self.shell.as_deref()).ok_or_else(|| {
            failed(
                "cannot run: the verdict shell cannot be found".to_owned(),
                String::new(),
            )
        })?;
        let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> =
            env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect();
        // The shared build directory (E1), and the files made newer than its last build.
        if let Some(build) = self.config.shared_build.as_ref() {
            vars.push((build.env.clone().into(), build.dir.clone().into_os_string()));
            let owned = dir.to_owned();
            blocking(move || enter(&owned))
                .await
                .map_err(|e| failed(format!("cannot run: {e}"), String::new()))?;
        }
        let spec = VerdictSpec {
            shell,
            command: command.to_owned(),
            env: vars,
            cwd: dir.to_owned(),
            timeout: self.timeout,
            limits: Limits::default(),
            // A setup command has no not-ready outcome: anything but a pass fails it.
            unready_exit: None,
        };
        // The child is recorded on the worktree's row while it runs (E3): a daemon that dies
        // meanwhile leaves it to the next one's sweep.
        let (key_tx, key_rx) = oneshot::channel();
        let started = Arc::new(Mutex::new(None));
        let reporting = super::verdict::Reporting {
            inner: shared.deps.spawner.as_ref(),
            key: Mutex::new(Some(key_tx)),
            started: started.clone(),
        };
        let path = dir.to_string_lossy().into_owned();
        let record = async {
            if let Ok(key) = key_rx.await {
                shared.worktrees.child(key, true);
                // Mutant (`mutant_loop_setup_child_unrecorded`): the child is not recorded.
                if !cfg!(mutant_loop_setup_child_unrecorded) {
                    let child = Some((key.pid, key.start.raw().cast_signed()));
                    self.record_child(shared, &path, child).await;
                }
            }
        };
        let running =
            async { tokio::join!(bosun_runner::verdict::run(&reporting, &spec), record).0 };
        // Mutant (`mutant_loop_setup_ignores_kill`): the command runs to its end or its
        // timeout whatever the kill says.
        let kill = kill.filter(|_| !cfg!(mutant_loop_setup_ignores_kill));
        let ran = match kill {
            // The command's tree goes with the dropped future.
            Some(kill) => tokio::select! {
                ran = running => Some(ran),
                () = super::verdict::killed(kill) => None,
            },
            None => Some(running.await),
        };
        let key = *started.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = key {
            shared.worktrees.child(key, false);
            self.record_child(shared, &path, None).await;
        }
        let ran = ran
            .ok_or(Unopened::Killed)?
            .map_err(|e| failed(format!("cannot run: {e}"), String::new()))?;
        match ran.outcome {
            Outcome::Pass => Ok(()),
            // `Unready` cannot happen without `unready_exit`; it would be a failure.
            Outcome::Fail | Outcome::Unready => Err(failed(
                ran.exit.code.map_or_else(
                    || "was ended by a signal".to_owned(),
                    |c| format!("exited {c}"),
                ),
                ran.interleaved.text,
            )),
            Outcome::Timeout => Err(failed(
                format!("timed out after {:?}", self.timeout),
                ran.interleaved.text,
            )),
        }
    }
}

/// Whether `a` and `b` name the same path, links resolved when both exist.
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Deletes the item branch `branch` of the clone `git` if it is still there, after dropping
/// git's entries of worktrees that are gone (a branch checked out in one cannot be deleted). A
/// failure leaves the branch, whose name is then never used again.
fn drop_branch(git: &Git, branch: &str) {
    if !branch.starts_with(worktree::BRANCH_PREFIX) {
        return;
    }
    let _ = git.run(&["worktree", "prune"], None);
    let there = git
        .optional(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .ok()
        .flatten()
        .is_some();
    if there {
        let _ = git.run(
            &["branch", "--quiet", "-D", "--end-of-options", branch],
            None,
        );
    }
}

/// The worktrees `git worktree list` gives for the clone `git` directly under `root`, on a
/// `bosun/` branch, other than the clone and those `recorded` names.
fn unrecorded(git: &Git, root: &Path, recorded: &[PathBuf]) -> Result<Vec<Worktree>, Error> {
    let list = git.run(&["worktree", "list", "--porcelain"], None)?;
    let Ok(real_root) = std::fs::canonicalize(root) else {
        return Ok(Vec::new());
    };
    let recorded: Vec<PathBuf> = recorded
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    let mut found = Vec::new();
    // Entries are blank-line separated; the first is the bare clone itself.
    for entry in list.split("\n\n").skip(1) {
        let mut path = None;
        let mut branch = None;
        let mut head = String::new();
        for line in entry.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_owned());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                h.clone_into(&mut head);
            }
        }
        let (Some(path), Some(branch)) = (path, branch) else {
            continue;
        };
        if !branch.starts_with(worktree::BRANCH_PREFIX) {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&path) else {
            // Gone: the next prune drops git's entry.
            continue;
        };
        if real.parent() != Some(real_root.as_path())
            || real.file_name().is_some_and(|n| n == CLONE)
            || recorded.contains(&real)
        {
            continue;
        }
        found.push(Worktree {
            path: root.join(real.file_name().unwrap_or_default()),
            branch,
            base: head,
        });
    }
    Ok(found)
}

/// The item whose worktree branch `short` (the name after `bosun/`) is: the one of `items`
/// whose directory name it is, alone or with a `-<n>` suffix. `None` unless exactly one is.
fn owner_of(short: &str, items: &[ItemId]) -> Option<ItemId> {
    let mut owners = items.iter().filter(|item| {
        let stem = worktree::dir_name(&item.0);
        short == stem
            || short
                .strip_prefix(stem.as_str())
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    });
    let owner = owners.next()?;
    owners.next().is_none().then(|| owner.clone())
}

/// The shared build directory of `repo` (E1) as the variable a process gets: `(env, dir)`;
/// `None` without `worktree.shared_build`.
pub(crate) fn build_env(repo: &Repo) -> Option<(std::ffi::OsString, std::ffi::OsString)> {
    let build = repo.worktree.as_ref()?.shared_build.as_ref()?;
    Some((build.env.clone().into(), build.dir.clone().into_os_string()))
}

/// Under `repo`'s shared build (E1), before a process starts in the worktree at `dir`: adds
/// the directory's variable to `vars` when `inject`, and touches the worktree's tracked files
/// ([`enter`]). Nothing without `worktree.shared_build`.
///
/// # Errors
///
/// The files cannot be touched.
pub(crate) async fn shared_build(
    repo: &Repo,
    dir: &Path,
    vars: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>,
    inject: bool,
) -> Result<(), String> {
    let Some(build) = build_env(repo) else {
        return Ok(());
    };
    if inject {
        vars.push(build);
    }
    let dir = dir.to_owned();
    blocking(move || enter(&dir)).await
}

/// Sets the modification time of every tracked file of the worktree at `dir` to now (the E1
/// rule): what a build tool judging freshness by modification time sees as newer than the last
/// build of the shared directory, which may have come from another worktree. A tracked path
/// that is missing or not a regular file is skipped; untracked and ignored files are not
/// touched. Costs one rebuild of the workspace's own crates.
///
/// # Errors
///
/// `git ls-files` fails, or a tracked file's time cannot be set.
pub(crate) fn enter(dir: &Path) -> Result<(), String> {
    if cfg!(mutant_loop_skip_mtime_touch) {
        // Mutant (`mutant_loop_skip_mtime_touch`): nothing is touched.
        return Ok(());
    }
    let listed = Git::new(dir)
        .run(&["ls-files", "-z"], None)
        .map_err(|e| format!("the tracked files of {}: {e}", dir.display()))?;
    let now = std::time::SystemTime::now();
    for name in listed.split('\0').filter(|n| !n.is_empty()) {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        touch(&path, now).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Sets `path`'s modification time to `at`, opening it only to write its attributes.
fn touch(path: &Path, at: std::time::SystemTime) -> std::io::Result<()> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        /// `FILE_WRITE_ATTRIBUTES`: enough to set the times, also of a read-only file.
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path)?;
    file.set_modified(at)
}

/// The bytes of the files under `path`, links not followed; what cannot be read counts as
/// nothing.
fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::read_dir(path).map_or(0, |entries| {
            entries.flatten().map(|e| size(&e.path())).sum()
        })
    } else {
        meta.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use bosun_config::{Config, Identity, Repo, SystemProbe};
    use bosun_core::ItemId;
    use bosun_git::Git;
    use bosun_queue::LocalFile;
    use bosun_store::{
        NewItem, Options as StoreOptions, Store, WorktreeKind, WorktreeState, Write,
    };

    use super::super::tests::without_session;
    use super::super::{Deps, GhLogins, Options, Shared, SystemClock, board};
    use super::*;
    use crate::events::{Events, Options as EventOptions};

    /// This file itself holds no raw NUL byte: one makes ripgrep and GitHub's viewer treat the
    /// whole file as binary, so searches skip it (write the escape `'\0'` instead).
    #[test]
    fn the_source_has_no_raw_nul_byte() {
        let source = include_bytes!("workspace.rs");
        let at = source.iter().position(|&b| b == 0u8);
        assert_eq!(
            at, None,
            "a raw NUL byte in workspace.rs at byte offset {at:?}"
        );
    }

    /// One test's world: the user's repository (`repo`, on `main`), the worktree root next to
    /// it (`wt`), a real store, and the daemon's shared state.
    struct World {
        dir: tempfile::TempDir,
        repo: PathBuf,
        root: PathBuf,
        gitconfig: PathBuf,
        config: Config,
        shared: Shared,
        _store: Store,
        item: ItemId,
    }

    impl World {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            for sub in ["claude", "db", "gh", "repo", "work"] {
                std::fs::create_dir_all(d.join(sub)).unwrap();
            }
            let gitconfig = d.join("gitconfig");
            std::fs::write(
                &gitconfig,
                "[user]\n\tname = Synthetic Agent\n\temail = agent@example.com\n",
            )
            .unwrap();
            let repo = d.join("repo");
            let root = d.join("wt");
            git(&gitconfig, &repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("README.md"), "demo\n").unwrap();
            git(&gitconfig, &repo, &["add", "README.md"]);
            git(&gitconfig, &repo, &["commit", "-q", "-m", "Start"]);

            let board = d.join("board.toml");
            std::fs::write(&board, "").unwrap();
            let config_path = d.join("config.toml");
            std::fs::write(&config_path, config_text(d, &gitconfig, &board, &root)).unwrap();
            let config = bosun_config::load(&config_path, &SystemProbe::new()).unwrap();

            let store = Store::open(d.join("db").join("bosun.db"), StoreOptions::default())
                .await
                .unwrap();
            let item = ItemId::new("demo#1");
            store
                .writer()
                .write(vec![Write::Item(NewItem {
                    item: item.clone(),
                    name: "demo".to_owned(),
                    repo: "demo".to_owned(),
                    card_key: Some("1".to_owned()),
                    at_ms: 1,
                })])
                .await
                .unwrap();
            let options = Options {
                repo: "demo".to_owned(),
                daemon_id: "test-host/test-user".to_owned(),
                agent: None,
                work_dir: d.join("work"),
                poll: Duration::from_millis(50),
                idle_backoff: false,
                recheck: Duration::from_hours(1),
                question_grace: Duration::from_secs(1),
                pr_poll: Duration::from_secs(1),
                mcp: d.join("bosun-mcp"),
                shim: d.join("bosun-shim"),
                daemon_dir: d.join("state"),
                endpoint: bosun_client::Endpoint::new("bosun-test-workspace-unused"),
                handshake: Duration::from_secs(1),
                boot: None,
                failpoint: None,
            };
            let deps = Deps {
                reader: store.reader().clone(),
                writer: store.writer().clone(),
                events: Events::new("test", EventOptions::default()),
                queue: Arc::new(LocalFile::new(&board)),
                visibility: Arc::new(board::Visibility::fixed(false)),
                spawner: Arc::new(bosun_proc::NativeSpawner),
                sessions: Arc::new(super::super::ThroughShim),
                power: Arc::new(bosun_proc::FakePower::default()),
                logins: Arc::new(GhLogins),
                forge: Arc::new(super::super::GhForge),
                credentials: crate::agent::Credentials::default(),
                clock: Arc::new(SystemClock::new()),
                alive: Arc::new(|_| true),
                control: crate::control::Control::with_token("bosun-test-workspace", None),
                leases: Arc::new(
                    crate::leases::Leases::new(
                        &config,
                        &crate::leases::journal_path(&d.join("state").join("bosun.token")),
                    )
                    .unwrap(),
                ),
                live: Arc::default(),
                blockers: Arc::default(),
                free_space: Arc::new(Disk),
            };
            Self {
                repo,
                root,
                gitconfig,
                config,
                shared: Shared {
                    options,
                    deps,
                    worktrees: Registry::default(),
                },
                _store: store,
                item,
                dir,
            }
        }

        fn repo_config(&self) -> Repo {
            self.config.repos["demo"].clone()
        }

        fn identity(&self) -> &Identity {
            &self.config.identities["test"]
        }

        /// The workspace of the repository with its worktree configuration edited by `edit`.
        fn workspace(&self, edit: impl FnOnce(&mut bosun_config::Worktree)) -> Workspace {
            let mut repo = self.repo_config();
            edit(repo.worktree.as_mut().unwrap());
            Workspace::of(&repo).unwrap()
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            git(&self.gitconfig, dir, args)
        }

        async fn rows(&self) -> Vec<bosun_store::WorktreeRow> {
            self.shared.deps.reader.worktrees(&self.item).await.unwrap()
        }
    }

    /// The configuration of the world in `d`: one identity, and the repository `demo` at
    /// `d/repo` with its worktrees under `root`.
    fn config_text(d: &Path, gitconfig: &Path, board: &Path, root: &Path) -> String {
        format!(
            "[identity.test]\n\
             claude_config_dir = '{claude}'\n\
             database = '{db}'\n\
             github_login = 'synthetic-login'\n\
             gh_config_dir = '{gh}'\n\
             git_config = '{gitconfig}'\n\n\
             [repo.demo]\n\
             path = '{repo}'\n\
             identity = 'test'\n\
             queue = {{ kind = 'local_file', file = '{board}' }}\n\
             model = 'opus'\n\
             verdict = {{ timeout = '60s' }}\n\
             worktree = {{ root = '{root}' }}\n",
            claude = d.join("claude").display(),
            db = d.join("db").join("bosun.db").display(),
            gh = d.join("gh").display(),
            gitconfig = gitconfig.display(),
            repo = d.join("repo").display(),
            board = board.display(),
            root = root.display(),
        )
    }

    /// Runs `git args` in `dir` through the invoker, without the session's variables and with
    /// only `gitconfig` as the user's configuration. Returns the trimmed output.
    fn git(gitconfig: &Path, dir: &Path, args: &[&str]) -> String {
        let out = without_session(&mut Git::new(dir).command())
            .args(["-c", "commit.gpgSign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A kill receiver that is never raised.
    fn kill() -> tokio::sync::watch::Receiver<super::super::Signal> {
        tokio::sync::watch::channel(super::super::Signal::Run).1
    }

    fn canonical(path: impl AsRef<Path>) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_clone_has_long_paths_on_windows() {
        let w = World::new().await;
        let ws = w.workspace(|_| {});
        let local = |w: &World| {
            w.git(
                &ws.clone_path(),
                &["config", "--local", "--get-all", "core.longpaths"],
            )
        };
        w.git(&w.repo, &["config", "core.autocrlf", "input"]);
        ws.ensure_clone().unwrap();
        assert_eq!(local(&w), "true");
        // A clone made before the setting was: it gets it at the next check.
        w.git(&ws.clone_path(), &["config", "--unset", "core.longpaths"]);
        ws.ensure_clone().unwrap();
        assert_eq!(local(&w), "true");
        // Nothing else of the user's repository-local configuration comes along: the clone's
        // `core.autocrlf` is its own pin, not the user's `input`.
        let autocrlf = [
            "config",
            "--local",
            "--get",
            "--default",
            "",
            "core.autocrlf",
        ];
        assert_eq!(w.git(&ws.clone_path(), &autocrlf), "false");
    }

    /// The clone pins the line-ending policy in its own config, whatever the user's
    /// repository says (here `core.autocrlf=true`, which a bare clone does not copy anyway),
    /// and a clone made before the pin gets it at the next check
    /// (`mutant_loop_clone_without_eol_pin` leaves the clone unpinned).
    #[tokio::test]
    async fn the_clone_pins_the_line_ending_policy() {
        let w = World::new().await;
        let ws = w.workspace(|_| {});
        let pinned = |w: &World| {
            EOL_PIN.map(|(key, _)| {
                w.git(
                    &ws.clone_path(),
                    &["config", "--local", "--get", "--default", "", key],
                )
            })
        };
        w.git(&w.repo, &["config", "core.autocrlf", "true"]);
        ws.ensure_clone().unwrap();
        assert_eq!(pinned(&w), ["false", "native"]);
        // A clone made before the pin: it gets it at the next check.
        w.git(&ws.clone_path(), &["config", "--unset", "core.autocrlf"]);
        w.git(&ws.clone_path(), &["config", "core.eol", "crlf"]);
        ws.ensure_clone().unwrap();
        assert_eq!(pinned(&w), ["false", "native"]);
    }

    #[tokio::test]
    async fn an_item_worktree_hangs_off_the_daemons_clone_not_the_users_checkout() {
        let w = World::new().await;
        let ws = w.workspace(|_| {});
        let user_head = w.git(&w.repo, &["rev-parse", "HEAD"]);
        let refs = |w: &World| w.git(&w.repo, &["for-each-ref"]);
        let status = |w: &World| w.git(&w.repo, &["status", "--porcelain"]);
        let (refs_before, status_before) = (refs(&w), status(&w));

        ws.ensure_clone().unwrap();
        let clone = ws.clone_path();
        assert_eq!(
            w.git(&clone, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        let at = ws.sync_target("main").unwrap();
        assert_eq!(at, user_head);
        let made = ws
            .open(&w.shared, &w.item, "demo-1", &at, WorktreeKind::Item, None)
            .await
            .unwrap();

        let common = w.git(
            &made.path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        assert_eq!(canonical(common), canonical(&clone));
        assert!(made.branch.starts_with("bosun/"), "{}", made.branch);
        assert_eq!(made.base, user_head);
        assert_eq!(w.git(&made.path, &["rev-parse", "HEAD"]), user_head);
        let rows = w.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, WorktreeState::Created);
        assert_eq!(rows[0].kind, WorktreeKind::Item);
        assert_eq!(rows[0].base_sha, user_head);

        // The user's checkout is as it was: one worktree (its own), no item branch.
        let listed = w.git(&w.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            listed
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "{listed}"
        );
        assert_eq!(w.git(&w.repo, &["branch", "--list", "bosun/*"]), "");
        assert_eq!(refs(&w), refs_before);
        assert_eq!(status(&w), status_before);

        // A new commit of the user's reaches the clone's target at the next sync.
        std::fs::write(w.repo.join("NEXT.md"), "next\n").unwrap();
        w.git(&w.repo, &["add", "NEXT.md"]);
        w.git(&w.repo, &["commit", "-q", "-m", "Next"]);
        let next = w.git(&w.repo, &["rev-parse", "HEAD"]);
        assert_eq!(ws.sync_target("main").unwrap(), next);
        assert_eq!(w.git(&clone, &["rev-parse", "refs/heads/main"]), next);

        // The clone is reused, its worktree with it.
        ws.ensure_clone().unwrap();
        assert!(made.path.join("README.md").is_file());
        assert_eq!(
            w.git(&clone, &["worktree", "list", "--porcelain"])
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            2
        );

        // Anything else at the clone's path is an error, and stays.
        let other = w.dir.path().join("other-root");
        let stray = other.join(CLONE);
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("mine.txt"), "mine").unwrap();
        let elsewhere = w.workspace(|c| c.root.clone_from(&other));
        let error = elsewhere.ensure_clone().unwrap_err();
        assert!(matches!(error, Error::NotTheClone { .. }), "{error:?}");
        assert!(stray.join("mine.txt").is_file());

        assert_eq!(
            ws.close(&w.shared, w.identity(), &made, None, None).await,
            Closed::Removed
        );
    }

    #[tokio::test]
    async fn a_failing_setup_is_reported_and_leaves_no_worktree() {
        let w = World::new().await;
        let failing = w.workspace(|c| c.setup = Some("echo no; exit 3".to_owned()));
        failing.ensure_clone().unwrap();
        let at = failing.sync_target("main").unwrap();

        let result = failing
            .open_with_setup(
                &w.shared,
                w.identity(),
                &w.item,
                "demo-1",
                &at,
                WorktreeKind::Item,
                None,
                &mut kill(),
            )
            .await;
        let Err(Unopened::Setup(failed)) = result else {
            panic!("a failing setup was {result:?}");
        };
        assert!(failed.tail.contains("no"), "{failed:?}");
        assert!(failed.reason.contains('3'), "{failed:?}");
        let rows = w.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, WorktreeState::Removed);
        assert!(!Path::new(&rows[0].path).exists());
        assert_eq!(
            w.git(&failing.clone_path(), &["branch", "--list", "bosun/*"]),
            ""
        );

        let passing = w.workspace(|c| c.setup = Some("echo ok > made-by-setup".to_owned()));
        let made = passing
            .open_with_setup(
                &w.shared,
                w.identity(),
                &w.item,
                "demo-1",
                &at,
                WorktreeKind::Item,
                None,
                &mut kill(),
            )
            .await
            .unwrap();
        assert!(made.path.join("made-by-setup").is_file());
        assert_eq!(
            passing
                .close(&w.shared, w.identity(), &made, None, None)
                .await,
            Closed::Removed
        );
    }

    #[tokio::test]
    async fn a_closed_item_keeps_its_work_under_a_ref() {
        let w = World::new().await;
        let ws = w.workspace(|_| {});
        ws.ensure_clone().unwrap();
        let at = ws.sync_target("main").unwrap();
        let made = ws
            .open(&w.shared, &w.item, "demo-1", &at, WorktreeKind::Item, None)
            .await
            .unwrap();
        std::fs::write(made.path.join("committed.txt"), "kept\n").unwrap();
        w.git(&made.path, &["add", "committed.txt"]);
        w.git(&made.path, &["commit", "-q", "-m", "Work"]);
        std::fs::write(made.path.join("loose.txt"), "not committed\n").unwrap();

        let closed = ws
            .close(&w.shared, w.identity(), &made, Some(&w.item), None)
            .await;
        assert_eq!(closed, Closed::Removed);

        let clone = ws.clone_path();
        let aside = super::super::git::aside_ref(&w.item);
        let files = w.git(&clone, &["ls-tree", "-r", "--name-only", &aside]);
        let files: Vec<&str> = files.lines().collect();
        assert!(files.contains(&"committed.txt"), "{files:?}");
        assert!(files.contains(&"loose.txt"), "{files:?}");
        assert!(files.contains(&"README.md"), "{files:?}");
        assert!(!made.path.exists());
        assert_eq!(w.git(&clone, &["branch", "--list", "bosun/*"]), "");
        let rows = w.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, WorktreeState::Removed);
        assert!(rows[0].bytes.is_some_and(|b| b > 0), "{:?}", rows[0]);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_quarantined_worktree_is_recorded_and_the_next_one_gets_a_new_path() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let w = World::new().await;
        let ws = w.workspace(|_| {});
        ws.ensure_clone().unwrap();
        let at = ws.sync_target("main").unwrap();
        let made = ws
            .open(&w.shared, &w.item, "demo-1", &at, WorktreeKind::Item, None)
            .await
            .unwrap();
        let held = made.path.join("README.md");
        // Held as an editor holds it: no sharing at all, deletion included.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .unwrap();

        let closed = ws.close(&w.shared, w.identity(), &made, None, None).await;
        let Closed::Quarantined(report) = closed else {
            panic!("a worktree with a file held open was closed as {closed:?}");
        };
        assert_eq!(report.blocking.as_deref(), Some(held.as_path()));
        let rows = w.rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, WorktreeState::Quarantined);
        assert_eq!(
            rows[0].blocking.as_deref(),
            Some(held.to_string_lossy().as_ref())
        );
        assert!(rows[0].error.is_some());

        let again = ws
            .open(&w.shared, &w.item, "demo-1", &at, WorktreeKind::Item, None)
            .await
            .unwrap();
        assert_ne!(again.path, made.path);
        assert_eq!(w.rows().await.len(), 2);

        drop(handle);
        assert_eq!(
            ws.close(&w.shared, w.identity(), &again, None, None).await,
            Closed::Removed
        );
    }

    #[test]
    fn a_worktree_entered_after_another_build_has_its_tracked_files_touched() {
        let dir = tempfile::tempdir().unwrap();
        let gitconfig = dir.path().join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Synthetic Agent\n\temail = agent@example.com\n",
        )
        .unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&gitconfig, &repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        for name in ["one.txt", "src/two.txt", "three.txt"] {
            std::fs::write(repo.join(name), "tracked\n").unwrap();
        }
        git(&gitconfig, &repo, &["add", "-A"]);
        git(&gitconfig, &repo, &["commit", "-q", "-m", "Start"]);
        std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(repo.join("ignored.txt"), "ignored\n").unwrap();
        std::fs::remove_file(repo.join("three.txt")).unwrap();

        // Another worktree's build is newer than every file here.
        let old = std::time::SystemTime::now() - Duration::from_hours(1);
        let (tracked, untouched) = (
            ["one.txt", "src/two.txt"].map(|n| repo.join(n)),
            ["untracked.txt", "ignored.txt"].map(|n| repo.join(n)),
        );
        for path in tracked.iter().chain(&untouched) {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();

        let started = std::time::SystemTime::now();
        enter(&repo).unwrap();
        for path in &tracked {
            assert!(
                modified(path) >= started,
                "{} was not touched",
                path.display()
            );
        }
        for path in &untouched {
            let at = modified(path);
            assert!(
                at < started - Duration::from_mins(30),
                "{} was touched: {at:?}",
                path.display()
            );
        }
        assert!(!repo.join("three.txt").exists());
    }

    #[tokio::test]
    async fn no_worktree_is_opened_below_the_disk_floor() {
        let w = World::new().await;
        let ws = w.workspace(|c| c.disk_floor = Some(u64::MAX));
        ws.ensure_clone().unwrap();
        let at = ws.sync_target("main").unwrap();

        let error = ws
            .open(&w.shared, &w.item, "demo-1", &at, WorktreeKind::Item, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Worktree(worktree::Error::BelowFloor { .. })),
            "{error:?}"
        );
        let entries: Vec<_> = std::fs::read_dir(&w.root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from(CLONE)]);
        assert_eq!(
            w.git(&ws.clone_path(), &["branch", "--list", "bosun/*"]),
            ""
        );
        assert_eq!(w.rows().await, []);
    }
}
