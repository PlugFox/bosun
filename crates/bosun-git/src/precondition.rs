//! The verdict preconditions (#7 "Verdict"): each one fails the attempt with its reason before
//! the verdict command runs.

use std::collections::BTreeSet;
use std::fmt;

use crate::base::{self, Base};
use crate::message::{self, Expected};
use crate::{Error, Git};

/// Why a precondition failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// The working tree has uncommitted changes or untracked files that are not ignored: the
    /// `git status --porcelain` entries, one per path.
    Dirty {
        /// The status entries, such as `?? new.txt` or ` M src/lib.rs`.
        entries: Vec<String>,
    },
    /// No commit was made since the base.
    NoCommits,
    /// The base is not an ancestor of HEAD: history below it was rewritten.
    NotDescendant {
        /// The base commit.
        base: String,
        /// The commit HEAD resolves to now.
        head: String,
    },
    /// HEAD is not on the branch it was on at claim.
    BranchChanged {
        /// The branch at claim; `None` for a detached HEAD.
        expected: Option<String>,
        /// The branch now; `None` for a detached HEAD.
        found: Option<String>,
    },
    /// A ref other than HEAD's branch was created, moved or deleted.
    RefMoved {
        /// The ref's full name.
        name: String,
        /// The object it named at claim; `None` when it did not exist.
        before: Option<String>,
        /// The object it names now; `None` when it no longer exists.
        after: Option<String>,
    },
    /// The stash list differs from the one at claim.
    StashChanged {
        /// The entries at claim, newest first.
        before: Vec<String>,
        /// The entries now, newest first.
        after: Vec<String>,
    },
    /// A commit message carries a keyword that closes an issue when the commit lands.
    ClosingKeyword {
        /// The commit.
        sha: String,
        /// The matched text.
        text: String,
    },
    /// A commit message carries a directive that skips CI.
    SkipCi {
        /// The commit.
        sha: String,
        /// The matched text.
        text: String,
    },
    /// A commit message carries an `Item:`, `Run:`, `Session:` or `Co-authored-by:` trailer
    /// whose value is not one of the expected ones.
    ForgedTrailer {
        /// The commit.
        sha: String,
        /// The trailer's name as written.
        key: String,
        /// Its value.
        value: String,
    },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "(none)".to_owned());
        match self {
            Self::Dirty { entries } => {
                write!(f, "the working tree is not clean: {}", entries.join(", "))
            }
            Self::NoCommits => f.write_str("no commit was made since the base"),
            Self::NotDescendant { base, head } => {
                write!(f, "the base {base} is not an ancestor of HEAD {head}")
            }
            Self::BranchChanged { expected, found } => write!(
                f,
                "HEAD is on {}, not on {} as at claim",
                opt(found),
                opt(expected)
            ),
            Self::RefMoved {
                name,
                before,
                after,
            } => write!(f, "the ref {name} moved: {} -> {}", opt(before), opt(after)),
            Self::StashChanged { before, after } => write!(
                f,
                "the stash changed: {} entries at claim, {} now",
                before.len(),
                after.len()
            ),
            Self::ClosingKeyword { sha, text } => {
                write!(f, "commit {sha} carries a closing keyword: {text:?}")
            }
            Self::SkipCi { sha, text } => {
                write!(f, "commit {sha} carries a CI skip directive: {text:?}")
            }
            Self::ForgedTrailer { sha, key, value } => {
                write!(f, "commit {sha} carries a forged trailer: {key}: {value}")
            }
        }
    }
}

/// Every precondition, in order; empty when all hold.
///
/// # Errors
///
/// A git command fails or prints something unexpected.
pub fn check(git: &Git, base: &Base, expected: &Expected) -> Result<Vec<Reason>, Error> {
    let mut reasons = clean_tree(git)?;
    reasons.extend(ancestry(git, base)?);
    reasons.extend(refs(git, base)?);
    reasons.extend(messages(git, base, expected)?);
    Ok(reasons)
}

/// The working tree is clean: no uncommitted change and no untracked file that is not
/// ignored. Ignored files are #59's snapshot, not this check.
///
/// # Errors
///
/// A git command fails.
pub fn clean_tree(git: &Git) -> Result<Vec<Reason>, Error> {
    if cfg!(mutant_git_skip_clean_tree) {
        return Ok(Vec::new());
    }
    // `--untracked-files=all` overrides a `status.showUntrackedFiles=no` the repository's
    // configuration might carry; submodules are compared too. With `--no-renames` a rename is
    // its two paths, so every entry is one field.
    let out = git.run(
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ],
        None,
    )?;
    let entries: Vec<String> = out
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(if entries.is_empty() {
        Vec::new()
    } else {
        vec![Reason::Dirty { entries }]
    })
}

/// At least one commit was made since the base, and the base is an ancestor of HEAD.
///
/// # Errors
///
/// A git command fails, or the base commit no longer exists.
pub fn ancestry(git: &Git, base: &Base) -> Result<Vec<Reason>, Error> {
    if cfg!(mutant_git_skip_ancestry) {
        return Ok(Vec::new());
    }
    let head = base::head(git)?;
    if !git.test(&["merge-base", "--is-ancestor", &base.head, &head])? {
        return Ok(vec![Reason::NotDescendant {
            base: base.head.clone(),
            head,
        }]);
    }
    if head == base.head {
        return Ok(vec![Reason::NoCommits]);
    }
    Ok(Vec::new())
}

/// HEAD is on the branch it was on at claim, and no other ref moved: no branch, tag or other
/// ref was created, moved or deleted, and the stash list is the same.
///
/// # Errors
///
/// A git command fails or prints something unexpected.
pub fn refs(git: &Git, base: &Base) -> Result<Vec<Reason>, Error> {
    if cfg!(mutant_git_skip_ref_check) {
        return Ok(Vec::new());
    }
    let mut reasons = Vec::new();
    let branch = base::branch(git)?;
    if branch != base.branch {
        reasons.push(Reason::BranchChanged {
            expected: base.branch.clone(),
            found: branch,
        });
    }
    let now = base::refs(git)?;
    let names: BTreeSet<&String> = base.refs.keys().chain(now.keys()).collect();
    for name in names {
        // The claim branch is where the commits go.
        if base.branch.as_ref() == Some(name) {
            continue;
        }
        let before = base.refs.get(name);
        let after = now.get(name);
        if before != after {
            reasons.push(Reason::RefMoved {
                name: name.clone(),
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }
    let stash = base::stash(git)?;
    if stash != base.stash {
        reasons.push(Reason::StashChanged {
            before: base.stash.clone(),
            after: stash,
        });
    }
    Ok(reasons)
}

/// The messages of the commits since the base (`base..HEAD`) carry no closing keyword, no CI
/// skip directive and no forged trailer.
///
/// # Errors
///
/// A git command fails or prints something unexpected.
pub fn messages(git: &Git, base: &Base, expected: &Expected) -> Result<Vec<Reason>, Error> {
    if cfg!(mutant_git_skip_message_check) {
        return Ok(Vec::new());
    }
    let range = format!("{}..HEAD", base.head);
    let log = git.run(
        &[
            "log",
            "-z",
            "--no-color",
            "--no-show-signature",
            "--encoding=UTF-8",
            "--format=%H%n%B",
            &range,
            "--",
        ],
        None,
    )?;
    let mut reasons = Vec::new();
    for record in log.split('\0').filter(|r| !r.trim().is_empty()) {
        let record = record.trim_start_matches('\n');
        let Some((sha, body)) = record.split_once('\n') else {
            return Err(git.unexpected(format!("a log record {record:?}")));
        };
        reasons.extend(message::problems(sha, body, expected));
    }
    Ok(reasons)
}
