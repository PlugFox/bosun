//! Commits and the sessions they are linked to (#3, "Linking cost to commits"; #23).
//!
//! The link is a query, never a stored column. In stage 0 every link is [`Link::Inferred`]: a
//! commit is linked to each session whose time span and repository contain it.
//!
//! - **Time.** The session's span is `[started, ended]`, from its first to its last record. Git
//!   keeps the commit time in whole seconds, so a commit stamped `t` was made somewhere in
//!   `[t, t + 999 ms]`, and it is in the span when that second overlaps it. The time is the
//!   committer's: a commit amended, rebased or cherry-picked in a session was made there.
//!   A session without a start or an end has no span and links nothing.
//! - **Repository.** A session's repositories are those its messages are attributed to
//!   (`message.repo_id`); a session none of whose messages is attributed falls back to the
//!   repository that contains its first working directory (the innermost one, on whole path
//!   components).
//! - **Several matches.** A commit is linked to every session that matches, ordered by start,
//!   then by row id: parallel sessions in one repository, and a sub-agent inside its parent,
//!   all contain the commit, and the query does not pretend to know which one made it. A report
//!   that needs one owner applies its own rule on top; the exact link by trailer arrives with
//!   stage 1 (#7).

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::{Commit, PathStyle, canonical_path};

use crate::error::{Error, from_db};
use crate::read::Reader;

/// How a commit is linked to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Link {
    /// By time span and repository: an estimate.
    Inferred,
}

/// A session a commit is linked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedSession {
    pub provider: String,
    /// The provider's session id.
    pub session_key: String,
    /// The sub-agent id; `None` for the main context.
    pub agent_key: Option<String>,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub link: Link,
}

/// A commit with the sessions it is linked to; none when it is outside every session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitLink {
    /// The repository's path key.
    pub repo: String,
    pub commit: Commit,
    pub sessions: Vec<LinkedSession>,
}

/// Git's resolution: a commit stamped `t` was made within `[t, t + SECOND)`.
const SECOND: i64 = 1000;

struct Span {
    session: LinkedSession,
    repos: BTreeSet<i64>,
}

impl Reader {
    /// The shas of the commits stored for the repository with path key `repo`, ascending: the
    /// cursor a commit reader resumes from after a restart.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn commit_shas(&self, repo: &str) -> Result<Vec<String>, Error> {
        Ok(sqlx::query_scalar!(
            r#"SELECT c.sha FROM "commit" c JOIN repo r ON r.id = c.repo_id
               WHERE r.path = ? ORDER BY c.sha"#,
            repo,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The commits with `from_ms <= ts < to_ms`, oldest first, each with the sessions it is
    /// linked to by the rule of the module documentation.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn commit_links(&self, from_ms: i64, to_ms: i64) -> Result<Vec<CommitLink>, Error> {
        // A session that can contain a commit of the range: it starts before the last second
        // of the range ends and ends after the range starts.
        let span_end = to_ms.saturating_add(SECOND - 1);
        let mut conn = self.pool.acquire().await?;
        // One read transaction, so the commits and the sessions are one snapshot.
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let commits = sqlx::query!(
            r#"SELECT c.repo_id, r.path AS repo, c.sha, c.ts_ms, c.author,
                      c.lines_added, c.lines_removed, c.trailers_json
               FROM "commit" c JOIN repo r ON r.id = c.repo_id
               WHERE c.ts_ms >= ? AND c.ts_ms < ?
               ORDER BY c.ts_ms, r.path, c.sha"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        let sessions = sqlx::query!(
            r#"SELECT id AS "id!: i64", provider, session_key, agent_key, first_cwd,
                      started_ms AS "started_ms!: i64", ended_ms AS "ended_ms!: i64"
               FROM session
               WHERE started_ms IS NOT NULL AND ended_ms IS NOT NULL
                 AND started_ms < ? AND ended_ms >= ?
               ORDER BY started_ms, id"#,
            span_end,
            from_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        let attributed = sqlx::query!(
            r#"SELECT DISTINCT m.session_id, m.repo_id AS "repo_id!: i64"
               FROM message m JOIN session s ON s.id = m.session_id
               WHERE m.repo_id IS NOT NULL
                 AND s.started_ms < ? AND s.ended_ms >= ?"#,
            span_end,
            from_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        let repos = sqlx::query!(r#"SELECT id AS "id!: i64", path FROM repo"#)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        let mut by_session: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
        for row in attributed {
            by_session
                .entry(row.session_id)
                .or_default()
                .insert(row.repo_id);
        }
        let repos: Vec<(i64, String)> = repos.into_iter().map(|r| (r.id, r.path)).collect();
        let spans: Vec<Span> = sessions
            .into_iter()
            .map(|s| {
                let repos = by_session.remove(&s.id).unwrap_or_else(|| {
                    s.first_cwd
                        .as_deref()
                        .and_then(|cwd| containing_repo(&repos, cwd))
                        .into_iter()
                        .collect()
                });
                Span {
                    session: LinkedSession {
                        provider: s.provider,
                        session_key: s.session_key,
                        agent_key: s.agent_key,
                        started_ms: s.started_ms,
                        ended_ms: s.ended_ms,
                        link: Link::Inferred,
                    },
                    repos,
                }
            })
            .collect();

        let mut out = Vec::with_capacity(commits.len());
        for c in commits {
            let trailers = serde_json::from_str(&c.trailers_json)
                .map_err(|e| Error::Corrupt(format!("trailers of commit {}: {e}", c.sha)))?;
            let sessions = spans
                .iter()
                .filter(|s| {
                    s.session.started_ms < c.ts_ms.saturating_add(SECOND)
                        && s.session.ended_ms >= c.ts_ms
                })
                .filter(|s| cfg!(mutant_commit_time_only) || s.repos.contains(&c.repo_id))
                .map(|s| s.session.clone())
                .collect();
            out.push(CommitLink {
                repo: c.repo,
                commit: Commit {
                    sha: c.sha,
                    ts_ms: c.ts_ms,
                    author: c.author,
                    lines_added: from_db(c.lines_added, "lines added")?,
                    lines_removed: from_db(c.lines_removed, "lines removed")?,
                    trailers,
                },
                sessions,
            });
        }
        Ok(out)
    }
}

/// The innermost repository whose path is `cwd` or an ancestor of it, on whole components.
/// Repository keys are canonical for this host, so `cwd` is canonicalised by the same rules.
fn containing_repo(repos: &[(i64, String)], cwd: &str) -> Option<i64> {
    let style = PathStyle::host();
    let separator = match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    };
    let cwd = canonical_path(cwd, style);
    repos
        .iter()
        .filter(|(_, root)| {
            cwd.strip_prefix(root.as_str()).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(separator) || root.ends_with(separator)
            })
        })
        .max_by_key(|(_, root)| root.len())
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sqlx::Connection;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

    use super::*;
    use crate::writer::tests::{record, source_file, write};
    use crate::{Options, Repo, Store, Write};

    fn key(path: &str) -> String {
        canonical_path(path, PathStyle::host())
    }

    fn commit(sha: &str, ts_ms: i64) -> Commit {
        Commit {
            sha: sha.to_owned(),
            ts_ms,
            author: "A U Thor <author@example.invalid>".to_owned(),
            lines_added: 3,
            lines_removed: 1,
            trailers: BTreeMap::from([("Item".to_owned(), vec!["#1".to_owned()])]),
        }
    }

    /// Runs `sql` on a connection of its own: the tests' stand-in for writes no `Write` makes
    /// yet (session spans and message attribution belong to ingest, #21).
    async fn exec(db: &Path, sql: &str) {
        let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(db))
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }

    fn keys(link: &CommitLink) -> Vec<&str> {
        link.sessions
            .iter()
            .map(|s| s.session_key.as_str())
            .collect()
    }

    #[tokio::test]
    async fn commits_are_stored_once_by_repository_and_sha() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let (a, b) = (key("/w/a"), key("/w/b"));
        let mut changed = commit("c1", 1_000);
        changed.author = "Someone Else <else@example.invalid>".to_owned();
        let batch = vec![
            Write::Repo(Repo {
                path: a.clone(),
                remote: Some("https://example.invalid/a.git".to_owned()),
                default_branch: Some("main".to_owned()),
            }),
            Write::Commit {
                repo: a.clone(),
                commit: commit("c1", 1_000),
            },
            // The same sha in another repository is another commit; B is registered by it.
            Write::Commit {
                repo: b.clone(),
                commit: commit("c1", 1_000),
            },
        ];
        store.writer().write(batch.clone()).await.unwrap();
        store.writer().write(batch).await.unwrap();
        // A stored commit is never rewritten.
        store
            .writer()
            .write(vec![Write::Commit {
                repo: a.clone(),
                commit: changed,
            }])
            .await
            .unwrap();

        assert_eq!(store.reader().commit_shas(&a).await.unwrap(), ["c1"]);
        assert_eq!(store.reader().commit_shas(&b).await.unwrap(), ["c1"]);
        let links = store.reader().commit_links(0, i64::MAX).await.unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|l| l.commit == commit("c1", 1_000)));
        let repos = sqlx::query!("SELECT path, remote, default_branch FROM repo ORDER BY path")
            .fetch_all(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(
            repos[0].remote.as_deref(),
            Some("https://example.invalid/a.git")
        );
        assert_eq!(repos[0].default_branch.as_deref(), Some("main"));
        assert_eq!(repos[1].remote, None);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn sessions_link_by_span_and_attributed_repository() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bosun.db");
        let store = Store::open(&db, Options::default()).await.unwrap();
        let (a, b) = (key("/w/a"), key("/w/b"));
        store
            .writer()
            .write(vec![
                Write::Commit {
                    repo: a.clone(),
                    commit: commit("in-a", 10_000),
                },
                Write::Commit {
                    repo: b.clone(),
                    commit: commit("in-b", 10_000),
                },
                Write::Commit {
                    repo: a.clone(),
                    commit: commit("late", 30_000),
                },
                // A session with a message: its window row is created by the write.
                source_file("f1"),
                write(record("m1", "f1", 9_000, 1, &[])),
            ])
            .await
            .unwrap();
        // `attributed` starts in A but its messages are in B: the messages decide. `by-cwd`
        // has no attributed message and falls back to its first cwd. `edge` ends inside the
        // commit's second, `starts-late` starts after it, `open` has no end.
        let b_sql = b.replace('\'', "''");
        exec(
            &db,
            &format!(
                "UPDATE session SET session_key = 'attributed', first_cwd = '/w/a/src',
                                    started_ms = 5000, ended_ms = 20000
                     WHERE session_key = 'session-f1';
                 UPDATE message SET repo_id = (SELECT id FROM repo WHERE path = '{b_sql}');
                 INSERT INTO session (provider, session_key, first_cwd, started_ms, ended_ms)
                 VALUES ('claude', 'by-cwd', '/w/a', 1000, 10000),
                        ('claude', 'edge', '/w/a/x', 2000, 10500),
                        ('claude', 'starts-late', '/w/a', 11000, 12000),
                        ('claude', 'open', '/w/a', 1000, NULL);"
            ),
        )
        .await;

        let links = store.reader().commit_links(0, i64::MAX).await.unwrap();
        let by_sha: BTreeMap<&str, &CommitLink> =
            links.iter().map(|l| (l.commit.sha.as_str(), l)).collect();
        assert_eq!(keys(by_sha["in-a"]), ["by-cwd", "edge"]);
        assert_eq!(keys(by_sha["in-b"]), ["attributed"]);
        assert_eq!(keys(by_sha["late"]), Vec::<&str>::new());
        assert!(
            links
                .iter()
                .flat_map(|l| &l.sessions)
                .all(|s| s.link == Link::Inferred)
        );
        // The range selects commits by their time.
        let range = store.reader().commit_links(10_000, 10_001).await.unwrap();
        assert_eq!(range.len(), 2);
        store.close().await.unwrap();
    }

    #[test]
    fn a_cwd_belongs_to_the_innermost_repository_on_whole_components() {
        let (outer, inner, sibling) = if cfg!(windows) {
            (r"c:\w\repo", r"c:\w\repo\sub", r"c:\w\repo-two")
        } else {
            ("/w/repo", "/w/repo/sub", "/w/repo-two")
        };
        let repos = vec![
            (1, outer.to_owned()),
            (2, inner.to_owned()),
            (3, sibling.to_owned()),
        ];
        let at = |p: &str| containing_repo(&repos, p);
        assert_eq!(at(outer), Some(1));
        assert_eq!(at(&format!("{outer}/src")), Some(1));
        assert_eq!(at(&format!("{inner}/x")), Some(2));
        assert_eq!(at(sibling), Some(3));
        assert_eq!(at(&format!("{outer}x")), None);
        if cfg!(windows) {
            assert_eq!(at(r"C:/W/Repo/src"), Some(1), "case and separators");
        }
    }
}
