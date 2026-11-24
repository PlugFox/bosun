//! Landing (#8 "Landing"): rebasing an item's branch onto its target and fast-forwarding the
//! target to it.
//!
//! - [`rebase_onto`] rebases a worktree's checkout onto a target. A conflict is not left for
//!   anyone to resolve: the rebase is aborted, the worktree is back, clean, at its previous
//!   head, and the answer names the conflicting files.
//! - [`fast_forward`] moves a branch to a descendant commit without ever touching a person's
//!   work:
//!   - a branch no worktree has checked out moves with `git update-ref`, given the head it was
//!     read at, so a branch that moved meanwhile is not overwritten;
//!   - a branch checked out in a clean worktree moves with `git merge --ff-only` inside it, so
//!     the ref, the index and the files move together;
//!   - a branch checked out with local changes (untracked files included), or in a worktree
//!     with a rebase, merge, cherry-pick, revert or bisect in progress, does not move: the
//!     answer is [`FastForward::Deferred`] and nothing is touched;
//!   - a commit that does not descend from the branch's head is refused.
//!
//! Every git command goes through [`Git::command`], so no repository hook runs.

use std::fmt;
use std::path::PathBuf;

use crate::{Git, Reason};

/// What [`rebase_onto`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebase {
    /// The checkout is rebased; `head` is its new head.
    Rebased {
        /// The new head.
        head: String,
    },
    /// The rebase conflicted and was aborted; the worktree is clean at its previous head.
    Conflict {
        /// The conflicting files, relative to the worktree.
        files: Vec<String>,
    },
}

/// What [`fast_forward`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastForward {
    /// The branch is at `to` (it already was when `from` equals `to`).
    Moved {
        /// The head it was at.
        from: String,
        /// The head it is at.
        to: String,
    },
    /// Nothing moved: the branch's checkout is in use.
    Deferred {
        /// Why.
        reason: Deferred,
    },
}

/// Why a fast-forward was deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deferred {
    /// The worktree that has the branch checked out has local changes.
    LocalChanges {
        /// The worktree.
        worktree: PathBuf,
        /// The changed paths, as `git status --porcelain` names them.
        files: Vec<String>,
    },
    /// The worktree that has the branch checked out is in the middle of an operation.
    InProgress {
        /// The worktree.
        worktree: PathBuf,
        /// The operation.
        operation: Operation,
    },
}

impl fmt::Display for Deferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalChanges { worktree, files } => write!(
                f,
                "{} has local changes ({})",
                worktree.display(),
                files.join(", ")
            ),
            Self::InProgress {
                worktree,
                operation,
            } => write!(f, "{} has a {operation} in progress", worktree.display()),
        }
    }
}

/// An operation a worktree can be in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// `git rebase` (either backend).
    Rebase,
    /// `git merge`.
    Merge,
    /// `git cherry-pick`.
    CherryPick,
    /// `git revert`.
    Revert,
    /// `git bisect`.
    Bisect,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
        })
    }
}

/// Why a rebase or a fast-forward failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The commit does not descend from the branch's head; nothing moved.
    NotDescendant {
        /// The branch.
        branch: String,
        /// Its head.
        head: String,
        /// The commit it was to move to.
        to: String,
    },
    /// The rebase stopped with no conflicting file; it was aborted.
    Stopped {
        /// What git said.
        stderr: String,
    },
    /// A git command failed.
    Git(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDescendant { branch, head, to } => write!(
                f,
                "{to} does not descend from {branch} at {head}: not a fast-forward, nothing moved"
            ),
            Self::Stopped { stderr } => {
                write!(f, "the rebase stopped and was aborted: {}", stderr.trim())
            }
            Self::Git(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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

/// Rebases the checkout of `worktree` onto `target` (a ref or a commit).
///
/// On a conflict the rebase is aborted, so the worktree is clean at its previous head, and the
/// conflicting files are returned.
///
/// # Errors
///
/// Git refuses to start the rebase (local changes, an unknown target), the rebase stops for a
/// reason other than a conflict (it is aborted then), or the abort fails.
pub fn rebase_onto(worktree: &Git, target: &str) -> Result<Rebase, Error> {
    let out = worktree
        .command()
        .args([
            "rebase",
            "--quiet",
            "--no-autostash",
            "--end-of-options",
            target,
        ])
        .output()
        .map_err(|source| crate::Error::Spawn { source })?;
    if out.status.success() {
        let head = worktree
            .run(&["rev-parse", "HEAD"], None)?
            .trim()
            .to_owned();
        return Ok(Rebase::Rebased { head });
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let state = paths(worktree, &["rebase-merge", "rebase-apply"])?;
    if !state.iter().any(|p| p.exists()) {
        // It never started: git said why.
        return Err(crate::Error::Git {
            repo: worktree.repo().to_owned(),
            command: "rebase".to_owned(),
            stderr,
        }
        .into());
    }
    let files: Vec<String> = worktree
        .run(&["diff", "--name-only", "--diff-filter=U", "-z"], None)?
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    // Mutant (`mutant_git_rebase_keep_conflict`): the conflict is left in the worktree.
    if !cfg!(mutant_git_rebase_keep_conflict) {
        worktree.run(&["rebase", "--abort"], None)?;
    }
    if files.is_empty() {
        return Err(Error::Stopped { stderr });
    }
    Ok(Rebase::Conflict { files })
}

/// Moves `branch` (a short name) of `repo` to `to` (a ref or a commit), which must descend
/// from its head; see the module documentation for how, and when it defers.
///
/// # Errors
///
/// `to` names no commit or does not descend from the branch's head, the branch does not
/// exist, it moved while this ran, or git fails.
pub fn fast_forward(repo: &Git, branch: &str, to: &str) -> Result<FastForward, Error> {
    let full = format!("refs/heads/{branch}");
    let to = repo
        .run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{to}^{{commit}}"),
            ],
            None,
        )?
        .trim()
        .to_owned();
    let head = repo
        .run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &full,
            ],
            None,
        )?
        .trim()
        .to_owned();
    if !repo.test(&["merge-base", "--is-ancestor", &head, &to])? {
        return Err(Error::NotDescendant {
            branch: branch.to_owned(),
            head,
            to,
        });
    }
    if head == to {
        return Ok(FastForward::Moved { from: head, to });
    }

    let Some(holder) = holder(repo, &full)? else {
        repo.run(
            &["update-ref", "-m", "bosun: fast-forward", &full, &to, &head],
            None,
        )?;
        return Ok(FastForward::Moved { from: head, to });
    };
    let checkout = Git::new(&holder.path);
    if let Some(operation) = holder.operation {
        return Ok(FastForward::Deferred {
            reason: Deferred::InProgress {
                worktree: holder.path,
                operation,
            },
        });
    }
    // The same test of a clean checkout as the verdict's (untracked files count).
    let files: Vec<String> = crate::clean_tree(&checkout)?
        .into_iter()
        .flat_map(|reason| match reason {
            Reason::Dirty { entries } => entries,
            _ => Vec::new(),
        })
        .map(|entry| entry.get(3..).unwrap_or(&entry).to_owned())
        .collect();
    // Mutant (`mutant_git_ff_dirty_checkout`): local changes are not looked at.
    if !files.is_empty() && !cfg!(mutant_git_ff_dirty_checkout) {
        return Ok(FastForward::Deferred {
            reason: Deferred::LocalChanges {
                worktree: holder.path,
                files,
            },
        });
    }
    checkout.run(
        &["merge", "--ff-only", "--quiet", "--end-of-options", &to],
        None,
    )?;
    Ok(FastForward::Moved { from: head, to })
}

/// A worktree that holds a branch: checked out, or detached by an operation on it.
struct Holder {
    path: PathBuf,
    /// The operation in progress there, if any.
    operation: Option<Operation>,
}

/// The worktree of `repo` that holds `full` (`refs/heads/<branch>`), with the operation in
/// progress there.
fn holder(repo: &Git, full: &str) -> Result<Option<Holder>, Error> {
    let list = repo.run(&["worktree", "list", "--porcelain"], None)?;
    for entry in list.split("\n\n") {
        let mut path = None;
        let mut checked_out = false;
        let mut skip = false;
        for line in entry.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line.strip_prefix("branch ") == Some(full) {
                checked_out = true;
            } else if line == "bare" || line == "prunable" || line.starts_with("prunable ") {
                // A bare repository has no checkout; a prunable entry's directory is gone (or
                // is no longer its worktree), so nothing there can be overwritten.
                skip = true;
            }
        }
        let Some(path) = path else { continue };
        // A stale entry git has not flagged yet: its directory is gone.
        if skip || !path.is_dir() {
            continue;
        }
        let operation = operation(&Git::new(&path), full)?;
        let on_it = operation.is_some_and(|(_, on_it)| on_it);
        if checked_out || on_it {
            return Ok(Some(Holder {
                path,
                operation: operation.map(|(op, _)| op),
            }));
        }
    }
    Ok(None)
}

/// The operation in progress in `worktree`, and whether it is on `full` (a rebase or a bisect
/// detaches the head from the branch it works on).
fn operation(worktree: &Git, full: &str) -> Result<Option<(Operation, bool)>, Error> {
    let [merge_dir, apply_dir, merge, pick, revert, bisect] = paths(
        worktree,
        &[
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_START",
        ],
    )?
    .try_into()
    .map_err(|_| worktree.unexpected("not one path per --git-path".to_owned()))?;
    for dir in [merge_dir, apply_dir] {
        if dir.exists() {
            // `head-name` holds the full name of the branch being rebased.
            let on_it = std::fs::read_to_string(dir.join("head-name"))
                .is_ok_and(|text| text.trim() == full);
            return Ok(Some((Operation::Rebase, on_it)));
        }
    }
    if bisect.exists() {
        // `BISECT_START` holds the short name of the branch bisect started from.
        let short = full.strip_prefix("refs/heads/").unwrap_or(full);
        let on_it = std::fs::read_to_string(&bisect).is_ok_and(|t| t.trim() == short);
        return Ok(Some((Operation::Bisect, on_it)));
    }
    for (file, op) in [
        (merge, Operation::Merge),
        (pick, Operation::CherryPick),
        (revert, Operation::Revert),
    ] {
        if file.exists() {
            return Ok(Some((op, false)));
        }
    }
    Ok(None)
}

/// The absolute paths of `names` inside the git directory of `worktree`.
fn paths(worktree: &Git, names: &[&str]) -> Result<Vec<PathBuf>, Error> {
    let mut args = vec!["rev-parse", "--path-format=absolute"];
    for name in names {
        args.extend(["--git-path", name]);
    }
    let out = worktree.run(&args, None)?;
    let paths: Vec<PathBuf> = out.lines().map(PathBuf::from).collect();
    if paths.len() != names.len() {
        return Err(worktree
            .unexpected(format!("{} paths for {} names", paths.len(), names.len()))
            .into());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::NO_HOOKS;

    /// `git args` in `dir` through the invoker, with no user or system configuration and no
    /// signing.
    fn raw(dir: &Path, args: &[&str]) -> std::process::Output {
        Git::new(dir)
            .command()
            .args(["-c", "commit.gpgSign=false"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "user.email=test@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs")
    }

    /// Runs `git args` in `dir` (see [`raw`]); it must succeed. Returns the trimmed output.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = raw(dir, args);
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

    /// A temporary directory holding a repository (`repo`) on `main` with one commit, which
    /// `a.txt` holds. The code under test reads the user's and the system's configuration, so
    /// the repository names its own committer, turns signing off, and turns off the line-ending
    /// conversion Git for Windows enables system-wide (the helper commits without it, so a
    /// checkout with it would rewrite every file).
    fn setup() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "commit.gpgSign", "false"]);
        git(&repo, &["config", "core.autocrlf", "false"]);
        commit(&repo, "a.txt", "first\n");
        (dir, repo)
    }

    /// Adds a worktree next to the repository with `branch` checked out.
    fn worktree(dir: &TempDir, repo: &Path, branch: &str) -> PathBuf {
        let path = dir.path().join(format!("wt-{branch}"));
        git(
            repo,
            &[
                "worktree",
                "add",
                "--quiet",
                &path.to_string_lossy(),
                branch,
            ],
        );
        path
    }

    fn head(dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev])
    }

    fn status(dir: &Path) -> String {
        git(dir, &["status", "--porcelain", "--untracked-files=all"])
    }

    #[test]
    fn a_clean_rebase_moves_the_branch_onto_the_target() {
        let (_dir, repo) = setup();
        git(&repo, &["checkout", "--quiet", "-b", "topic"]);
        commit(&repo, "t.txt", "topic\n");
        git(&repo, &["checkout", "--quiet", "main"]);
        let target = commit(&repo, "m.txt", "main\n");
        git(&repo, &["checkout", "--quiet", "topic"]);

        let rebased = rebase_onto(&Git::new(&repo), "main").unwrap();
        let Rebase::Rebased { head: new } = rebased else {
            panic!("a clean rebase was reported {rebased:?}");
        };
        assert_eq!(new, head(&repo, "topic"));
        assert_eq!(head(&repo, "topic^"), target);
        assert!(repo.join("t.txt").is_file() && repo.join("m.txt").is_file());
        assert_eq!(status(&repo), "");
    }

    #[test]
    fn a_conflicting_rebase_is_aborted_and_reported() {
        let (_dir, repo) = setup();
        git(&repo, &["checkout", "--quiet", "-b", "topic"]);
        commit(&repo, "a.txt", "topic\n");
        commit(&repo, "t.txt", "topic only\n");
        git(&repo, &["checkout", "--quiet", "main"]);
        commit(&repo, "a.txt", "main\n");
        git(&repo, &["checkout", "--quiet", "topic"]);
        let before = head(&repo, "HEAD");
        let git_repo = Git::new(&repo);

        let rebased = rebase_onto(&git_repo, "main").unwrap();
        assert_eq!(
            rebased,
            Rebase::Conflict {
                files: vec!["a.txt".to_owned()]
            }
        );
        assert_eq!(head(&repo, "HEAD"), before);
        assert_eq!(git(&repo, &["symbolic-ref", "--short", "HEAD"]), "topic");
        assert_eq!(status(&repo), "");
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "topic\n"
        );
        let state = paths(&git_repo, &["rebase-merge", "rebase-apply"]).unwrap();
        assert!(state.iter().all(|p| !p.exists()), "{state:?}");
    }

    #[test]
    fn a_branch_no_worktree_has_checked_out_moves_by_its_ref() {
        let (_dir, repo) = setup();
        let from = head(&repo, "main");
        git(&repo, &["branch", "land"]);
        let to = commit(&repo, "b.txt", "second\n");

        let moved = fast_forward(&Git::new(&repo), "land", "main").unwrap();
        assert_eq!(
            moved,
            FastForward::Moved {
                from,
                to: to.clone()
            }
        );
        assert_eq!(head(&repo, "land"), to);
        assert_eq!(status(&repo), "");
    }

    #[test]
    fn a_clean_checkout_of_the_target_moves_with_its_files() {
        let (dir, repo) = setup();
        let from = head(&repo, "main");
        git(&repo, &["branch", "land"]);
        let to = commit(&repo, "b.txt", "second\n");
        let wt = worktree(&dir, &repo, "land");

        let moved = fast_forward(&Git::new(&repo), "land", &to).unwrap();
        assert_eq!(
            moved,
            FastForward::Moved {
                from,
                to: to.clone()
            }
        );
        assert_eq!(head(&wt, "HEAD"), to);
        assert_eq!(git(&wt, &["symbolic-ref", "--short", "HEAD"]), "land");
        assert_eq!(
            std::fs::read_to_string(wt.join("b.txt")).unwrap(),
            "second\n"
        );
        assert_eq!(status(&wt), "");
    }

    #[test]
    fn a_dirty_checkout_of_the_target_defers_the_fast_forward() {
        let (dir, repo) = setup();
        git(&repo, &["branch", "land"]);
        let to = commit(&repo, "b.txt", "second\n");
        let wt = worktree(&dir, &repo, "land");
        // The user's work: a changed tracked file and a new one, neither touched by `to`.
        std::fs::write(wt.join("a.txt"), b"mine\r\nnot committed").unwrap();
        std::fs::write(wt.join("notes.txt"), b"\x00scratch\xff").unwrap();
        let before = head(&repo, "land");
        let index = std::fs::read(git(
            &wt,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ))
        .unwrap();

        let deferred = fast_forward(&Git::new(&repo), "land", &to).unwrap();
        let FastForward::Deferred {
            reason: Deferred::LocalChanges { worktree, files },
        } = deferred
        else {
            panic!("a dirty checkout was answered {deferred:?}");
        };
        assert_eq!(
            std::fs::canonicalize(worktree).unwrap(),
            std::fs::canonicalize(&wt).unwrap()
        );
        assert_eq!(files, ["a.txt", "notes.txt"]);
        assert_eq!(head(&repo, "land"), before);
        assert_eq!(head(&wt, "HEAD"), before);
        assert_eq!(
            std::fs::read(wt.join("a.txt")).unwrap(),
            b"mine\r\nnot committed"
        );
        assert_eq!(
            std::fs::read(wt.join("notes.txt")).unwrap(),
            b"\x00scratch\xff"
        );
        assert!(!wt.join("b.txt").exists());
        let index_after = std::fs::read(git(
            &wt,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ))
        .unwrap();
        assert_eq!(index_after, index);
    }

    #[test]
    fn a_checkout_mid_rebase_defers_the_fast_forward() {
        let (dir, repo) = setup();
        git(&repo, &["branch", "land"]);
        commit(&repo, "a.txt", "main\n");
        let wt = worktree(&dir, &repo, "land");
        commit(&wt, "a.txt", "land\n");
        let before = head(&wt, "HEAD");
        // A descendant of the branch, made before the rebase starts.
        git(&repo, &["branch", "next", "land"]);
        let wt_next = worktree(&dir, &repo, "next");
        let to = commit(&wt_next, "c.txt", "next\n");
        // The user's rebase stops on a conflict and detaches the head from the branch.
        assert!(!raw(&wt, &["rebase", "main"]).status.success());
        let detached = git(&wt, &["rev-parse", "HEAD"]);

        let deferred = fast_forward(&Git::new(&repo), "land", &to).unwrap();
        let FastForward::Deferred {
            reason:
                Deferred::InProgress {
                    worktree,
                    operation,
                },
        } = deferred
        else {
            panic!("a checkout mid-rebase was answered {deferred:?}");
        };
        assert_eq!(operation, Operation::Rebase);
        assert_eq!(
            std::fs::canonicalize(worktree).unwrap(),
            std::fs::canonicalize(&wt).unwrap()
        );
        assert_eq!(head(&repo, "land"), before);
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), detached);
    }

    #[test]
    fn a_commit_that_does_not_descend_from_the_branch_is_refused() {
        let (_dir, repo) = setup();
        git(&repo, &["checkout", "--quiet", "-b", "land"]);
        let before = commit(&repo, "l.txt", "land\n");
        git(&repo, &["checkout", "--quiet", "main"]);
        let other = commit(&repo, "m.txt", "main\n");

        let error = fast_forward(&Git::new(&repo), "land", "main").unwrap_err();
        let Error::NotDescendant {
            branch,
            head: at,
            to,
        } = &error
        else {
            panic!("a non-descendant was answered {error:?}");
        };
        assert_eq!((branch.as_str(), at, to), ("land", &before, &other));
        assert!(error.to_string().contains("nothing moved"), "{error}");
        assert_eq!(head(&repo, "land"), before);
    }

    #[test]
    fn a_worktree_whose_directory_is_gone_does_not_hold_the_branch() {
        let (dir, repo) = setup();
        let from = head(&repo, "main");
        git(&repo, &["branch", "land"]);
        let to = commit(&repo, "b.txt", "second\n");
        let wt = worktree(&dir, &repo, "land");
        // Deleted behind git's back: its entry stays until a prune.
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(git(&repo, &["worktree", "list", "--porcelain"]).contains("prunable"));

        let moved = fast_forward(&Git::new(&repo), "land", &to).unwrap();
        assert_eq!(
            moved,
            FastForward::Moved {
                from,
                to: to.clone()
            }
        );
        assert_eq!(head(&repo, "land"), to);
    }

    #[test]
    fn a_target_that_looks_like_an_option_is_a_revision() {
        let (_dir, repo) = setup();
        git(&repo, &["checkout", "--quiet", "-b", "topic"]);
        commit(&repo, "t.txt", "topic\n");
        let before = head(&repo, "HEAD");

        // As an option, `--root` would rebase the whole branch and succeed.
        let error = rebase_onto(&Git::new(&repo), "--root").unwrap_err();
        assert!(matches!(error, Error::Git(_)), "{error:?}");
        assert_eq!(head(&repo, "HEAD"), before);
        assert_eq!(status(&repo), "");
    }
}
