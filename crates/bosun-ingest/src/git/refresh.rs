//! Reading a configured repository's new commits into the store, as the daemon does (#26).

use std::fmt;
use std::path::Path;

use bosun_core::{canonical_path, style_of};
use bosun_store::{Reader, Repo, Write, Writer};

use super::{Cursor, scan};

/// Why [`refresh`] stored nothing.
#[derive(Debug)]
#[non_exhaustive]
pub enum RefreshError {
    /// The repository could not be read.
    Git(super::Error),
    /// The store refused the commits, or is closed.
    Store(bosun_store::Error),
    /// The blocking read panicked or was cancelled.
    Task(String),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(e) => e.fmt(f),
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Task(e) => write!(f, "commit reader task: {e}"),
        }
    }
}

impl std::error::Error for RefreshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::Task(_) => None,
        }
    }
}

impl From<bosun_store::Error> for RefreshError {
    fn from(e: bosun_store::Error) -> Self {
        Self::Store(e)
    }
}

/// Reads the commits of the repository at `root` that the store does not hold yet (asking
/// `reader`) and stores them through `writer`, with the repository's origin URL and default
/// branch, in one batch. Returns how many commits the scan read.
///
/// The cursor is every commit already stored for the repository ([`Cursor::from_shas`]), so a
/// restart reads nothing twice and a rewrite reads only the new objects. `git` runs on a
/// blocking thread.
///
/// # Errors
///
/// The repository cannot be read, or the store fails. Nothing is stored then.
pub async fn refresh(reader: &Reader, writer: &Writer, root: &Path) -> Result<usize, RefreshError> {
    let path = root.display().to_string();
    let key = canonical_path(&path, style_of(&path));
    let cursor = Cursor::from_shas(reader.commit_shas(&key).await?);
    let dir = root.to_owned();
    let found = tokio::task::spawn_blocking(move || scan(&dir, &cursor))
        .await
        .map_err(|e| RefreshError::Task(e.to_string()))?
        .map_err(RefreshError::Git)?;
    let read = found.commits.len();
    let mut batch = vec![Write::Repo(Repo {
        path: path.clone(),
        remote: found.remote,
        default_branch: found.default_branch,
    })];
    batch.extend(found.commits.into_iter().map(|commit| Write::Commit {
        repo: path.clone(),
        commit,
    }));
    writer.write(batch).await?;
    Ok(read)
}
