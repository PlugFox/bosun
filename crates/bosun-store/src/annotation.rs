//! Annotations (#11, part D): a timestamped line about a repository, such as the landing of a
//! heat proposal's remedy, meant to be shown on trend charts. The table exists since migration
//! 0001; [`crate::Write::Annotation`] writes it and [`Reader::annotations`] reads it.

use bosun_core::{canonical_path, style_of};
use sqlx::sqlite::SqliteConnection;

use crate::error::Error;
use crate::read::Reader;
use crate::writer::repo_at;

/// One stored annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationRow {
    pub ts_ms: i64,
    /// The repository's current path key; `None` for an annotation of no repository.
    pub repo: Option<String>,
    pub text: String,
}

/// Stores an annotation of the repository at path key `repo` (or a path it was configured at
/// before); an unknown repository is refused.
pub(crate) async fn add(
    conn: &mut SqliteConnection,
    ts_ms: i64,
    repo: &str,
    text: &str,
) -> Result<(), Error> {
    let key = canonical_path(repo, style_of(repo));
    let Some(repo_id) = repo_at(conn, &key).await? else {
        return Err(Error::Refused(format!(
            "annotation is not stored: unknown repository {repo}"
        )));
    };
    sqlx::query!(
        "INSERT INTO annotation (ts_ms, repo_id, text) VALUES (?, ?, ?)",
        ts_ms,
        repo_id,
        text,
    )
    .execute(conn)
    .await?;
    Ok(())
}

impl Reader {
    /// The annotations of `[from_ms, to_ms)`, by time, then by id.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn annotations(&self, from_ms: i64, to_ms: i64) -> Result<Vec<AnnotationRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT a.ts_ms, r.path AS "repo?: String", a.text
               FROM annotation a LEFT JOIN repo r ON r.id = a.repo_id
               WHERE a.ts_ms >= ? AND a.ts_ms < ?
               ORDER BY a.ts_ms, a.id"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| AnnotationRow {
                ts_ms: row.ts_ms,
                repo: row.repo,
                text: row.text,
            })
            .collect())
    }
}
