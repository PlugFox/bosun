//! What the report reads of the loop's tables (#72): each item's history, and the commits of
//! the runs' commit ranges. Every number the report prints is computed by the caller
//! (`bosun-heat`).

use bosun_core::{Commit, ItemId};

use super::read::{AgentCallRow, AttemptRow, DenialRow, RunRow, StoredItem, TransitionRow};
use crate::error::{Error, from_db};
use crate::read::Reader;

/// One item and everything recorded of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemHistory {
    pub item: StoredItem,
    /// The park reason, outcome or needs-you reason of its state, as stored (the names of the
    /// `item` table).
    pub detail: Option<String>,
    /// By number.
    pub attempts: Vec<AttemptRow>,
    /// Every session bosun spawned for it, in order.
    pub runs: Vec<RunRow>,
    /// Its arrows, in order.
    pub transitions: Vec<TransitionRow>,
    /// Attempt number and test name, in order.
    pub flaky_tests: Vec<(u32, String)>,
    /// The agent's calls, refused ones too, in order.
    pub agent_calls: Vec<AgentCallRow>,
    /// The tool calls refused during its attempts, by attempt, session and position.
    pub denials: Vec<DenialRow>,
}

/// A stored commit that a session's commit range lists ([`crate::Write::RunCommits`]): the
/// session that made it and the session's attempt and item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCommitRow {
    /// The repository's path key.
    pub repo: String,
    pub commit: Commit,
    pub item: ItemId,
    pub attempt: u32,
    /// The session's number.
    pub spawn: u32,
    pub provider: String,
    /// The session's provider session id.
    pub session_key: String,
}

impl Reader {
    /// The history of every item with a transition at `from_ms <= ts < to_ms`, by item id.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn item_histories(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<ItemHistory>, Error> {
        let rows = sqlx::query!(
            r#"SELECT i.item_key,
                      coalesce(i.park_reason, i.outcome, i.reason) AS "detail?: String"
               FROM item i
               WHERE EXISTS (SELECT 1 FROM transition t
                             WHERE t.item_id = i.id AND t.ts_ms >= ? AND t.ts_ms < ?)
               ORDER BY i.item_key"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id = ItemId::new(row.item_key);
            let Some(item) = self.item(&id).await? else {
                continue;
            };
            out.push(ItemHistory {
                item,
                detail: row.detail,
                attempts: self.attempts(&id).await?,
                runs: self.runs(&id).await?,
                transitions: self.transitions(&id).await?,
                flaky_tests: self.flaky_tests(&id).await?,
                agent_calls: self.agent_calls(&id).await?,
                denials: self.denials(&id).await?,
            });
        }
        Ok(out)
    }

    /// The stored commits with `from_ms <= ts < to_ms` that a session's commit range lists,
    /// oldest first (then by repository and sha), each with its session.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn run_commits(&self, from_ms: i64, to_ms: i64) -> Result<Vec<RunCommitRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT r.path AS repo, c.sha, c.ts_ms, c.author, c.lines_added, c.lines_removed,
                      c.trailers_json, i.item_key, a.number, run.spawn, run.provider,
                      run.session_key
               FROM "commit" c
               JOIN repo r ON r.id = c.repo_id
               JOIN run_commit rc ON rc.repo_id = c.repo_id AND rc.sha = c.sha
               JOIN run ON run.id = rc.run_id
               JOIN attempt a ON a.id = run.attempt_id
               JOIN item i ON i.id = a.item_id
               WHERE c.ts_ms >= ? AND c.ts_ms < ?
               ORDER BY c.ts_ms, r.path, c.sha"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        let number = |value: i64, what: &str| {
            u32::try_from(value)
                .map_err(|_| Error::Corrupt(format!("{what} out of range: {value}")))
        };
        rows.into_iter()
            .map(|row| {
                let trailers = serde_json::from_str(&row.trailers_json)
                    .map_err(|e| Error::Corrupt(format!("trailers of commit {}: {e}", row.sha)))?;
                Ok(RunCommitRow {
                    repo: row.repo,
                    commit: Commit {
                        sha: row.sha,
                        ts_ms: row.ts_ms,
                        author: row.author,
                        lines_added: from_db(row.lines_added, "lines added")?,
                        lines_removed: from_db(row.lines_removed, "lines removed")?,
                        trailers,
                    },
                    item: ItemId::new(row.item_key),
                    attempt: number(row.number, "attempt number")?,
                    spawn: number(row.spawn, "spawn number")?,
                    provider: row.provider,
                    session_key: row.session_key,
                })
            })
            .collect()
    }
}
