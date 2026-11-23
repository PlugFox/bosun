//! Reader queries over the loop's tables: for the loop (items as the machine holds them, the
//! children to reap, the outbox to drain), the report and the API (the history).

use bosun_core::{Attempt, Column, Exit, Item, ItemId, Reason, RunResult, SpawnMode};

use super::codec;
use super::{AgentCallKind, Snapshot};
use crate::error::{Error, from_db};
use crate::read::Reader;

/// An item as recorded, with what the machine does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    /// The item, exactly as the last recorded transition left it.
    pub item: Item,
    /// The repository's canonical path key.
    pub repo: String,
    pub card_key: Option<String>,
    /// The hash of the current snapshot, if one was taken.
    pub snapshot_hash: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// One recorded arrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRow {
    /// `<item>/t<seq>`.
    pub key: String,
    pub seq: u32,
    pub attempt: u32,
    /// State names ([`bosun_core::State::as_str`]).
    pub from: String,
    pub to: String,
    /// The park reason, outcome or reason of `to`.
    pub to_detail: Option<String>,
    pub attempt_from: Option<String>,
    pub attempt_to: Option<String>,
    pub cause: String,
    pub ts_ms: i64,
}

/// One attempt and its range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRow {
    pub attempt: Attempt,
    /// The hash of the snapshot it ran from.
    pub snapshot_hash: Option<String>,
    pub head_sha: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

/// One session bosun spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub attempt: u32,
    pub spawn: u32,
    pub mode: SpawnMode,
    pub provider: String,
    pub session_key: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub exit: Option<Exit>,
    /// HEAD when the session started and when it ended, once its range is recorded
    /// ([`crate::Write::RunCommits`]).
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
}

/// One verdict run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRow {
    pub attempt: u32,
    pub run: u32,
    pub command: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    /// `None` while it runs.
    pub result: Option<RunResult>,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
    pub verdict_line: Option<String>,
    pub tail: Option<String>,
}

/// One call of the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCallRow {
    pub attempt: u32,
    pub kind: AgentCallKind,
    pub body: String,
    pub head_sha: Option<String>,
    pub refused: bool,
    pub ts_ms: i64,
}

/// A child process recorded as running: a session or a verdict run. Crash recovery reaps it
/// by pid and start time (#50).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedChild {
    pub item: ItemId,
    pub attempt: u32,
    /// The verdict run's number; `None` for a session.
    pub verdict_run: Option<u32>,
    pub pid: u32,
    pub started_ms: i64,
}

/// A board write as the outbox holds it: what to write, never a rendered body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxOp {
    /// Write a daemon-owned column.
    Project(Column),
    /// Post a new comment for the user, rendered at send time.
    Comment(Reason),
    /// Create or edit the sticky item report, rendered at send time.
    UpsertComment,
}

/// A pending board write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    /// Acknowledge this revision: a write folded in since keeps the row pending.
    pub revision: u32,
    pub item: ItemId,
    /// The write's idempotence key; a comment carries it as its hidden key.
    pub effect_key: String,
    pub op: OutboxOp,
    /// The key pending writes coalesce under; for the sticky report, its comment key.
    pub coalesce_key: Option<String>,
    pub tries: u32,
    pub last_error: Option<String>,
    pub created_ms: i64,
}

fn number(value: i64, what: &'static str) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::Corrupt(format!("{what} out of range: {value}")))
}

fn strings(json: Option<&str>) -> Result<Vec<String>, Error> {
    match json {
        None => Ok(Vec::new()),
        Some(text) => serde_json::from_str(text)
            .map_err(|e| Error::Corrupt(format!("an agent call's list: {e}"))),
    }
}

impl Reader {
    /// The item `id` as recorded, or `None` when it is not known.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn item(&self, id: &ItemId) -> Result<Option<StoredItem>, Error> {
        Ok(self.items_where(Some(id), false).await?.into_iter().next())
    }

    /// Every item, by id. With `active`, only those in a daemon-owned state with work in
    /// flight ([`bosun_core::State::is_active`]): the items crash recovery visits.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn items(&self, active: bool) -> Result<Vec<StoredItem>, Error> {
        self.items_where(None, active).await
    }

    async fn items_where(
        &self,
        only: Option<&ItemId>,
        active: bool,
    ) -> Result<Vec<StoredItem>, Error> {
        let key = only.map(|id| id.0.as_str());
        let rows = sqlx::query!(
            r#"SELECT i.item_key, r.path AS repo, i.card_key, i.state, i.park_reason,
                      i.park_until_ms, i.park_next, i.outcome, i.reason, i.seq, i.attempts,
                      i.spawns, i.runs, i.failed, i.uncounted, i.boot, i.created_ms,
                      i.updated_ms, s.hash AS "snapshot_hash?: String",
                      a.number AS "a_number?: i64", a.spawn AS "a_spawn?: i64",
                      a.run AS "a_run?: i64", a.state AS "a_state?: String",
                      a.first_run AS "a_first_run?: String",
                      a.crash_retries AS "a_crash_retries?: i64"
               FROM item i
               JOIN repo r ON r.id = i.repo_id
               LEFT JOIN snapshot s ON s.id = i.snapshot_id
               LEFT JOIN attempt a ON a.item_id = i.id AND a.number = i.attempt_number
               WHERE (?1 IS NULL OR i.item_key = ?1)
               ORDER BY i.item_key"#,
            key,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let state = codec::parse_state(
                &row.state,
                row.park_reason.as_deref(),
                row.park_until_ms,
                row.park_next.as_deref(),
                row.outcome.as_deref(),
                row.reason.as_deref(),
            )?;
            if active && !state.is_active() {
                continue;
            }
            let attempt = match (row.a_number, row.a_spawn, row.a_state) {
                (Some(n), Some(spawn), Some(state)) => Some(Attempt {
                    number: number(n, "attempt number")?,
                    spawn: number(spawn, "spawn number")?,
                    run: number(row.a_run.unwrap_or(0), "verdict run number")?,
                    state: codec::parse_attempt_state(&state)?,
                    first_run: row
                        .a_first_run
                        .as_deref()
                        .map(codec::parse_run_result)
                        .transpose()?,
                    crash_retries: number(row.a_crash_retries.unwrap_or(0), "crash retries")?,
                }),
                _ => None,
            };
            items.push(StoredItem {
                item: Item {
                    id: ItemId::new(row.item_key),
                    state,
                    attempt,
                    seq: number(row.seq, "transition count")?,
                    attempts: number(row.attempts, "attempt count")?,
                    spawns: number(row.spawns, "session count")?,
                    runs: number(row.runs, "verdict run count")?,
                    failed: number(row.failed, "failed attempts")?,
                    uncounted: number(row.uncounted, "uncounted parks")?,
                    boot: number(row.boot, "boot")?,
                },
                repo: row.repo,
                card_key: row.card_key,
                snapshot_hash: row.snapshot_hash,
                created_ms: row.created_ms,
                updated_ms: row.updated_ms,
            });
        }
        Ok(items)
    }

    /// The transitions of `item`, in order.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn transitions(&self, item: &ItemId) -> Result<Vec<TransitionRow>, Error> {
        let rows = sqlx::query!(
            "SELECT t.transition_key, t.seq, t.attempt, t.from_state, t.to_state, t.to_detail,
                    t.attempt_from, t.attempt_to, t.cause, t.ts_ms
             FROM transition t JOIN item i ON i.id = t.item_id
             WHERE i.item_key = ? ORDER BY t.seq, t.id",
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TransitionRow {
                    key: row.transition_key,
                    seq: number(row.seq, "transition number")?,
                    attempt: number(row.attempt, "attempt number")?,
                    from: row.from_state,
                    to: row.to_state,
                    to_detail: row.to_detail,
                    attempt_from: row.attempt_from,
                    attempt_to: row.attempt_to,
                    cause: row.cause,
                    ts_ms: row.ts_ms,
                })
            })
            .collect()
    }

    /// The current snapshot of `item`: the one its next attempt runs from.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn snapshot(&self, item: &ItemId) -> Result<Option<Snapshot>, Error> {
        let row = sqlx::query!(
            "SELECT s.snapshot_key, s.taken_ms, s.hash, s.body, s.contract_json, s.comments_json,
                    s.config_hash, s.policy_hash, s.base_sha, s.branch
             FROM item i JOIN snapshot s ON s.id = i.snapshot_id
             WHERE i.item_key = ?",
            item.0,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| Snapshot {
            item: item.clone(),
            key: row.snapshot_key,
            taken_ms: row.taken_ms,
            hash: row.hash,
            body: row.body,
            contract_json: row.contract_json,
            comments_json: row.comments_json,
            config_hash: row.config_hash,
            policy_hash: row.policy_hash,
            base_sha: row.base_sha,
            branch: row.branch,
        }))
    }

    /// The attempts of `item`, by number.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn attempts(&self, item: &ItemId) -> Result<Vec<AttemptRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT a.number, a.spawn, a.run, a.state, a.first_run, a.crash_retries,
                      a.head_sha, a.started_ms, a.ended_ms, s.hash AS "snapshot_hash?: String"
               FROM attempt a
               JOIN item i ON i.id = a.item_id
               LEFT JOIN snapshot s ON s.id = a.snapshot_id
               WHERE i.item_key = ? ORDER BY a.number"#,
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(AttemptRow {
                    attempt: Attempt {
                        number: number(row.number, "attempt number")?,
                        spawn: number(row.spawn, "spawn number")?,
                        run: number(row.run, "verdict run number")?,
                        state: codec::parse_attempt_state(&row.state)?,
                        first_run: row
                            .first_run
                            .as_deref()
                            .map(codec::parse_run_result)
                            .transpose()?,
                        crash_retries: number(row.crash_retries, "crash retries")?,
                    },
                    snapshot_hash: row.snapshot_hash,
                    head_sha: row.head_sha,
                    started_ms: row.started_ms,
                    ended_ms: row.ended_ms,
                })
            })
            .collect()
    }

    /// The sessions bosun spawned for `item`, in order.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn runs(&self, item: &ItemId) -> Result<Vec<RunRow>, Error> {
        let rows = sqlx::query!(
            "SELECT a.number, r.spawn, r.mode, r.provider, r.session_key, r.started_ms,
                    r.ended_ms, r.exit, r.limit_reset_ms, r.base_sha, r.head_sha
             FROM run r JOIN attempt a ON a.id = r.attempt_id JOIN item i ON i.id = a.item_id
             WHERE i.item_key = ? ORDER BY r.spawn",
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RunRow {
                    attempt: number(row.number, "attempt number")?,
                    spawn: number(row.spawn, "spawn number")?,
                    mode: codec::parse_spawn_mode(&row.mode)?,
                    provider: row.provider,
                    session_key: row.session_key,
                    started_ms: row.started_ms,
                    ended_ms: row.ended_ms,
                    exit: row
                        .exit
                        .as_deref()
                        .map(|e| codec::parse_exit(e, row.limit_reset_ms))
                        .transpose()?,
                    base_sha: row.base_sha,
                    head_sha: row.head_sha,
                })
            })
            .collect()
    }

    /// The verdict runs of `item`, in order.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn verdict_runs(&self, item: &ItemId) -> Result<Vec<VerdictRow>, Error> {
        let rows = sqlx::query!(
            "SELECT a.number, v.run, v.command, v.started_ms, v.ended_ms, v.result, v.exit_code,
                    v.duration_ms, v.verdict_line, v.tail
             FROM verdict_run v
             JOIN attempt a ON a.id = v.attempt_id
             JOIN item i ON i.id = a.item_id
             WHERE i.item_key = ? ORDER BY v.run",
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(VerdictRow {
                    attempt: number(row.number, "attempt number")?,
                    run: number(row.run, "verdict run number")?,
                    command: row.command,
                    started_ms: row.started_ms,
                    ended_ms: row.ended_ms,
                    result: row
                        .result
                        .as_deref()
                        .map(codec::parse_run_result)
                        .transpose()?,
                    exit_code: row.exit_code,
                    duration_ms: row
                        .duration_ms
                        .map(|d| from_db(d, "verdict duration"))
                        .transpose()?,
                    verdict_line: row.verdict_line,
                    tail: row.tail,
                })
            })
            .collect()
    }

    /// The agent's calls for `item`, in order.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn agent_calls(&self, item: &ItemId) -> Result<Vec<AgentCallRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT a.number, c.kind, c.body, c.decisions_json, c.followups_json,
                      c.blocking AS "blocking?: bool", c.head_sha, c.refused AS "refused!: bool",
                      c.ts_ms
               FROM agent_call c
               JOIN attempt a ON a.id = c.attempt_id
               JOIN item i ON i.id = a.item_id
               WHERE i.item_key = ? ORDER BY c.ts_ms, c.id"#,
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let kind = match row.kind.as_str() {
                    "summary" => AgentCallKind::Summary {
                        decisions: strings(row.decisions_json.as_deref())?,
                        followups: strings(row.followups_json.as_deref())?,
                    },
                    "question" => AgentCallKind::Question {
                        blocking: row.blocking.unwrap_or(false),
                    },
                    "note" => AgentCallKind::Note,
                    other => return Err(Error::Corrupt(format!("unknown agent call {other:?}"))),
                };
                Ok(AgentCallRow {
                    attempt: number(row.number, "attempt number")?,
                    kind,
                    body: row.body,
                    head_sha: row.head_sha,
                    refused: row.refused,
                    ts_ms: row.ts_ms,
                })
            })
            .collect()
    }

    /// The summary of attempt `attempt` of `item`: the last accepted `submit_summary` call.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn summary(
        &self,
        item: &ItemId,
        attempt: u32,
    ) -> Result<Option<AgentCallRow>, Error> {
        Ok(self.agent_calls(item).await?.into_iter().rfind(|c| {
            c.attempt == attempt && !c.refused && matches!(c.kind, AgentCallKind::Summary { .. })
        }))
    }

    /// The children recorded as running, of every item: what crash recovery reaps.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn children(&self) -> Result<Vec<RecordedChild>, Error> {
        let rows = sqlx::query!(
            r#"SELECT i.item_key, a.number, NULL AS "verdict_run?: i64",
                      a.pid AS "pid!: i64", a.pid_started_ms AS "started!: i64"
               FROM attempt a JOIN item i ON i.id = a.item_id
               WHERE a.pid IS NOT NULL
               UNION ALL
               SELECT i.item_key, a.number, v.run,
                      v.pid, v.pid_started_ms
               FROM verdict_run v
               JOIN attempt a ON a.id = v.attempt_id
               JOIN item i ON i.id = a.item_id
               WHERE v.pid IS NOT NULL
               ORDER BY 1, 2, 3"#
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RecordedChild {
                    item: ItemId::new(row.item_key),
                    attempt: number(row.number, "attempt number")?,
                    verdict_run: row
                        .verdict_run
                        .map(|r| number(r, "verdict run number"))
                        .transpose()?,
                    pid: number(row.pid, "pid")?,
                    started_ms: row.started,
                })
            })
            .collect()
    }

    /// The pending board writes due at `now_ms`, in the order they were enqueued.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn outbox(&self, now_ms: i64) -> Result<Vec<OutboxRow>, Error> {
        let rows = sqlx::query!(
            "SELECT o.id, o.revision, i.item_key, o.effect_key, o.op, o.column_name, o.reason,
                    o.coalesce_key, o.tries, o.last_error, o.created_ms
             FROM outbox o JOIN item i ON i.id = o.item_id
             WHERE o.acked_ms IS NULL AND (o.retry_at_ms IS NULL OR o.retry_at_ms <= ?)
             ORDER BY o.id",
            now_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let op = match (row.op.as_str(), row.column_name, row.reason) {
                    ("project", Some(column), _) => {
                        OutboxOp::Project(codec::parse_column(&column)?)
                    }
                    ("comment", _, Some(reason)) => {
                        OutboxOp::Comment(codec::parse_reason(&reason)?)
                    }
                    ("upsert_comment", _, _) => OutboxOp::UpsertComment,
                    (op, _, _) => {
                        return Err(Error::Corrupt(format!(
                            "an outbox row {op:?} is incomplete"
                        )));
                    }
                };
                Ok(OutboxRow {
                    id: row.id,
                    revision: number(row.revision, "outbox revision")?,
                    item: ItemId::new(row.item_key),
                    effect_key: row.effect_key,
                    op,
                    coalesce_key: row.coalesce_key,
                    tries: number(row.tries, "outbox tries")?,
                    last_error: row.last_error,
                    created_ms: row.created_ms,
                })
            })
            .collect()
    }

    /// The node id of the board comment of `item` with key `key`, if the daemon created one.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn board_comment(&self, item: &ItemId, key: &str) -> Result<Option<String>, Error> {
        Ok(sqlx::query_scalar!(
            "SELECT c.node_id FROM board_comment c JOIN item i ON i.id = c.item_id
             WHERE i.item_key = ? AND c.comment_key = ?",
            item.0,
            key,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The node ids of every board comment the daemon created for `item`: the prompt builder
    /// leaves them out (#2).
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn board_comments(&self, item: &ItemId) -> Result<Vec<String>, Error> {
        Ok(sqlx::query_scalar!(
            "SELECT c.node_id FROM board_comment c JOIN item i ON i.id = c.item_id
             WHERE i.item_key = ? ORDER BY c.id",
            item.0,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The flaky tests of `item`: attempt number and test name, in order.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn flaky_tests(&self, item: &ItemId) -> Result<Vec<(u32, String)>, Error> {
        let rows = sqlx::query!(
            "SELECT a.number, f.test_name
             FROM flaky_test f JOIN attempt a ON a.id = f.attempt_id JOIN item i ON i.id = f.item_id
             WHERE i.item_key = ? ORDER BY a.number, f.test_name",
            item.0,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok((number(row.number, "attempt number")?, row.test_name)))
            .collect()
    }

    /// The number of the last recorded daemon start; the next start takes the one after.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn last_boot(&self) -> Result<Option<u32>, Error> {
        let last = sqlx::query_scalar!(r#"SELECT max(number) AS "n?: i64" FROM boot"#)
            .fetch_one(&self.pool)
            .await?;
        last.map(|n| number(n, "boot number")).transpose()
    }

    /// The state names of every item, with how many items are in each: for the overview.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn item_counts(&self) -> Result<Vec<(String, u64)>, Error> {
        let rows = sqlx::query!(
            r#"SELECT state, count(*) AS "n!: i64" FROM item GROUP BY state ORDER BY state"#
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok((row.state, from_db(row.n, "item count")?)))
            .collect()
    }
}
