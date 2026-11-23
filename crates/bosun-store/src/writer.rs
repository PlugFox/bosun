//! The writer task: the only write connection, fed batches through a channel (#3).
//!
//! A batch is the caller's unit of atomicity: the writer applies it in one transaction, so it
//! is in the database whole or not at all. Ingest sends the rows parsed from one read of a file
//! together with that file's new offset, so a crash never leaves rows without their offset or
//! an offset without its rows.

use std::time::Duration;

use bosun_core::{Gap, Message, Rates, WindowKey, default_table, merge};
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Executor};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::error::{Error, to_db};
use crate::read;

/// How many batches may wait for the writer before senders wait too.
const QUEUE: usize = 64;

/// One change to the database. A batch of them is applied in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Registers a transcript file or records how far it has been parsed.
    SourceFile(SourceFile),
    /// A record of a message, merged with what is stored by [`bosun_core::merge`]. Its source
    /// file must have been registered by an earlier write.
    Message {
        message: Message,
        parser_version: u32,
    },
    /// A line of `file_key` that could not be read as expected. Keyed by (file, offset): a
    /// second gap at the same offset replaces the first.
    Gap {
        provider: String,
        file_key: String,
        gap: Gap,
        parser_version: u32,
    },
    /// A new price for a model from a time on. Closes the current row; never touches a message.
    ChangePrice(PriceChange),
}

/// A transcript file and how far it has been parsed. Keyed by (provider, file key): writing it
/// again updates the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub provider: String,
    /// The file id, as in [`bosun_core::Origin::file`].
    pub file_key: String,
    pub path: String,
    pub size: u64,
    pub parsed_offset: u64,
    pub parser_version: u32,
    pub last_line_hash: Option<String>,
}

/// New rates for `model` (and `prompt_threshold`) valid from `from_ms` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceChange {
    pub model: String,
    pub prompt_threshold: Option<u64>,
    /// Must be after the start of the current row.
    pub from_ms: i64,
    pub rates: Rates,
}

#[derive(Debug)]
enum Command {
    Batch(Vec<Write>, oneshot::Sender<Result<(), Error>>),
    Shutdown(oneshot::Sender<Result<(), Error>>),
}

/// A handle to the writer task. Cloning it is cheap; every clone feeds the same task.
#[derive(Debug, Clone)]
pub struct Writer {
    commands: mpsc::Sender<Command>,
}

impl Writer {
    pub(crate) fn spawn(conn: SqliteConnection, checkpoint: Duration) -> (Self, JoinHandle<()>) {
        let (commands, inbox) = mpsc::channel(QUEUE);
        let task = tokio::spawn(run(conn, inbox, checkpoint));
        (Self { commands }, task)
    }

    /// Applies `batch` in one transaction and waits for the commit.
    ///
    /// # Errors
    ///
    /// The batch failed and was rolled back as a whole, or the store is closed.
    pub async fn write(&self, batch: Vec<Write>) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Batch(batch, reply))
            .await
            .map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Shutdown(reply))
            .await
            .map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)?
    }
}

async fn run(mut conn: SqliteConnection, mut inbox: mpsc::Receiver<Command>, every: Duration) {
    let mut timer = tokio::time::interval_at(Instant::now() + every, every);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Batch(batch, reply)) => {
                    let _ = reply.send(apply_batch(&mut conn, batch).await);
                }
                Some(Command::Shutdown(reply)) => {
                    let _ = reply.send(close(conn).await);
                    return;
                }
                // Every handle is gone: stop as if shut down; nobody waits for the result.
                None => {
                    let _ = close(conn).await;
                    return;
                }
            },
            _ = timer.tick() => {
                // A failed periodic checkpoint is retried on the next tick, and SQLite's own
                // automatic checkpoint still bounds the WAL meanwhile.
                let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)").await;
            }
        }
    }
}

async fn close(mut conn: SqliteConnection) -> Result<(), Error> {
    let checkpoint = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").await;
    conn.close().await?;
    checkpoint?;
    Ok(())
}

async fn apply_batch(conn: &mut SqliteConnection, batch: Vec<Write>) -> Result<(), Error> {
    if cfg!(mutant_store_commit_per_row) {
        for (row, write) in batch.into_iter().enumerate() {
            let mut tx = conn.begin().await?;
            apply(&mut tx, write).await?;
            tx.commit().await?;
            failpoint(row + 1);
        }
        return Ok(());
    }
    let mut tx = conn.begin().await?;
    for (row, write) in batch.into_iter().enumerate() {
        apply(&mut tx, write).await?;
        failpoint(row + 1);
    }
    tx.commit().await?;
    Ok(())
}

/// In tests, aborts the process once the given row of a batch has been applied (see the abort
/// test below). Compiled out of every other build.
#[cfg(test)]
pub(crate) static ABORT_AFTER_ROW: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn failpoint(row: usize) {
    if ABORT_AFTER_ROW.load(std::sync::atomic::Ordering::SeqCst) == row {
        eprintln!("failpoint: abort after row {row}");
        std::process::abort();
    }
}

#[cfg(not(test))]
fn failpoint(_row: usize) {}

async fn apply(conn: &mut SqliteConnection, write: Write) -> Result<(), Error> {
    match write {
        Write::SourceFile(file) => upsert_source_file(conn, &file).await,
        Write::Message {
            message,
            parser_version,
        } => upsert_message(conn, message, parser_version).await,
        Write::Gap {
            provider,
            file_key,
            gap,
            parser_version,
        } => upsert_gap(conn, &provider, &file_key, &gap, parser_version).await,
        Write::ChangePrice(change) => change_price(conn, &change).await,
    }
}

async fn upsert_source_file(conn: &mut SqliteConnection, file: &SourceFile) -> Result<(), Error> {
    let size = to_db(file.size, "file size")?;
    let offset = to_db(file.parsed_offset, "parsed offset")?;
    sqlx::query!(
        "INSERT INTO source_file
             (provider, file_key, path, size, parsed_offset, parser_version, last_line_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (provider, file_key) DO UPDATE SET
             path = excluded.path,
             size = excluded.size,
             parsed_offset = excluded.parsed_offset,
             parser_version = excluded.parser_version,
             last_line_hash = excluded.last_line_hash",
        file.provider,
        file.file_key,
        file.path,
        size,
        offset,
        file.parser_version,
        file.last_line_hash,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn source_file_id(
    conn: &mut SqliteConnection,
    provider: &str,
    file_key: &str,
) -> Result<i64, Error> {
    sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM source_file WHERE provider = ? AND file_key = ?"#,
        provider,
        file_key
    )
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| Error::UnknownSourceFile {
        provider: provider.to_owned(),
        file_key: file_key.to_owned(),
    })
}

async fn session_id(
    conn: &mut SqliteConnection,
    provider: &str,
    window: &WindowKey,
) -> Result<i64, Error> {
    let found = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM session
         WHERE provider = ? AND session_key = ? AND ifnull(agent_key, '') = ifnull(?, '')"#,
        provider,
        window.session_id,
        window.agent_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = found {
        return Ok(id);
    }
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO session (provider, session_key, agent_key) VALUES (?, ?, ?)
           RETURNING id AS "id!: i64""#,
        provider,
        window.session_id,
        window.agent_id,
    )
    .fetch_one(conn)
    .await?)
}

async fn model_id(conn: &mut SqliteConnection, provider: &str, name: &str) -> Result<i64, Error> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO model (provider, name) VALUES (?, ?)
           ON CONFLICT (provider, name) DO UPDATE SET name = excluded.name
           RETURNING id AS "id!: i64""#,
        provider,
        name,
    )
    .fetch_one(conn)
    .await?)
}

/// Read-modify-write: the stored message (if any) and the incoming record are folded by
/// [`bosun_core::merge`], and the result replaces the row and upserts every tool call.
async fn upsert_message(
    conn: &mut SqliteConnection,
    incoming: Message,
    parser_version: u32,
) -> Result<(), Error> {
    let merged = match read::load_message(conn, &incoming.provider, &incoming.id).await? {
        Some(existing) => merge(existing, incoming)?,
        None => incoming,
    };
    let m = &merged;
    let session = session_id(conn, &m.provider, &m.origin.window).await?;
    let file = source_file_id(conn, &m.provider, &m.origin.file).await?;
    let model = model_id(conn, &m.provider, &m.model).await?;
    let t = &m.usage.tokens;
    let input = to_db(t.input, "input tokens")?;
    let five_minute = to_db(t.cache_write_5m, "cache write tokens")?;
    let one_hour = to_db(t.cache_write_1h, "cache write tokens")?;
    let read = to_db(t.cache_read, "cache read tokens")?;
    let output = to_db(t.output, "output tokens")?;
    let usage = m.usage.raw.to_string();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO message (
               provider, message_key, session_id, source_file_id, ts_ms, model_id, cwd,
               input_tokens, cache_write_5m_tokens, cache_write_1h_tokens, cache_read_tokens,
               output_tokens, usage_json, parser_version)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (provider, message_key) DO UPDATE SET
               session_id = excluded.session_id,
               source_file_id = excluded.source_file_id,
               ts_ms = excluded.ts_ms,
               model_id = excluded.model_id,
               cwd = excluded.cwd,
               input_tokens = excluded.input_tokens,
               cache_write_5m_tokens = excluded.cache_write_5m_tokens,
               cache_write_1h_tokens = excluded.cache_write_1h_tokens,
               cache_read_tokens = excluded.cache_read_tokens,
               output_tokens = excluded.output_tokens,
               usage_json = excluded.usage_json,
               parser_version = excluded.parser_version
           RETURNING id AS "id!: i64""#,
        m.provider,
        m.id,
        session,
        file,
        m.origin.ts_ms,
        model,
        m.cwd,
        input,
        five_minute,
        one_hour,
        read,
        output,
        usage,
        parser_version,
    )
    .fetch_one(&mut *conn)
    .await?;

    for (key, call) in &m.tool_calls {
        let input_bytes = to_db(call.input_bytes, "tool input bytes")?;
        let result_bytes = to_db(call.result_bytes, "tool result bytes")?;
        sqlx::query!(
            "INSERT INTO tool_call
                 (message_id, tool_use_key, name, target, input_bytes, result_bytes, is_error)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (message_id, tool_use_key) DO UPDATE SET
                 name = excluded.name,
                 target = excluded.target,
                 input_bytes = excluded.input_bytes,
                 result_bytes = excluded.result_bytes,
                 is_error = excluded.is_error",
            id,
            key,
            call.name,
            call.target,
            input_bytes,
            result_bytes,
            call.is_error,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn upsert_gap(
    conn: &mut SqliteConnection,
    provider: &str,
    file_key: &str,
    gap: &Gap,
    parser_version: u32,
) -> Result<(), Error> {
    let file = source_file_id(conn, provider, file_key).await?;
    let offset = to_db(gap.offset, "gap offset")?;
    let length = to_db(gap.length, "gap length")?;
    let keys = serde_json::Value::from(gap.top_level_keys.clone()).to_string();
    sqlx::query!(
        "INSERT INTO ingest_gap (source_file_id, byte_offset, length, record_type,
                                 top_level_keys_json, reason, parser_version)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (source_file_id, byte_offset) DO UPDATE SET
             length = excluded.length,
             record_type = excluded.record_type,
             top_level_keys_json = excluded.top_level_keys_json,
             reason = excluded.reason,
             parser_version = excluded.parser_version",
        file,
        offset,
        length,
        gap.record_type,
        keys,
        gap.reason,
        parser_version,
    )
    .execute(conn)
    .await?;
    Ok(())
}

struct RateColumns {
    input: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    cache_read: i64,
    output: i64,
}

fn rate_columns(rates: &Rates) -> Result<RateColumns, Error> {
    Ok(RateColumns {
        input: to_db(rates.input, "price")?,
        cache_write_5m: to_db(rates.cache_write_5m, "price")?,
        cache_write_1h: to_db(rates.cache_write_1h, "price")?,
        cache_read: to_db(rates.cache_read, "price")?,
        output: to_db(rates.output, "price")?,
    })
}

async fn change_price(conn: &mut SqliteConnection, change: &PriceChange) -> Result<(), Error> {
    let threshold = change
        .prompt_threshold
        .map(|t| to_db(t, "prompt threshold"))
        .transpose()?;
    let current = sqlx::query!(
        r#"SELECT id AS "id!: i64", valid_from_ms, is_reference FROM price
           WHERE model = ? AND prompt_threshold IS ? AND valid_to_ms IS NULL"#,
        change.model,
        threshold,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let is_reference = match current {
        Some(row) if row.valid_from_ms >= change.from_ms => {
            return Err(Error::PriceChange(format!(
                "the current row of {} starts at {} ms, not before {} ms",
                change.model, row.valid_from_ms, change.from_ms
            )));
        }
        Some(row) => {
            sqlx::query!(
                "UPDATE price SET valid_to_ms = ? WHERE id = ?",
                change.from_ms,
                row.id
            )
            .execute(&mut *conn)
            .await?;
            row.is_reference
        }
        None => 0,
    };
    let version =
        sqlx::query_scalar!(r#"SELECT ifnull(max(version), 0) + 1 AS "v!: i64" FROM price"#)
            .fetch_one(&mut *conn)
            .await?;
    let r = rate_columns(&change.rates)?;
    sqlx::query!(
        "INSERT INTO price (version, model, valid_from_ms, valid_to_ms, prompt_threshold,
                            input, cache_write_5m, cache_write_1h, cache_read, output,
                            is_reference)
         VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?)",
        version,
        change.model,
        change.from_ms,
        threshold,
        r.input,
        r.cache_write_5m,
        r.cache_write_1h,
        r.cache_read,
        r.output,
        is_reference,
    )
    .execute(&mut *conn)
    .await?;
    // The rows must still form a valid table; otherwise the batch rolls back.
    read::load_price_table(conn).await?;
    Ok(())
}

/// Fills an empty price table with [`bosun_core::default_table`] as version 1. A table that
/// has rows is left alone: a price set by the user is never overwritten.
pub(crate) async fn seed_prices(conn: &mut SqliteConnection) -> Result<(), Error> {
    let rows = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM price"#)
        .fetch_one(&mut *conn)
        .await?;
    if rows > 0 {
        return Ok(());
    }
    let table = default_table();
    let mut tx = conn.begin().await?;
    for row in table.rows() {
        let threshold = row
            .prompt_threshold
            .map(|t| to_db(t, "prompt threshold"))
            .transpose()?;
        let r = rate_columns(&row.rates)?;
        let is_reference = row.model == table.reference_model();
        sqlx::query!(
            "INSERT INTO price (version, model, valid_from_ms, valid_to_ms, prompt_threshold,
                                input, cache_write_5m, cache_write_1h, cache_read, output,
                                is_reference)
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            row.model,
            row.valid_from_ms,
            row.valid_to_ms,
            threshold,
            r.input,
            r.cache_write_5m,
            r.cache_write_1h,
            r.cache_read,
            r.output,
            is_reference,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use bosun_core::{Origin, Tokens, ToolCall, Usage};

    use super::*;
    use crate::{Options, Store};

    pub(crate) const PROVIDER: &str = "claude";

    pub(crate) fn source_file(key: &str) -> Write {
        Write::SourceFile(SourceFile {
            provider: PROVIDER.to_owned(),
            file_key: key.to_owned(),
            path: format!("projects/p/{key}.jsonl"),
            size: 100,
            parsed_offset: 100,
            parser_version: 1,
            last_line_hash: None,
        })
    }

    /// A record of message `id` in file `file`, with tool calls named by their ids.
    pub(crate) fn record(id: &str, file: &str, ts_ms: i64, output: u64, tools: &[&str]) -> Message {
        Message {
            provider: PROVIDER.to_owned(),
            id: id.to_owned(),
            origin: Origin {
                ts_ms,
                file: file.to_owned(),
                window: WindowKey {
                    session_id: format!("session-{file}"),
                    agent_id: None,
                },
            },
            model: "claude-opus-5-5".to_owned(),
            cwd: "/work/repo".to_owned(),
            usage: Usage {
                tokens: Tokens {
                    input: 10,
                    cache_write_5m: 1_000,
                    cache_write_1h: 2_000,
                    cache_read: 50_000,
                    output,
                },
                raw: serde_json::json!({ "output_tokens": output, "service_tier": "standard" }),
            },
            tool_calls: tools
                .iter()
                .map(|t| {
                    let call = ToolCall {
                        name: "Read".to_owned(),
                        target: Some(format!("src/{t}.rs")),
                        input_bytes: 40,
                        ..ToolCall::default()
                    };
                    ((*t).to_owned(), call)
                })
                .collect(),
        }
    }

    pub(crate) fn write(message: Message) -> Write {
        Write::Message {
            message,
            parser_version: 1,
        }
    }

    async fn count(store: &Store, sql_like: &str) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!: i64" FROM message WHERE message_key LIKE ?"#,
            sql_like
        )
        .fetch_one(store.reader().pool())
        .await
        .unwrap()
    }

    const ABORT_TEST: &str = "writer::tests::an_abort_mid_batch_leaves_the_whole_batch_or_none";
    const ABORT_CHILD: &str = "BOSUN_STORE_ABORT_CHILD_DB";
    const BATCH: usize = 10;
    const ABORT_AFTER: usize = 4;

    /// The child half: commits one batch, then aborts after row `ABORT_AFTER` of the next.
    async fn abort_child(db: &Path) {
        let store = Store::open(db, Options::default()).await.unwrap();
        let first: Vec<Write> = std::iter::once(source_file("f1"))
            .chain((0..BATCH).map(|i| write(record(&format!("first-{i}"), "f1", 1, 5, &[]))))
            .collect();
        store.writer().write(first).await.unwrap();

        ABORT_AFTER_ROW.store(ABORT_AFTER, Ordering::SeqCst);
        let second: Vec<Write> = (0..BATCH)
            .map(|i| write(record(&format!("second-{i}"), "f1", 2, 5, &["t"])))
            .collect();
        let _ = store.writer().write(second).await;
        // Reaching this line means the failpoint did not fire; the parent sees a clean exit.
        eprintln!("failpoint not reached");
    }

    #[tokio::test]
    async fn an_abort_mid_batch_leaves_the_whole_batch_or_none() {
        if let Some(db) = std::env::var_os(ABORT_CHILD) {
            abort_child(Path::new(&db)).await;
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bosun.db");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([ABORT_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(ABORT_CHILD, &db)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(!child.status.success(), "the child did not abort: {stderr}");
        assert!(
            stderr.contains(&format!("failpoint: abort after row {ABORT_AFTER}")),
            "the child failed before the failpoint: {stderr}"
        );

        let store = Store::open(&db, Options::default()).await.unwrap();
        let batch = i64::try_from(BATCH).unwrap();
        assert_eq!(count(&store, "first-%").await, batch, "the committed batch");
        let second = count(&store, "second-%").await;
        assert!(
            second == 0 || second == batch,
            "the aborted batch is partial: {second} of {BATCH} rows"
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn records_of_one_message_merge_into_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        // A split message: three records, the last has the final output; the same message
        // also appears later in a second file (a continued session).
        let batch = vec![
            source_file("f1"),
            source_file("f2"),
            write(record("m", "f2", 900, 120, &["c"])),
            write(record("m", "f1", 100, 3, &["a"])),
            write(record("m", "f1", 100, 80, &["b"])),
        ];
        store.writer().write(batch.clone()).await.unwrap();
        // Ingesting the same records again changes nothing.
        store.writer().write(batch).await.unwrap();

        let stored = store
            .reader()
            .message(PROVIDER, "m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.usage.tokens.output, 120);
        assert_eq!(
            stored.origin.file, "f1",
            "the earliest file owns the message"
        );
        assert_eq!(stored.origin.ts_ms, 100);
        let ids: Vec<&str> = stored.tool_calls.keys().map(String::as_str).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(count(&store, "%").await, 1);
        let calls = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM tool_call"#)
            .fetch_one(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(calls, 3);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_rolls_back_its_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let batch = vec![
            source_file("f1"),
            write(record("ok", "f1", 1, 1, &[])),
            // No source file registered for f9.
            write(record("bad", "f9", 1, 1, &[])),
        ];
        let err = store.writer().write(batch).await.unwrap_err();
        assert!(matches!(err, Error::UnknownSourceFile { .. }), "{err}");
        assert_eq!(count(&store, "%").await, 0);
        // The writer keeps serving after a failed batch.
        store
            .writer()
            .write(vec![
                source_file("f1"),
                write(record("ok", "f1", 1, 1, &[])),
            ])
            .await
            .unwrap();
        assert_eq!(count(&store, "%").await, 1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn gaps_are_keyed_by_file_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let gap = |reason: &str| Write::Gap {
            provider: PROVIDER.to_owned(),
            file_key: "f1".to_owned(),
            gap: Gap {
                offset: 42,
                length: 7,
                record_type: Some("assistant".to_owned()),
                top_level_keys: vec!["type".to_owned(), "message".to_owned()],
                reason: reason.to_owned(),
            },
            parser_version: 1,
        };
        store
            .writer()
            .write(vec![source_file("f1"), gap("first"), gap("second")])
            .await
            .unwrap();
        let rows = sqlx::query!("SELECT reason, top_level_keys_json FROM ingest_gap")
            .fetch_all(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "second");
        assert_eq!(rows[0].top_level_keys_json, r#"["type","message"]"#);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_timer_checkpoints_the_wal_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bosun.db");
        let options = Options {
            checkpoint_interval: Duration::from_millis(200),
            ..Options::default()
        };
        let store = Store::open(&db, options).await.unwrap();
        store
            .writer()
            .write(vec![source_file("f1"), write(record("m", "f1", 1, 1, &[]))])
            .await
            .unwrap();
        // `immutable=1` reads the database file alone, ignoring the WAL.
        let in_file = || async {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db)
                .read_only(true)
                .immutable(true);
            let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
            let n = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM message"#)
                .fetch_one(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            n
        };
        assert_eq!(in_file().await, 0, "the commit is still in the WAL only");
        let deadline = Instant::now() + Duration::from_secs(10);
        while in_file().await == 0 {
            assert!(Instant::now() < deadline, "no checkpoint within 10 s");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        store.close().await.unwrap();
    }
}
