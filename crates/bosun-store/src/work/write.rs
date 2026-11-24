//! The writer's handlers for the loop's tables. Each runs inside the batch's transaction.

use bosun_core::{Attempt, Effect, EffectKind, Item, ItemId, Stepped, canonical_path, style_of};
use sqlx::sqlite::SqliteConnection;

use super::codec;
use super::{
    AgentCall, AgentCallKind, NewItem, RunCommits, RunEnd, RunStart, Snapshot, VerdictEnd,
    VerdictStart,
};
use crate::error::{Error, to_db};
use crate::writer::{Repo, upsert_repo};

/// The row id of `item`.
pub(crate) async fn item_id(conn: &mut SqliteConnection, item: &ItemId) -> Result<i64, Error> {
    sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM item WHERE item_key = ?"#,
        item.0
    )
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| Error::UnknownItem(item.0.clone()))
}

/// The row id of attempt `number` of `item`.
pub(crate) async fn attempt_id(
    conn: &mut SqliteConnection,
    item: &ItemId,
    number: u32,
) -> Result<i64, Error> {
    sqlx::query_scalar!(
        r#"SELECT a.id AS "id!: i64" FROM attempt a JOIN item i ON i.id = a.item_id
           WHERE i.item_key = ? AND a.number = ?"#,
        item.0,
        number,
    )
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| Error::UnknownAttempt {
        item: item.0.clone(),
        attempt: number,
    })
}

pub(crate) async fn upsert_item(conn: &mut SqliteConnection, new: &NewItem) -> Result<(), Error> {
    upsert_repo(
        conn,
        &Repo {
            path: new.repo.clone(),
            remote: None,
            default_branch: None,
        },
    )
    .await?;
    let key = canonical_path(&new.repo, style_of(&new.repo));
    let repo = sqlx::query_scalar!(r#"SELECT id AS "id!: i64" FROM repo WHERE path = ?"#, key)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query!(
        "INSERT INTO item (item_key, repo_id, card_key, created_ms, updated_ms)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (item_key) DO UPDATE SET
             card_key = coalesce(excluded.card_key, card_key)",
        new.item.0,
        repo,
        new.card_key,
        new.at_ms,
        new.at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn insert_snapshot(
    conn: &mut SqliteConnection,
    snapshot: &Snapshot,
) -> Result<(), Error> {
    let item = item_id(conn, &snapshot.item).await?;
    let Some(id) = sqlx::query_scalar!(
        r#"INSERT INTO snapshot (item_id, snapshot_key, taken_ms, hash, body, contract_json,
                                 comments_json, config_hash, policy_hash, base_sha, branch,
                                 base_refs, base_stash)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (snapshot_key) DO NOTHING
           RETURNING id AS "id!: i64""#,
        item,
        snapshot.key,
        snapshot.taken_ms,
        snapshot.hash,
        snapshot.body,
        snapshot.contract_json,
        snapshot.comments_json,
        snapshot.config_hash,
        snapshot.policy_hash,
        snapshot.base_sha,
        snapshot.branch,
        snapshot.base_refs_json,
        snapshot.base_stash_json,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(());
    };
    sqlx::query!("UPDATE item SET snapshot_id = ? WHERE id = ?", id, item)
        .execute(conn)
        .await?;
    Ok(())
}

/// Records one arrow: the transition row under its key, then the item and attempt it produced
/// and its board writes. A transition whose key is stored is a replay and changes nothing; a
/// different arrow under that key, or one that does not follow the recorded item, is stale.
// Long because the mutant repeats the insert as its own checked query.
#[allow(clippy::too_many_lines)]
pub(crate) async fn record_transition(
    conn: &mut SqliteConnection,
    stepped: &Stepped,
    at_ms: i64,
) -> Result<(), Error> {
    let t = &stepped.transition;
    let item = &stepped.item;
    let item_row = item_id(conn, &item.id).await?;
    let key = t.key.to_string();
    let detail = codec::state_detail(t.to);
    let effects = serde_json::Value::from(
        t.effects
            .iter()
            .map(|e| serde_json::json!({ "key": e.key.to_string(), "kind": codec::effect_kind(e.kind) }))
            .collect::<Vec<_>>(),
    )
    .to_string();
    let from = t.from.as_str();
    let to = t.to.as_str();
    let attempt_from = t.attempt_from.map(bosun_core::AttemptState::as_str);
    let attempt_to = t.attempt_to.map(bosun_core::AttemptState::as_str);
    // Without the key a replay is a second row, which the unique key refuses: the batch fails.
    let inserted = if cfg!(mutant_store_transition_no_key) {
        sqlx::query_scalar!(
            r#"INSERT INTO transition (transition_key, item_id, seq, attempt, from_state, to_state,
                                       to_detail, attempt_from, attempt_to, cause, effects_json,
                                       ts_ms)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!: i64""#,
            key,
            item_row,
            t.key.seq,
            t.attempt,
            from,
            to,
            detail,
            attempt_from,
            attempt_to,
            t.cause,
            effects,
            at_ms,
        )
        .fetch_optional(&mut *conn)
        .await?
    } else {
        sqlx::query_scalar!(
            r#"INSERT INTO transition (transition_key, item_id, seq, attempt, from_state, to_state,
                                       to_detail, attempt_from, attempt_to, cause, effects_json,
                                       ts_ms)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT (transition_key) DO NOTHING
               RETURNING id AS "id!: i64""#,
            key,
            item_row,
            t.key.seq,
            t.attempt,
            from,
            to,
            detail,
            attempt_from,
            attempt_to,
            t.cause,
            effects,
            at_ms,
        )
        .fetch_optional(&mut *conn)
        .await?
    };
    if inserted.is_none() {
        // A replay is the same arrow; another arrow under the key was computed from a stale
        // item, and its effects must not run.
        let stored = sqlx::query!(
            "SELECT from_state, to_state, cause FROM transition WHERE transition_key = ?",
            key
        )
        .fetch_one(&mut *conn)
        .await?;
        if (
            stored.from_state.as_str(),
            stored.to_state.as_str(),
            stored.cause.as_str(),
        ) != (from, to, t.cause)
        {
            return Err(Error::Stale(format!(
                "{key} is stored as {} -> {} ({}), not {from} -> {to} ({})",
                stored.from_state, stored.to_state, stored.cause, t.cause
            )));
        }
        return Ok(());
    }
    // The arrow must follow the recorded item; one from an older state is stale.
    let recorded = sqlx::query_scalar!(
        r#"SELECT seq AS "seq!: i64" FROM item WHERE id = ?"#,
        item_row
    )
    .fetch_one(&mut *conn)
    .await?;
    if recorded + 1 != i64::from(t.key.seq) {
        return Err(Error::Stale(format!(
            "{key} does not follow the recorded transition {recorded}"
        )));
    }
    // The attempt a new one replaced keeps its final state (a failed verdict, #62), not the
    // last state an earlier transition recorded for it.
    if let Some(ended) = &t.ended {
        upsert_attempt(conn, item_row, ended, at_ms).await?;
    }
    if let Some(attempt) = &item.attempt {
        upsert_attempt(conn, item_row, attempt, at_ms).await?;
    }
    // An attempt that is no longer current (a restart or a requeue dropped it) has ended.
    let current = item.attempt.as_ref().map(|a| a.number);
    sqlx::query!(
        "UPDATE attempt SET ended_ms = ?
         WHERE item_id = ? AND ended_ms IS NULL AND number IS NOT ?",
        at_ms,
        item_row,
        current,
    )
    .execute(&mut *conn)
    .await?;
    update_item(conn, item_row, item, at_ms).await?;
    for effect in &t.effects {
        enqueue(conn, item_row, effect, at_ms).await?;
    }
    Ok(())
}

async fn upsert_attempt(
    conn: &mut SqliteConnection,
    item_row: i64,
    attempt: &Attempt,
    at_ms: i64,
) -> Result<(), Error> {
    let state = attempt.state.as_str();
    let first_run = attempt.first_run.map(codec::run_result);
    let ended = (attempt.state.passed() || attempt.state.failed()).then_some(at_ms);
    sqlx::query!(
        "INSERT INTO attempt (item_id, number, snapshot_id, state, spawn, run, first_run,
                              crash_retries, started_ms, ended_ms)
         VALUES (?, ?, (SELECT snapshot_id FROM item WHERE id = ?), ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (item_id, number) DO UPDATE SET
             state = excluded.state,
             spawn = excluded.spawn,
             run = excluded.run,
             first_run = excluded.first_run,
             crash_retries = excluded.crash_retries,
             ended_ms = coalesce(ended_ms, excluded.ended_ms)",
        item_row,
        attempt.number,
        item_row,
        state,
        attempt.spawn,
        attempt.run,
        first_run,
        attempt.crash_retries,
        at_ms,
        ended,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn update_item(
    conn: &mut SqliteConnection,
    item_row: i64,
    item: &Item,
    at_ms: i64,
) -> Result<(), Error> {
    let c = codec::state_columns(item.state);
    let attempt = item.attempt.as_ref().map(|a| a.number);
    sqlx::query!(
        "UPDATE item SET
             state = ?, park_reason = ?, park_until_ms = ?, park_next = ?, outcome = ?,
             reason = ?, attempt_number = ?, seq = ?, attempts = ?, spawns = ?, runs = ?,
             failed = ?, uncounted = ?, boot = ?, updated_ms = ?
         WHERE id = ?",
        c.state,
        c.park_reason,
        c.park_until_ms,
        c.park_next,
        c.outcome,
        c.reason,
        attempt,
        item.seq,
        item.attempts,
        item.spawns,
        item.runs,
        item.failed,
        item.uncounted,
        item.boot,
        at_ms,
        item_row,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Puts a board write in the outbox. Effects that are not board writes are not the outbox's
/// and are skipped. A pending row with the same coalesce key absorbs the write: its revision
/// goes up, so an acknowledgement of what was sent before leaves it pending.
pub(crate) async fn enqueue(
    conn: &mut SqliteConnection,
    item_row: i64,
    effect: &Effect,
    at_ms: i64,
) -> Result<(), Error> {
    let (op, column, reason) = match effect.kind {
        EffectKind::Project(column) => ("project", Some(codec::column(column)), None),
        EffectKind::Comment(reason) => ("comment", None, Some(codec::reason(reason))),
        EffectKind::Report => ("upsert_comment", None, None),
        _ => return Ok(()),
    };
    let key = effect.key.to_string();
    let known = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM outbox WHERE effect_key = ?"#,
        key
    )
    .fetch_optional(&mut *conn)
    .await?;
    if known.is_some() {
        return Ok(());
    }
    let coalesce = if cfg!(mutant_store_outbox_no_coalesce) {
        None
    } else {
        effect.coalesce_key()
    };
    if let Some(coalesce) = &coalesce {
        let folded = sqlx::query!(
            "UPDATE outbox SET revision = revision + 1, retry_at_ms = NULL
             WHERE coalesce_key = ? AND acked_ms IS NULL",
            coalesce,
        )
        .execute(&mut *conn)
        .await?;
        if folded.rows_affected() > 0 {
            return Ok(());
        }
    }
    sqlx::query!(
        "INSERT INTO outbox (item_id, effect_key, op, column_name, reason, coalesce_key,
                             created_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        item_row,
        key,
        op,
        column,
        reason,
        coalesce,
        at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn enqueue_effect(
    conn: &mut SqliteConnection,
    effect: &Effect,
    at_ms: i64,
) -> Result<(), Error> {
    let item = item_id(conn, &effect.key.item).await?;
    enqueue(conn, item, effect, at_ms).await
}

/// The board acknowledged revision `revision` of outbox row `id`. A row that absorbed a newer
/// write since stays pending, to be sent again.
pub(crate) async fn acknowledge(
    conn: &mut SqliteConnection,
    id: i64,
    revision: u32,
    at_ms: i64,
) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE outbox SET acked_ms = ?, last_error = NULL, retry_at_ms = NULL
         WHERE id = ? AND revision = ? AND acked_ms IS NULL",
        at_ms,
        id,
        revision,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A send of outbox row `id` failed; it is tried again from `retry_at_ms` on.
pub(crate) async fn send_failed(
    conn: &mut SqliteConnection,
    id: i64,
    error: &str,
    retry_at_ms: i64,
) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE outbox SET tries = tries + 1, last_error = ?, retry_at_ms = ?
         WHERE id = ? AND acked_ms IS NULL",
        error,
        retry_at_ms,
        id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Marks every session of `provider` with key `session_key` (the main transcript and its
/// sub-agents) as a run session when bosun spawned it.
pub(crate) async fn mark_run_session(
    conn: &mut SqliteConnection,
    provider: &str,
    session_key: &str,
) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE session SET kind = 'run'
         WHERE provider = ? AND session_key = ? AND kind <> 'run'
           AND EXISTS (SELECT 1 FROM run r WHERE r.provider = ? AND r.session_key = ?)",
        provider,
        session_key,
        provider,
        session_key,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn insert_run(conn: &mut SqliteConnection, run: &RunStart) -> Result<(), Error> {
    let attempt = attempt_id(conn, &run.item, run.attempt).await?;
    let mode = codec::spawn_mode(run.mode);
    sqlx::query!(
        "INSERT INTO run (attempt_id, spawn, mode, provider, session_key, started_ms)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (attempt_id, spawn) DO NOTHING",
        attempt,
        run.spawn,
        mode,
        run.provider,
        run.session_key,
        run.started_ms,
    )
    .execute(&mut *conn)
    .await?;
    // A transcript ingested before this row is a run session too.
    if !cfg!(mutant_store_run_kind_order) {
        mark_run_session(conn, &run.provider, &run.session_key).await?;
    }
    Ok(())
}

pub(crate) async fn session_child(
    conn: &mut SqliteConnection,
    item: &ItemId,
    attempt: u32,
    pid: u32,
    pid_started_ms: i64,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, item, attempt).await?;
    sqlx::query!(
        "UPDATE attempt SET pid = ?, pid_started_ms = ? WHERE id = ?",
        pid,
        pid_started_ms,
        attempt,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn attempt_guards(
    conn: &mut SqliteConnection,
    item: &ItemId,
    attempt: u32,
    ignored_json: &str,
    git_config_json: &str,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, item, attempt).await?;
    sqlx::query!(
        "UPDATE attempt SET guard_ignored = ?, guard_git_config = ? WHERE id = ?",
        ignored_json,
        git_config_json,
        attempt,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn attempt_refusal(
    conn: &mut SqliteConnection,
    item: &ItemId,
    attempt: u32,
    reasons: &[String],
) -> Result<(), Error> {
    let attempt = attempt_id(conn, item, attempt).await?;
    let json = serde_json::Value::from(reasons).to_string();
    sqlx::query!("UPDATE attempt SET refusal = ? WHERE id = ?", json, attempt,)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn end_run(conn: &mut SqliteConnection, end: &RunEnd) -> Result<(), Error> {
    let attempt = attempt_id(conn, &end.item, end.attempt).await?;
    let (exit, reset) = codec::exit(end.exit);
    let updated = sqlx::query!(
        "UPDATE run SET ended_ms = ?, exit = ?, limit_reset_ms = ?
         WHERE attempt_id = ? AND spawn = ?",
        end.ended_ms,
        exit,
        reset,
        attempt,
        end.spawn,
    )
    .execute(&mut *conn)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(Error::UnknownRow(format!(
            "session {} of attempt {} of {}",
            end.spawn, end.attempt, end.item
        )));
    }
    // A late end of an earlier session of the attempt leaves the current session's child.
    sqlx::query!(
        "UPDATE attempt SET pid = NULL, pid_started_ms = NULL,
                            head_sha = coalesce(?, head_sha)
         WHERE id = ? AND spawn = ?",
        end.head_sha,
        attempt,
        end.spawn,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Records a session's commit range and links each commit of it to the session.
pub(crate) async fn run_commits(
    conn: &mut SqliteConnection,
    range: &RunCommits,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, &range.item, range.attempt).await?;
    let run = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM run WHERE attempt_id = ? AND spawn = ?"#,
        attempt,
        range.spawn,
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| {
        Error::UnknownRow(format!(
            "session {} of attempt {} of {}",
            range.spawn, range.attempt, range.item
        ))
    })?;
    sqlx::query!(
        "UPDATE run SET base_sha = ?, head_sha = ? WHERE id = ?",
        range.base_sha,
        range.head_sha,
        run,
    )
    .execute(&mut *conn)
    .await?;
    for sha in &range.commits {
        sqlx::query!(
            "INSERT INTO run_commit (run_id, repo_id, sha)
             SELECT r.id, i.repo_id, ?
             FROM run r JOIN attempt a ON a.id = r.attempt_id JOIN item i ON i.id = a.item_id
             WHERE r.id = ?
             ON CONFLICT (repo_id, sha) DO NOTHING",
            sha,
            run,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A child process is gone (reaped at start, or found dead): no row names it any more. A
/// verdict run abandoned by a restart keeps no result.
pub(crate) async fn child_gone(
    conn: &mut SqliteConnection,
    pid: u32,
    started_ms: i64,
) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE attempt SET pid = NULL, pid_started_ms = NULL
         WHERE pid = ? AND pid_started_ms = ?",
        pid,
        started_ms,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE verdict_run SET pid = NULL, pid_started_ms = NULL
         WHERE pid = ? AND pid_started_ms = ?",
        pid,
        started_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn start_verdict(
    conn: &mut SqliteConnection,
    start: &VerdictStart,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, &start.item, start.attempt).await?;
    sqlx::query!(
        "INSERT INTO verdict_run (attempt_id, run, command, pid, pid_started_ms, started_ms)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (attempt_id, run) DO UPDATE SET
             pid = excluded.pid,
             pid_started_ms = excluded.pid_started_ms
         WHERE ended_ms IS NULL",
        attempt,
        start.run,
        start.command,
        start.pid,
        start.pid_started_ms,
        start.started_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn end_verdict(
    conn: &mut SqliteConnection,
    end: &VerdictEnd,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, &end.item, end.attempt).await?;
    let result = codec::run_result(end.result);
    let duration = to_db(end.duration_ms, "verdict duration")?;
    let updated = sqlx::query!(
        "UPDATE verdict_run SET
             pid = NULL, pid_started_ms = NULL, ended_ms = ?, result = ?, exit_code = ?,
             duration_ms = ?, verdict_line = ?, tail = ?
         WHERE attempt_id = ? AND run = ?",
        end.ended_ms,
        result,
        end.exit_code,
        duration,
        end.verdict_line,
        end.tail,
        attempt,
        end.run,
    )
    .execute(conn)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(Error::UnknownRow(format!(
            "verdict run {} of attempt {} of {}",
            end.run, end.attempt, end.item
        )));
    }
    Ok(())
}

fn json_list(list: &[String]) -> String {
    serde_json::Value::from(list.to_vec()).to_string()
}

pub(crate) async fn insert_agent_call(
    conn: &mut SqliteConnection,
    call: &AgentCall,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, &call.item, call.attempt).await?;
    let (kind, decisions, followups, blocking) = match &call.kind {
        AgentCallKind::Summary {
            decisions,
            followups,
        } => (
            "summary",
            Some(json_list(decisions)),
            Some(json_list(followups)),
            None,
        ),
        AgentCallKind::Question { blocking } => ("question", None, None, Some(*blocking)),
        AgentCallKind::Note => ("note", None, None, None),
    };
    sqlx::query!(
        "INSERT INTO agent_call (attempt_id, kind, body, decisions_json, followups_json, blocking,
                                 head_sha, refused, ts_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        attempt,
        kind,
        call.body,
        decisions,
        followups,
        blocking,
        call.head_sha,
        call.refused,
        call.at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn upsert_board_comment(
    conn: &mut SqliteConnection,
    item: &ItemId,
    key: &str,
    node_id: &str,
    at_ms: i64,
) -> Result<(), Error> {
    let item = item_id(conn, item).await?;
    sqlx::query!(
        "INSERT INTO board_comment (item_id, comment_key, node_id, created_ms)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (item_id, comment_key) DO UPDATE SET
             node_id = excluded.node_id,
             created_ms = excluded.created_ms",
        item,
        key,
        node_id,
        at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn upsert_item_note(
    conn: &mut SqliteConnection,
    item: &ItemId,
    kind: &str,
    body: &str,
    at_ms: i64,
) -> Result<(), Error> {
    let item = item_id(conn, item).await?;
    sqlx::query!(
        "INSERT INTO item_note (item_id, kind, body, ts_ms) VALUES (?, ?, ?, ?)
         ON CONFLICT (item_id, kind) DO UPDATE SET body = excluded.body, ts_ms = excluded.ts_ms",
        item,
        kind,
        body,
        at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn insert_flaky_tests(
    conn: &mut SqliteConnection,
    item: &ItemId,
    attempt: u32,
    names: &[String],
    at_ms: i64,
) -> Result<(), Error> {
    let attempt = attempt_id(conn, item, attempt).await?;
    for name in names {
        sqlx::query!(
            "INSERT INTO flaky_test (repo_id, test_name, item_id, attempt_id, ts_ms)
             SELECT i.repo_id, ?, i.id, a.id, ?
             FROM attempt a JOIN item i ON i.id = a.item_id WHERE a.id = ?
             ON CONFLICT (attempt_id, test_name) DO NOTHING",
            name,
            at_ms,
            attempt,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(crate) async fn insert_boot(
    conn: &mut SqliteConnection,
    number: u32,
    started_ms: i64,
    pid: u32,
) -> Result<(), Error> {
    sqlx::query!(
        "INSERT INTO boot (number, started_ms, pid) VALUES (?, ?, ?)",
        number,
        started_ms,
        pid,
    )
    .execute(conn)
    .await?;
    Ok(())
}
