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
//! - The target is fetched from the user's checkout at claim ([`Workspace::sync_target`]),
//!   fast-forward only: a target the user rewrote is an error, not an overwrite. It is never
//!   checked out in the clone.
//! - An item's worktree ([`Workspace::open`]) is made by [`bosun_git::worktree::create`] from
//!   the clone, below no disk floor, and recorded ([`Write::Worktree`]). The configured
//!   `setup` command runs in it ([`Workspace::setup`]) the way a verdict command runs; a
//!   failure is the caller's Needs you, not a failed attempt, and leaves no worktree behind
//!   ([`Workspace::open_with_setup`]).
//! - Closing it ([`Workspace::close`]) keeps the item's work in the clone when asked (under
//!   [`super::git::aside_ref`]), runs `teardown`, measures it, and removes it with its branch;
//!   one that cannot be removed is quarantined, recorded with the file that blocked, and
//!   reported once: never left behind silently, and its path is never used again.
//!
//! The git work is blocking and synchronous; the functions that also write to the store or run
//! a command are async and move their git work to the blocking pool ([`super::blocking`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bosun_config::{Identity, Repo};
use bosun_core::{ItemId, PathStyle, canonical_path};
use bosun_git::Git;
use bosun_git::worktree::{self, Floor, FreeSpace, Quarantine, Removal, Worktree};
use bosun_queue::lint::verdict_shell;
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::{NewWorktree, WorktreeKind, WorktreeState, Write};

use super::{Shared, blocking, identity, now_ms};

/// The name of the daemon's clone under the worktree root.
pub(crate) const CLONE: &str = ".clone";

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

impl fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` {}", self.command, self.reason)?;
        if !self.tail.trim().is_empty() {
            write!(f, "; its output ends:\n{}", self.tail.trim_end())?;
        }
        Ok(())
    }
}

/// Why [`Workspace::open_with_setup`] made no worktree.
#[derive(Debug)]
pub(crate) enum Unopened {
    /// It was not made.
    Error(Error),
    /// It was made, its setup failed, and it was closed again.
    Setup(SetupFailed),
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

    /// Removes it with its branch, or quarantines it (reported by [`Workspace::close`]).
    pub(crate) async fn close(self, shared: &Shared, identity: &Identity) -> Closed {
        self.workspace
            .close(shared, identity, &self.worktree, None)
            .await
    }
}

/// The volume's free space, as the system reports it.
struct Disk;

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
                Git::new(root).run(&["clone", "--bare", "--quiet", "--", &user, CLONE], None)?;
                Ok(Git::new(path))
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

    /// The blocking part of [`Workspace::open`].
    fn create(&self, name: &str, at: &str) -> Result<Worktree, Error> {
        // Mutant (`mutant_loop_worktree_in_user_repo`): the worktree hangs off the user's
        // checkout.
        let source = if cfg!(mutant_loop_worktree_in_user_repo) {
            Git::new(&self.user)
        } else {
            self.clone_git()
        };
        let made = match self.config.disk_floor {
            Some(bytes) => worktree::create(
                &source,
                &self.config.root,
                name,
                at,
                Floor {
                    bytes,
                    space: &Disk,
                },
            ),
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
        let this = self.clone();
        let (name, at) = (name.to_owned(), at.to_owned());
        let made = blocking(move || this.create(&name, &at)).await?;
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
        Ok(made)
    }

    /// Runs the configured `setup` command, if any, in `worktree`.
    ///
    /// # Errors
    ///
    /// It exited non-zero, timed out or could not run.
    pub(crate) async fn setup(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
    ) -> Result<(), SetupFailed> {
        let Some(command) = &self.config.setup else {
            return Ok(());
        };
        let ran = self.run(shared, identity, &worktree.path, command).await;
        // Mutant (`mutant_loop_setup_failure_ignored`): a failing setup counts as done.
        if cfg!(mutant_loop_setup_failure_ignored) {
            return Ok(());
        }
        ran
    }

    /// [`Workspace::open`], then [`Workspace::setup`]; a worktree whose setup failed is closed
    /// again.
    ///
    /// # Errors
    ///
    /// It was not made, or its setup failed (and it is gone, or quarantined and reported).
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
    ) -> Result<Worktree, Unopened> {
        let made = self
            .open(shared, item, name, at, kind, user_base)
            .await
            .map_err(Unopened::Error)?;
        if let Err(failed) = self.setup(shared, identity, &made).await {
            self.close(shared, identity, &made, None).await;
            return Err(Unopened::Setup(failed));
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
    /// It was not made, or its setup failed (and it is gone, or quarantined and reported).
    pub(crate) async fn checkout(
        &self,
        shared: &Shared,
        identity: &Identity,
        item: &ItemId,
        name: &str,
        at: &str,
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
            )
            .await?
        };
        Ok(Checkout {
            workspace: self.clone(),
            worktree,
        })
    }

    /// Closes `worktree`: keeps `aside`'s work in the clone first when given, runs `teardown`
    /// (a failure is reported and the removal goes on), measures it, and removes it with its
    /// branch, or quarantines it. The row is ended either way.
    pub(crate) async fn close(
        &self,
        shared: &Shared,
        identity: &Identity,
        worktree: &Worktree,
        aside: Option<&ItemId>,
    ) -> Closed {
        if let Some(item) = aside {
            let (dir, owned) = (worktree.path.clone(), item.clone());
            if let Err(e) = blocking(move || super::git::keep_work(&dir, &owned)).await {
                let error = format!(
                    "the work of {item} in {} cannot be kept, so the worktree is not removed: {e}",
                    worktree.path.display()
                );
                shared.trouble(error.clone());
                return Closed::Kept(error);
            }
        }
        if let Some(command) = &self.config.teardown
            && let Err(failed) = self.run(shared, identity, &worktree.path, command).await
        {
            shared.trouble(format!(
                "the teardown of the worktree {} failed, and it is removed anyway: {failed}",
                worktree.path.display()
            ));
        }
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

    /// Runs `command` in `dir` as a verdict command runs: the verdict shell, the agent
    /// environment, the verdict timeout, a bounded output.
    async fn run(
        &self,
        shared: &Shared,
        identity: &Identity,
        dir: &Path,
        command: &str,
    ) -> Result<(), SetupFailed> {
        let failed = |reason: String, tail: String| SetupFailed {
            command: command.to_owned(),
            reason,
            tail,
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
        let spec = VerdictSpec {
            shell,
            command: command.to_owned(),
            env: env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect(),
            cwd: dir.to_owned(),
            timeout: self.timeout,
            limits: Limits::default(),
        };
        let ran = bosun_runner::verdict::run(shared.deps.spawner.as_ref(), &spec)
            .await
            .map_err(|e| failed(format!("cannot run: {e}"), String::new()))?;
        match ran.outcome {
            Outcome::Pass => Ok(()),
            Outcome::Fail => Err(failed(
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
                mcp: d.join("bosun-mcp"),
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
                power: Arc::new(bosun_proc::FakePower::default()),
                logins: Arc::new(GhLogins),
                credentials: crate::agent::Credentials::default(),
                clock: Arc::new(SystemClock::new()),
                alive: Arc::new(|_| true),
                control: crate::control::Control::with_token("bosun-test-workspace", None),
            };
            Self {
                repo,
                root,
                gitconfig,
                config,
                shared: Shared { options, deps },
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

    fn canonical(path: impl AsRef<Path>) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
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
            ws.close(&w.shared, w.identity(), &made, None).await,
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
            )
            .await
            .unwrap();
        assert!(made.path.join("made-by-setup").is_file());
        assert_eq!(
            passing.close(&w.shared, w.identity(), &made, None).await,
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
            .close(&w.shared, w.identity(), &made, Some(&w.item))
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

        let closed = ws.close(&w.shared, w.identity(), &made, None).await;
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
            ws.close(&w.shared, w.identity(), &again, None).await,
            Closed::Removed
        );
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
