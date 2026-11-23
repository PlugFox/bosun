//! The raw rows the report folds (#6, "The report"; #26). Every number here is a count or a sum
//! of stored integers; pricing and every ratio are computed by the caller (`bosun-heat`).

use std::collections::BTreeMap;

use bosun_core::{PriceTable, Tokens, TouchKind};

use crate::error::{Error, from_db};
use crate::read::{Reader, load_price_table};

/// One stored message, with what the report groups it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub provider: String,
    /// The provider's message id.
    pub message: String,
    /// The provider's session id: the key of the root session the message belongs to.
    pub session: String,
    /// The sub-agent id; `None` for the session's main transcript.
    pub agent: Option<String>,
    /// The kind of the root session (`run` or `interactive`), else of the message's own window
    /// when the root has no row.
    pub kind: String,
    pub model: String,
    pub ts_ms: i64,
    /// The repository's path key; `None` for a message in no repository.
    pub repo: Option<String>,
    pub tokens: Tokens,
}

/// The token sums of one model in one context window over all time, split by whether the
/// messages are inside the provider's window of the root session's check (#46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSums {
    pub provider: String,
    pub session: String,
    /// The sub-agent id; `None` for the main transcript.
    pub agent: Option<String>,
    /// The model id as the transcript names it.
    pub model: String,
    /// Whether these messages are inside the provider's window: `start <= ts <= end`, where a
    /// bound the store does not have (a check stored before #46) leaves that side open.
    pub in_window: bool,
    pub messages: u64,
    pub tokens: Tokens,
}

/// The provider's own totals for one model of a root session (`session_check`), as stored:
/// the last record of the session.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub provider: String,
    pub session: String,
    pub model: String,
    pub input: u64,
    /// Cache writes of both durations: the provider does not split them.
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    /// The provider's cost of the model; `None` when it gave none.
    pub cost_usd: Option<f64>,
}

/// What the store holds of one parser version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionHealth {
    /// Source files whose last parse was by this version.
    pub files: u64,
    /// Of them, files whose lines were counted.
    pub files_counted: u64,
    /// The complete lines of the counted files.
    pub lines: u64,
    /// Gap rows written by this version.
    pub gaps: u64,
}

/// One tool call of a message, with the files it touched (#27: the estimates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    pub provider: String,
    /// The provider's id of the message that made the call.
    pub message: String,
    /// The provider's id of the call.
    pub tool_use: String,
    pub name: String,
    /// The size of the result: its text, and the base64 data of its images.
    pub result_bytes: u64,
    pub is_error: bool,
    /// The files the call touched, sorted: repository-relative paths (absolute outside a
    /// repository) and the kind of each touch.
    pub touches: Vec<(String, TouchKind)>,
}

/// The five stored counts, in the order of the `message` columns (the 5-minute cache write
/// is the short one, the 1-hour one the long), as [`Tokens`].
fn tokens([input, short, long, read, output]: [i64; 5]) -> Result<Tokens, Error> {
    Ok(Tokens {
        input: from_db(input, "input tokens")?,
        cache_write_5m: from_db(short, "cache write tokens")?,
        cache_write_1h: from_db(long, "cache write tokens")?,
        cache_read: from_db(read, "cache read tokens")?,
        output: from_db(output, "output tokens")?,
    })
}

impl Reader {
    /// Every message with `from_ms <= ts < to_ms`, oldest first (then by key), with the price
    /// table of the same snapshot.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn usage(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(PriceTable, Vec<UsageRow>), Error> {
        let mut conn = self.pool.acquire().await?;
        // One read transaction, so the prices and the messages are one snapshot.
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let table = load_price_table(&mut tx).await?;
        let rows = sqlx::query!(
            r#"SELECT m.provider, m.message_key, s.session_key, s.agent_key,
                      coalesce(root.kind, s.kind) AS "kind!: String",
                      md.name AS model, m.ts_ms, r.path AS "repo?: String",
                      m.input_tokens, m.cache_write_5m_tokens, m.cache_write_1h_tokens,
                      m.cache_read_tokens, m.output_tokens
               FROM message m
               JOIN model md ON md.id = m.model_id
               JOIN session s ON s.id = m.session_id
               LEFT JOIN session root ON root.provider = s.provider
                    AND root.session_key = s.session_key AND root.agent_key IS NULL
               LEFT JOIN repo r ON r.id = m.repo_id
               WHERE m.ts_ms >= ? AND m.ts_ms < ?
               ORDER BY m.ts_ms, m.provider, m.message_key"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let rows = rows
            .into_iter()
            .map(|row| {
                Ok(UsageRow {
                    provider: row.provider,
                    message: row.message_key,
                    session: row.session_key,
                    agent: row.agent_key,
                    kind: row.kind,
                    model: row.model,
                    ts_ms: row.ts_ms,
                    repo: row.repo,
                    tokens: tokens([
                        row.input_tokens,
                        row.cache_write_5m_tokens,
                        row.cache_write_1h_tokens,
                        row.cache_read_tokens,
                        row.output_tokens,
                    ])?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok((table, rows))
    }

    /// Every message of the sessions named by (provider, session id), over all time: each
    /// session's main transcript and its sub-agents. Oldest first (then by key), with the price
    /// table of the same snapshot.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn session_usage(
        &self,
        sessions: &[(String, String)],
    ) -> Result<(PriceTable, Vec<UsageRow>), Error> {
        let keys = serde_json::to_string(sessions)
            .map_err(|e| Error::Corrupt(format!("session keys: {e}")))?;
        let mut conn = self.pool.acquire().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let table = load_price_table(&mut tx).await?;
        let rows = sqlx::query!(
            r#"SELECT m.provider, m.message_key, s.session_key, s.agent_key,
                      coalesce(root.kind, s.kind) AS "kind!: String",
                      md.name AS model, m.ts_ms, r.path AS "repo?: String",
                      m.input_tokens, m.cache_write_5m_tokens, m.cache_write_1h_tokens,
                      m.cache_read_tokens, m.output_tokens
               FROM (SELECT DISTINCT json_extract(value, '$[0]') AS provider,
                                     json_extract(value, '$[1]') AS session_key
                     FROM json_each(?)) k
               JOIN session s ON s.provider = k.provider AND s.session_key = k.session_key
               JOIN message m ON m.session_id = s.id
               JOIN model md ON md.id = m.model_id
               LEFT JOIN session root ON root.provider = s.provider
                    AND root.session_key = s.session_key AND root.agent_key IS NULL
               LEFT JOIN repo r ON r.id = m.repo_id
               ORDER BY m.ts_ms, m.provider, m.message_key"#,
            keys,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let rows = rows
            .into_iter()
            .map(|row| {
                Ok(UsageRow {
                    provider: row.provider,
                    message: row.message_key,
                    session: row.session_key,
                    agent: row.agent_key,
                    kind: row.kind,
                    model: row.model,
                    ts_ms: row.ts_ms,
                    repo: row.repo,
                    tokens: tokens([
                        row.input_tokens,
                        row.cache_write_5m_tokens,
                        row.cache_write_1h_tokens,
                        row.cache_read_tokens,
                        row.output_tokens,
                    ])?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok((table, rows))
    }

    /// The provider's totals of every root session that has them, with the token sums over
    /// all time of every context window of those sessions (the main transcript and each
    /// sub-agent) per model and side of the provider's window, in one snapshot. Sorted by key.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn checks(&self) -> Result<(Vec<CheckRow>, Vec<WindowSums>), Error> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let checks = sqlx::query!(
            r#"SELECT s.provider, s.session_key, c.model, c.input_tokens, c.cache_write_tokens,
                      c.cache_read_tokens, c.output_tokens, c.cost_usd
               FROM session_check c JOIN session s ON s.id = c.root_session_id
               ORDER BY s.provider, s.session_key, c.model"#
        )
        .fetch_all(&mut *tx)
        .await?;
        let windows = sqlx::query!(
            r#"SELECT s.provider, s.session_key, s.agent_key, md.name AS model,
                      (w.start_ms IS NULL OR m.ts_ms >= w.start_ms)
                          AND (w.end_ms IS NULL OR m.ts_ms <= w.end_ms) AS "in_window!: bool",
                      count(*) AS "messages!: i64",
                      sum(m.input_tokens) AS "input!: i64",
                      sum(m.cache_write_5m_tokens) AS "cache_write_5m!: i64",
                      sum(m.cache_write_1h_tokens) AS "cache_write_1h!: i64",
                      sum(m.cache_read_tokens) AS "cache_read!: i64",
                      sum(m.output_tokens) AS "output!: i64"
               FROM message m
               JOIN session s ON s.id = m.session_id
               JOIN model md ON md.id = m.model_id
               JOIN session root ON root.provider = s.provider
                    AND root.session_key = s.session_key AND root.agent_key IS NULL
               LEFT JOIN session_check_window w ON w.root_session_id = root.id
               WHERE EXISTS (SELECT 1 FROM session_check c WHERE c.root_session_id = root.id)
               -- Column 5 is the side of the window.
               GROUP BY s.id, md.name, 5
               ORDER BY s.provider, s.session_key, ifnull(s.agent_key, ''), md.name, 5"#
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let checks = checks
            .into_iter()
            .map(|row| {
                Ok(CheckRow {
                    provider: row.provider,
                    session: row.session_key,
                    model: row.model,
                    input: from_db(row.input_tokens, "input tokens")?,
                    cache_write: from_db(row.cache_write_tokens, "cache write tokens")?,
                    cache_read: from_db(row.cache_read_tokens, "cache read tokens")?,
                    output: from_db(row.output_tokens, "output tokens")?,
                    cost_usd: row.cost_usd,
                })
            })
            .collect::<Result<_, Error>>()?;
        let windows = windows
            .into_iter()
            .map(|row| {
                Ok(WindowSums {
                    provider: row.provider,
                    session: row.session_key,
                    agent: row.agent_key,
                    model: row.model,
                    in_window: row.in_window,
                    messages: from_db(row.messages, "message count")?,
                    tokens: tokens([
                        row.input,
                        row.cache_write_5m,
                        row.cache_write_1h,
                        row.cache_read,
                        row.output,
                    ])?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok((checks, windows))
    }

    /// Source files, counted lines and gaps per parser version, over the whole store.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn ingest_health(&self) -> Result<BTreeMap<u32, VersionHealth>, Error> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let files = sqlx::query!(
            r#"SELECT parser_version, count(*) AS "files!: i64",
                      count(parsed_lines) AS "counted!: i64",
                      ifnull(sum(parsed_lines), 0) AS "lines!: i64"
               FROM source_file GROUP BY parser_version"#
        )
        .fetch_all(&mut *tx)
        .await?;
        let gaps = sqlx::query!(
            r#"SELECT parser_version, count(*) AS "gaps!: i64"
               FROM ingest_gap GROUP BY parser_version"#
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let version =
            |v: i64| u32::try_from(v).map_err(|_| Error::Corrupt(format!("parser version {v}")));
        let mut out: BTreeMap<u32, VersionHealth> = BTreeMap::new();
        for row in files {
            let health = out.entry(version(row.parser_version)?).or_default();
            health.files = from_db(row.files, "file count")?;
            health.files_counted = from_db(row.counted, "file count")?;
            health.lines = from_db(row.lines, "line count")?;
        }
        for row in gaps {
            out.entry(version(row.parser_version)?).or_default().gaps =
                from_db(row.gaps, "gap count")?;
        }
        Ok(out)
    }

    /// Every tool call of the messages with `from_ms <= ts < to_ms`, with its file touches,
    /// sorted by message (time, then key), then call id.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn tool_calls(&self, from_ms: i64, to_ms: i64) -> Result<Vec<ToolRow>, Error> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let calls = sqlx::query!(
            r#"SELECT t.id AS "id!: i64", m.provider, m.message_key, t.tool_use_key, t.name,
                      t.result_bytes, t.is_error
               FROM tool_call t JOIN message m ON m.id = t.message_id
               WHERE m.ts_ms >= ? AND m.ts_ms < ?
               ORDER BY m.ts_ms, m.provider, m.message_key, t.tool_use_key"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        let touches = sqlx::query!(
            r#"SELECT f.tool_call_id, f.path, f.kind
               FROM file_touch f
               JOIN tool_call t ON t.id = f.tool_call_id
               JOIN message m ON m.id = t.message_id
               WHERE m.ts_ms >= ? AND m.ts_ms < ?
               ORDER BY f.tool_call_id, f.path, f.kind"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut by_call: BTreeMap<i64, Vec<(String, TouchKind)>> = BTreeMap::new();
        for row in touches {
            let kind = match row.kind.as_str() {
                "read" => TouchKind::Read,
                "write" => TouchKind::Write,
                "search" => TouchKind::Search,
                "exec" => TouchKind::Exec,
                "unknown" => TouchKind::Unknown,
                other => return Err(Error::Corrupt(format!("file touch kind {other:?}"))),
            };
            by_call
                .entry(row.tool_call_id)
                .or_default()
                .push((row.path, kind));
        }
        calls
            .into_iter()
            .map(|row| {
                Ok(ToolRow {
                    provider: row.provider,
                    message: row.message_key,
                    tool_use: row.tool_use_key,
                    name: row.name,
                    result_bytes: from_db(row.result_bytes, "result bytes")?,
                    is_error: row.is_error != 0,
                    touches: by_call.remove(&row.id).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The number of stored commits per repository path key with `from_ms <= ts < to_ms`.
    /// A repository with none in the range has no entry.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn commit_counts(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<BTreeMap<String, u64>, Error> {
        let rows = sqlx::query!(
            r#"SELECT r.path, count(*) AS "commits!: i64"
               FROM "commit" c JOIN repo r ON r.id = c.repo_id
               WHERE c.ts_ms >= ? AND c.ts_ms < ?
               GROUP BY r.path"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok((row.path, from_db(row.commits, "commit count")?)))
            .collect()
    }
}
