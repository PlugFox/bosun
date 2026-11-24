//! Recent sessions in a repository (#67): what the automatic pause reads.

use crate::{Error, Reader};

/// A context window with messages in a repository since some time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    /// The provider's session id.
    pub session_key: String,
    /// bosun spawned it (its id is a `run` row's): not the user's interactive work.
    pub run: bool,
    /// Its last message in the repository.
    pub last_ms: i64,
}

impl Reader {
    /// The sessions with a message attributed to the repository with path key `repo` at or
    /// after `since_ms`, each with its last such message, in session order. A session that is
    /// both (a sub-agent window of a run) counts as a run.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn recent_sessions(
        &self,
        repo: &str,
        since_ms: i64,
    ) -> Result<Vec<RecentSession>, Error> {
        let rows = sqlx::query!(
            r#"SELECT s.session_key AS "session_key!", MAX(s.kind = 'run') AS "run!: bool",
                      MAX(m.ts_ms) AS "last_ms!: i64"
               FROM message m
               JOIN session s ON s.id = m.session_id
               JOIN repo r ON r.id = m.repo_id
               WHERE r.path = ? AND m.ts_ms >= ?
               GROUP BY s.session_key
               ORDER BY s.session_key"#,
            repo,
            since_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| RecentSession {
                session_key: row.session_key,
                run: row.run,
                last_ms: row.last_ms,
            })
            .collect())
    }
}
