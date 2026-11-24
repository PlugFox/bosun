//! Landing (#8, section Landing): rebasing an item's branch onto its target and moving the
//! target to the result, without ever moving a checkout that someone is working in.
//!
//! # Rebase
//!
//! [`rebase_onto`] runs `git rebase <target>` in a worktree. A conflict is not left for an
//! agent to resolve: the rebase is aborted, the conflicting files are reported, and the
//! worktree is back at its previous head and clean.
//!
//! # Fast-forward
//!
//! [`fast_forward`] moves a branch to a commit that descends from its head, and only in one of
//! three ways:
//!
//! - the branch is checked out nowhere: `update-ref` with the head it was read at, so a branch
//!   that moved in the meantime is not overwritten;
//! - it is checked out in a clean worktree: `merge --ff-only`, so the ref, the index and the
//!   files move together;
//! - it is checked out with local changes, or in the middle of an operation (a rebase, merge,
//!   cherry-pick or revert): nothing moves and [`FastForward::Deferred`] says why.
//!
//! All git runs through [`Git::command`], hooks disabled.

use std::fmt;
use std::path::PathBuf;

use crate::{Git, Reason, clean_tree};

/// The git state files of an operation in progress, by the name `git rev-parse --git-path`
/// knows them under; the first one that exists is reported.
const OPERATIONS: [&str; 5] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
];

/// The result of [`rebase_onto`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rebase {
    /// The branch is rebased; `head` is its new head.
    Rebased {
        /// The commit the branch now points at.
        head: String,
    },
    /// The rebase stopped on conflicts in `files` and was aborted: the worktree is clean and
    /// at the head it had before.
    Conflict {
        /// The conflicting paths, relative to the worktree.
        files: Vec<String>,
    },
}

/// The result of [`fast_forward`] when it does not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FastForward {
    /// The branch points at the target now.
    Moved,
    /// Nothing moved: the checkout of the branch has local changes or an operation in progress.
    Deferred {
        /// Why the checkout was left alone.
        reason: String,
    },
}

/// What can go wrong landing a branch.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A git command failed.
    Git(crate::Error),
    /// A revision does not name a commit.
    Unknown {
        /// The revision as given.
        rev: String,
    },
    /// `to` does not descend from the branch's head, so a fast-forward cannot take the branch
    /// there.
    NotDescendant {
        /// The branch, without `refs/heads/`.
        branch: String,
        /// The commit the branch points at.
        head: String,
        /// The commit it was to move to.
        to: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => write!(f, "{error}"),
            Self::Unknown { rev } => write!(f, "{rev} does not name a commit"),
            Self::NotDescendant { branch, head, to } => write!(
                f,
                "{to} does not descend from {branch} at {head}; the branch is not moved"
            ),
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

/// Rebases the branch checked out in `git` onto `target`.
///
/// On a conflict the rebase is aborted before this returns, so the worktree is never left in
/// the middle of a rebase.
///
/// # Errors
///
/// `target` names no commit, the worktree cannot be rebased (it has uncommitted changes, for
/// example), or a git command fails for another reason; the rebase is aborted in that case
/// too.
pub fn rebase_onto(git: &Git, target: &str) -> Result<Rebase, Error> {
    let target = resolve(git, target)?;
    let Err(error) = git.run(&["rebase", "--quiet", target.as_str()], None) else {
        let head = git.run(&["rev-parse", "--verify", "HEAD"], None)?;
        return Ok(Rebase::Rebased {
            head: head.trim().to_owned(),
        });
    };
    let files = unmerged(git)?;
    if files.is_empty() {
        // Not a conflict: git refused to start (a dirty worktree) or stopped while replaying.
        if rebasing(git)? {
            git.run(&["rebase", "--abort"], None)?;
        }
        return Err(error.into());
    }
    // Mutant (`mutant_git_rebase_keep_conflict`): the conflicted rebase is left in progress,
    // so the worktree is neither clean nor at its previous head.
    if !cfg!(mutant_git_rebase_keep_conflict) {
        git.run(&["rebase", "--abort"], None)?;
    }
    Ok(Rebase::Conflict { files })
}

/// Moves `branch` to `to`, which must descend from the branch's head.
///
/// See the module documentation for the three cases. A branch that is checked out nowhere is
/// moved with `update-ref` against the head read here, so it fails rather than overwrite a
/// branch that moved meanwhile.
///
/// # Errors
///
/// `branch` or `to` names no commit, `to` does not descend from the branch's head (nothing
/// moves), or a git command fails.
pub fn fast_forward(git: &Git, branch: &str, to: &str) -> Result<FastForward, Error> {
    let refname = format!("refs/heads/{branch}");
    let head = resolve(git, &refname)?;
    let to = resolve(git, to)?;
    if !git.test(&["merge-base", "--is-ancestor", head.as_str(), to.as_str()])? {
        return Err(Error::NotDescendant {
            branch: branch.to_owned(),
            head,
            to,
        });
    }
    let Some(path) = checkout_of(git, &refname)? else {
        git.run(
            &["update-ref", refname.as_str(), to.as_str(), head.as_str()],
            None,
        )?;
        return Ok(FastForward::Moved);
    };
    let checkout = Git::new(path);
    if let Some(operation) = operation(&checkout)? {
        return Ok(FastForward::Deferred {
            reason: format!(
                "{operation} is in progress in {}",
                checkout.repo().display()
            ),
        });
    }
    // Mutant (`mutant_git_ff_dirty_checkout`): the dirty check is skipped, so a checkout with
    // local changes is merged into.
    if !cfg!(mutant_git_ff_dirty_checkout) {
        let dirty = clean_tree(&checkout)?;
        if !dirty.is_empty() {
            return Ok(FastForward::Deferred {
                reason: describe(&dirty),
            });
        }
    }
    checkout.run(&["merge", "--ff-only", "--quiet", to.as_str()], None)?;
    Ok(FastForward::Moved)
}

/// The full sha of `rev`, which must name a commit. A revision that looks like an option is
/// refused, as git would read it as one.
fn resolve(git: &Git, rev: &str) -> Result<String, Error> {
    let unknown = || Error::Unknown {
        rev: rev.to_owned(),
    };
    if rev.starts_with('-') {
        return Err(unknown());
    }
    let revision = format!("{rev}^{{commit}}");
    git.optional(&["rev-parse", "--verify", "--quiet", revision.as_str()])?
        .ok_or_else(unknown)
}

/// The paths with unresolved conflicts in `git`'s worktree.
fn unmerged(git: &Git) -> Result<Vec<String>, Error> {
    let listing = git.run(&["diff", "--name-only", "--diff-filter=U", "-z"], None)?;
    Ok(listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Whether a rebase is in progress in `git`'s worktree.
fn rebasing(git: &Git) -> Result<bool, Error> {
    Ok(exists(git, "rebase-merge")? || exists(git, "rebase-apply")?)
}

/// The operation in progress in `git`'s worktree, if one is.
fn operation(git: &Git) -> Result<Option<&'static str>, Error> {
    for name in OPERATIONS {
        if exists(git, name)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Whether the git state file `name` of `git`'s worktree exists.
fn exists(git: &Git, name: &str) -> Result<bool, Error> {
    let Some(path) = git.optional(&["rev-parse", "--git-path", name])? else {
        return Ok(false);
    };
    Ok(git.repo().join(path).exists())
}

/// The worktree that has `refname` checked out, if any.
fn checkout_of(git: &Git, refname: &str) -> Result<Option<PathBuf>, Error> {
    let listing = git.run(&["worktree", "list", "--porcelain"], None)?;
    let mut path = None;
    for line in listing.lines() {
        if let Some(worktree) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(worktree));
        } else if line.strip_prefix("branch ") == Some(refname) {
            return Ok(path);
        }
    }
    Ok(None)
}

/// The preconditions that failed, as one line.
fn describe(reasons: &[Reason]) -> String {
    reasons
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A repository on `main` with one commit. Its own configuration carries the identity and
    /// turns signing off, so every commit and rebase the tests make needs nothing else.
    fn repository() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "--quiet", "--initial-branch=main"], None)
            .unwrap();
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgSign", "false"),
        ] {
            git.run(&["config", key, value], None).unwrap();
        }
        write_commit(&git, "a.txt", "base\n", "Initial commit");
        dir
    }

    /// Writes `file` with `content`, commits everything with `message`, and returns the sha.
    fn write_commit(git: &Git, file: &str, content: &str, message: &str) -> String {
        fs::write(git.repo().join(file), content).unwrap();
        git.run(&["add", "--all"], None).unwrap();
        git.run(&["commit", "--quiet", "-m", message], None)
            .unwrap();
        head(git, "HEAD")
    }

    /// The sha `rev` names.
    fn head(git: &Git, rev: &str) -> String {
        git.run(&["rev-parse", "--verify", rev], None)
            .unwrap()
            .trim()
            .to_owned()
    }

    /// `git status --porcelain`, which is empty for a clean worktree.
    fn status(git: &Git) -> String {
        git.run(&["status", "--porcelain"], None).unwrap()
    }

    #[test]
    fn a_clean_rebase_moves_the_branch_onto_the_target() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "feature"], None)
            .unwrap();
        write_commit(&git, "b.txt", "feature\n", "Feature");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        let target = write_commit(&git, "c.txt", "main\n", "Main moves on");
        git.run(&["checkout", "--quiet", "feature"], None).unwrap();

        let Rebase::Rebased { head: new } = rebase_onto(&git, "main").unwrap() else {
            panic!("a rebase without conflicts did not succeed");
        };
        assert_eq!(new, head(&git, "HEAD"));
        assert_eq!(
            head(&git, "HEAD^"),
            target,
            "the feature commit sits on main"
        );
        assert_eq!(status(&git), "");
    }

    #[test]
    fn a_conflicting_rebase_is_aborted_and_reported() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "feature"], None)
            .unwrap();
        let before = write_commit(&git, "a.txt", "feature\n", "Feature edits a.txt");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        write_commit(&git, "a.txt", "main\n", "Main edits a.txt");
        git.run(&["checkout", "--quiet", "feature"], None).unwrap();

        let outcome = rebase_onto(&git, "main").unwrap();
        assert_eq!(
            outcome,
            Rebase::Conflict {
                files: vec!["a.txt".to_owned()]
            }
        );
        assert_eq!(status(&git), "", "the worktree is clean after the abort");
        assert_eq!(head(&git, "HEAD"), before, "the head is the one it had");
        assert!(!repo.path().join(".git").join("rebase-merge").exists());
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "feature\n"
        );
    }

    #[test]
    fn a_branch_checked_out_nowhere_moves_with_update_ref() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "feature"], None)
            .unwrap();
        let old = write_commit(&git, "b.txt", "feature\n", "Feature");
        git.run(&["checkout", "--quiet", "-b", "tip"], None)
            .unwrap();
        let new = write_commit(&git, "c.txt", "tip\n", "Tip");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();

        assert_eq!(
            fast_forward(&git, "feature", &new).unwrap(),
            FastForward::Moved
        );
        assert_eq!(head(&git, "refs/heads/feature"), new);
        assert_ne!(old, new);
        assert_eq!(status(&git), "", "the checkout of main is untouched");
    }

    #[test]
    fn a_clean_checkout_of_the_target_fast_forwards_with_merge() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "tip"], None)
            .unwrap();
        let new = write_commit(&git, "b.txt", "tip\n", "Tip");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();

        assert_eq!(
            fast_forward(&git, "main", &new).unwrap(),
            FastForward::Moved
        );
        assert_eq!(head(&git, "HEAD"), new, "the ref and HEAD move together");
        assert_eq!(status(&git), "", "the index and the files moved too");
        assert_eq!(
            fs::read_to_string(repo.path().join("b.txt")).unwrap(),
            "tip\n"
        );
    }

    #[test]
    fn a_dirty_checkout_of_the_target_defers_the_fast_forward() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "tip"], None)
            .unwrap();
        let new = write_commit(&git, "b.txt", "tip\n", "Tip");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        let edited = repo.path().join("a.txt");
        fs::write(&edited, "the user is editing\n").unwrap();
        let before_ref = head(&git, "refs/heads/main");
        let before_bytes = fs::read(&edited).unwrap();

        let outcome = fast_forward(&git, "main", &new).unwrap();
        assert!(
            matches!(outcome, FastForward::Deferred { .. }),
            "a dirty checkout was merged into: {outcome:?}"
        );
        assert_eq!(
            head(&git, "refs/heads/main"),
            before_ref,
            "the ref is unchanged"
        );
        assert_eq!(
            fs::read(&edited).unwrap(),
            before_bytes,
            "the edit is unchanged"
        );
        assert!(!repo.path().join("b.txt").exists());
    }

    #[test]
    fn a_checkout_with_an_operation_in_progress_defers_the_fast_forward() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "other"], None)
            .unwrap();
        write_commit(&git, "a.txt", "other\n", "Other edits a.txt");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        write_commit(&git, "a.txt", "main\n", "Main edits a.txt");
        git.run(&["checkout", "--quiet", "-b", "tip"], None)
            .unwrap();
        let new = write_commit(&git, "b.txt", "tip\n", "Tip");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        // The merge conflicts and stops: MERGE_HEAD is left in the checkout of main.
        assert!(git.run(&["merge", "--quiet", "other"], None).is_err());
        let before_ref = head(&git, "refs/heads/main");

        let outcome = fast_forward(&git, "main", &new).unwrap();
        assert!(
            matches!(&outcome, FastForward::Deferred { reason } if reason.contains("MERGE_HEAD")),
            "a merge in progress was fast-forwarded past: {outcome:?}"
        );
        assert_eq!(head(&git, "refs/heads/main"), before_ref);
    }

    #[test]
    fn a_target_that_does_not_descend_from_the_head_is_refused() {
        let repo = repository();
        let git = Git::new(repo.path());
        git.run(&["checkout", "--quiet", "-b", "side"], None)
            .unwrap();
        let sibling = write_commit(&git, "b.txt", "side\n", "Side");
        git.run(&["checkout", "--quiet", "main"], None).unwrap();
        let before = write_commit(&git, "c.txt", "main\n", "Main moves on");

        let err = fast_forward(&git, "main", &sibling).unwrap_err();
        assert!(
            matches!(err, Error::NotDescendant { .. }),
            "not refused as a non-descendant: {err}"
        );
        assert_eq!(
            head(&git, "refs/heads/main"),
            before,
            "the branch is not moved"
        );
        assert_eq!(status(&git), "");
    }

    #[test]
    fn an_unknown_revision_is_refused_by_name() {
        let repo = repository();
        let git = Git::new(repo.path());
        let err = fast_forward(&git, "main", "no-such-commit").unwrap_err();
        assert_eq!(err.to_string(), "no-such-commit does not name a commit");
        assert!(matches!(
            rebase_onto(&git, "--exec=x"),
            Err(Error::Unknown { .. })
        ));
    }
}
