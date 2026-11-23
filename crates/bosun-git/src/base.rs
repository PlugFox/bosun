//! The base an item is claimed at (#7 "Verdict"): what the preconditions compare against.

use std::collections::BTreeMap;

use crate::{Error, Git};

/// The repository's state at claim: HEAD, its branch, every ref and the stash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// The commit HEAD resolved to.
    pub head: String,
    /// The full name of the branch HEAD was on (`refs/heads/...`); `None` when it was detached.
    pub branch: Option<String>,
    /// Every ref (`for-each-ref`), full name to the object it named, the stash ref included.
    pub refs: BTreeMap<String, String>,
    /// The stash entries, newest first. `refs/stash` names only the newest one; a drop followed
    /// by a push can leave it where it was while the list changed.
    pub stash: Vec<String>,
}

impl Base {
    /// Captures the base of the repository of `git`.
    ///
    /// # Errors
    ///
    /// `git` cannot be run, the directory is not a repository, HEAD names no commit yet, or a
    /// command prints something unexpected.
    pub fn capture(git: &Git) -> Result<Self, Error> {
        Ok(Self {
            head: head(git)?,
            branch: branch(git)?,
            refs: refs(git)?,
            stash: stash(git)?,
        })
    }
}

/// The commit HEAD resolves to.
pub(crate) fn head(git: &Git) -> Result<String, Error> {
    Ok(git
        .run(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], None)?
        .trim()
        .to_owned())
}

/// The full name of HEAD's branch, `None` when detached.
pub(crate) fn branch(git: &Git) -> Result<Option<String>, Error> {
    git.optional(&["symbolic-ref", "--quiet", "HEAD"])
}

/// Every ref and the object it names.
pub(crate) fn refs(git: &Git) -> Result<BTreeMap<String, String>, Error> {
    let out = git.run(&["for-each-ref", "--format=%(refname) %(objectname)"], None)?;
    let mut refs = BTreeMap::new();
    for line in out.lines() {
        // A ref name holds no space, so the last one separates the object.
        let Some((name, object)) = line.rsplit_once(' ') else {
            return Err(git.unexpected(format!("a ref line {line:?}")));
        };
        refs.insert(name.to_owned(), object.to_owned());
    }
    Ok(refs)
}

/// The stash entries, newest first.
pub(crate) fn stash(git: &Git) -> Result<Vec<String>, Error> {
    Ok(git
        .run(&["stash", "list", "--format=%H"], None)?
        .lines()
        .map(str::to_owned)
        .collect())
}
