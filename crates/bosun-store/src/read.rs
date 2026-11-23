//! The read-only pool and the queries over it. Every derived number is computed here, at query
//! time, from raw token counts and the stored price rows (#3, "The bill").

use std::collections::BTreeMap;

use bosun_core::{
    Cost, Eqv, Message, Origin, PriceRow, PriceTable, Rates, Tokens, ToolCall, Usage, WindowKey,
};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;

use crate::error::{Error, from_db};
use crate::writer::SourceFile;

/// Queries over the read-only pool. Cloning it is cheap; every clone shares the pool.
#[derive(Debug, Clone)]
pub struct Reader {
    pub(crate) pool: SqlitePool,
}

/// The bill of the messages in a time range, priced by the stored table at each message's time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bill {
    /// Messages in the range.
    pub messages: u64,
    /// Raw token sums per category: exact, whatever the prices.
    pub tokens: Tokens,
    /// The cost of the priced messages.
    pub cost: Cost,
    /// `cost` in input-equivalent tokens of the table's reference model.
    pub eqv: Eqv,
    /// Models with no price at the time of some message, with how many messages each. Their
    /// messages are in `tokens` and not in `cost`.
    pub unpriced: BTreeMap<String, u64>,
}

impl Reader {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }

    /// The stored message with key (`provider`, `id`), as the merge left it.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn message(&self, provider: &str, id: &str) -> Result<Option<Message>, Error> {
        let mut conn = self.pool.acquire().await?;
        load_message(&mut conn, provider, id).await
    }

    /// Every transcript file of `provider` and how far it has been parsed, by file key.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn source_files(&self, provider: &str) -> Result<Vec<SourceFile>, Error> {
        let rows = sqlx::query!(
            "SELECT file_key, path, size, parsed_offset, parser_version, last_line_hash
             FROM source_file WHERE provider = ? ORDER BY file_key",
            provider,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(SourceFile {
                    provider: provider.to_owned(),
                    file_key: row.file_key,
                    path: row.path,
                    size: from_db(row.size, "file size")?,
                    parsed_offset: from_db(row.parsed_offset, "parsed offset")?,
                    parser_version: u32::try_from(row.parser_version).map_err(|_| {
                        Error::Corrupt(format!("parser version {}", row.parser_version))
                    })?,
                    last_line_hash: row.last_line_hash,
                })
            })
            .collect()
    }

    /// The price table as stored now.
    ///
    /// # Errors
    ///
    /// A database error, or rows that do not form a valid table.
    pub async fn price_table(&self) -> Result<PriceTable, Error> {
        let mut conn = self.pool.acquire().await?;
        load_price_table(&mut conn).await
    }

    /// The bill of the messages with `from_ms <= ts < to_ms`, each priced by the row valid at
    /// its time.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn bill(&self, from_ms: i64, to_ms: i64) -> Result<Bill, Error> {
        let mut conn = self.pool.acquire().await?;
        // One read transaction, so the prices and the messages are one snapshot.
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let table = load_price_table(&mut tx).await?;
        let rows = sqlx::query!(
            r#"SELECT md.name AS model, m.ts_ms,
                      m.input_tokens, m.cache_write_5m_tokens, m.cache_write_1h_tokens,
                      m.cache_read_tokens, m.output_tokens
               FROM message m JOIN model md ON md.id = m.model_id
               WHERE m.ts_ms >= ? AND m.ts_ms < ?"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut tokens = Tokens::default();
        let mut cost = Cost::ZERO;
        let mut unpriced = BTreeMap::new();
        for row in &rows {
            let t = Tokens {
                input: from_db(row.input_tokens, "input tokens")?,
                cache_write_5m: from_db(row.cache_write_5m_tokens, "cache write tokens")?,
                cache_write_1h: from_db(row.cache_write_1h_tokens, "cache write tokens")?,
                cache_read: from_db(row.cache_read_tokens, "cache read tokens")?,
                output: from_db(row.output_tokens, "output tokens")?,
            };
            tokens.input += t.input;
            tokens.cache_write_5m += t.cache_write_5m;
            tokens.cache_write_1h += t.cache_write_1h;
            tokens.cache_read += t.cache_read;
            tokens.output += t.output;
            match table.cost(&row.model, row.ts_ms, &t) {
                Ok(c) => cost = cost + c,
                Err(unknown) => *unpriced.entry(unknown.model).or_insert(0) += 1,
            }
        }
        Ok(Bill {
            messages: rows.len() as u64,
            tokens,
            cost,
            eqv: table.eqv(cost),
            unpriced,
        })
    }

    /// The bills of the messages with `from_ms <= ts < to_ms`, one per group of `by`, each
    /// message priced by the row valid at its time, with the price table they were priced by.
    /// A group with no message has no entry.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn bills(
        &self,
        from_ms: i64,
        to_ms: i64,
        by: By,
    ) -> Result<(PriceTable, BTreeMap<Group, Bill>), Error> {
        let mut conn = self.pool.acquire().await?;
        // One read transaction, so the prices and the messages are one snapshot.
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let table = load_price_table(&mut tx).await?;
        let rows = sqlx::query!(
            r#"SELECT md.name AS model, m.ts_ms, s.session_key, s.agent_key,
                      r.path AS "repo?: String",
                      m.input_tokens, m.cache_write_5m_tokens, m.cache_write_1h_tokens,
                      m.cache_read_tokens, m.output_tokens
               FROM message m
               JOIN model md ON md.id = m.model_id
               JOIN session s ON s.id = m.session_id
               LEFT JOIN repo r ON r.id = m.repo_id
               WHERE m.ts_ms >= ? AND m.ts_ms < ?"#,
            from_ms,
            to_ms,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut groups: BTreeMap<Group, Fold> = BTreeMap::new();
        for row in rows {
            let tokens = Tokens {
                input: from_db(row.input_tokens, "input tokens")?,
                cache_write_5m: from_db(row.cache_write_5m_tokens, "cache write tokens")?,
                cache_write_1h: from_db(row.cache_write_1h_tokens, "cache write tokens")?,
                cache_read: from_db(row.cache_read_tokens, "cache read tokens")?,
                output: from_db(row.output_tokens, "output tokens")?,
            };
            let group = match by {
                By::Session => Group::Session {
                    session: row.session_key,
                    agent: row.agent_key,
                },
                By::Repo => Group::Repo(row.repo),
                By::Model => Group::Model(row.model.clone()),
            };
            groups
                .entry(group)
                .or_default()
                .add(&table, &row.model, row.ts_ms, tokens);
        }
        let bills = groups
            .into_iter()
            .map(|(group, fold)| (group, fold.finish(&table)))
            .collect();
        Ok((table, bills))
    }
}

/// How [`Reader::bills`] groups messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// Per context window: the window that owns each message.
    Session,
    /// Per repository each message's working directory is attributed to.
    Repo,
    /// Per model.
    Model,
}

/// The key of one group of [`Reader::bills`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// The provider's session id and the sub-agent id (`None` for the main transcript).
    Session {
        session: String,
        agent: Option<String>,
    },
    /// The repository's canonical path key; `None` for messages in no repository.
    Repo(Option<String>),
    /// The model id as transcripts name it.
    Model(String),
}

/// The sums of one bill as its messages are added.
#[derive(Debug, Default)]
struct Fold {
    messages: u64,
    tokens: Tokens,
    cost: Cost,
    unpriced: BTreeMap<String, u64>,
}

impl Fold {
    fn add(&mut self, table: &PriceTable, model: &str, ts_ms: i64, t: Tokens) {
        self.messages += 1;
        self.tokens.input += t.input;
        self.tokens.cache_write_5m += t.cache_write_5m;
        self.tokens.cache_write_1h += t.cache_write_1h;
        self.tokens.cache_read += t.cache_read;
        self.tokens.output += t.output;
        match table.cost(model, ts_ms, &t) {
            Ok(c) => self.cost = self.cost + c,
            Err(unknown) => *self.unpriced.entry(unknown.model).or_insert(0) += 1,
        }
    }

    fn finish(self, table: &PriceTable) -> Bill {
        Bill {
            messages: self.messages,
            tokens: self.tokens,
            cost: self.cost,
            eqv: table.eqv(self.cost),
            unpriced: self.unpriced,
        }
    }
}

/// Reads back a stored message in the shape [`bosun_core::merge`] takes.
pub(crate) async fn load_message(
    conn: &mut SqliteConnection,
    provider: &str,
    id: &str,
) -> Result<Option<Message>, Error> {
    let Some(row) = sqlx::query!(
        r#"SELECT m.id AS "row_id!: i64", m.ts_ms, f.file_key, s.session_key, s.agent_key,
                  md.name AS model, m.cwd,
                  m.input_tokens, m.cache_write_5m_tokens, m.cache_write_1h_tokens,
                  m.cache_read_tokens, m.output_tokens, m.usage_json
           FROM message m
           JOIN source_file f ON f.id = m.source_file_id
           JOIN session s ON s.id = m.session_id
           JOIN model md ON md.id = m.model_id
           WHERE m.provider = ? AND m.message_key = ?"#,
        provider,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    let calls = sqlx::query!(
        "SELECT tool_use_key, name, target, input_bytes, result_bytes, is_error
         FROM tool_call WHERE message_id = ?",
        row.row_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut tool_calls = BTreeMap::new();
    for call in calls {
        tool_calls.insert(
            call.tool_use_key,
            ToolCall {
                name: call.name,
                target: call.target,
                input_bytes: from_db(call.input_bytes, "tool input bytes")?,
                result_bytes: from_db(call.result_bytes, "tool result bytes")?,
                is_error: call.is_error != 0,
            },
        );
    }

    let raw = serde_json::from_str(&row.usage_json)
        .map_err(|e| Error::Corrupt(format!("usage of message {provider}/{id}: {e}")))?;
    Ok(Some(Message {
        provider: provider.to_owned(),
        id: id.to_owned(),
        origin: Origin {
            ts_ms: row.ts_ms,
            file: row.file_key,
            window: WindowKey {
                session_id: row.session_key,
                agent_id: row.agent_key,
            },
        },
        model: row.model,
        cwd: row.cwd,
        usage: Usage {
            tokens: Tokens {
                input: from_db(row.input_tokens, "input tokens")?,
                cache_write_5m: from_db(row.cache_write_5m_tokens, "cache write tokens")?,
                cache_write_1h: from_db(row.cache_write_1h_tokens, "cache write tokens")?,
                cache_read: from_db(row.cache_read_tokens, "cache read tokens")?,
                output: from_db(row.output_tokens, "output tokens")?,
            },
            raw,
        },
        tool_calls,
    }))
}

/// The stored price rows as a [`PriceTable`]. The reference model is the one whose rows carry
/// the reference flag; there must be exactly one.
pub(crate) async fn load_price_table(conn: &mut SqliteConnection) -> Result<PriceTable, Error> {
    let rows = sqlx::query!(
        "SELECT model, valid_from_ms, valid_to_ms, prompt_threshold,
                input, cache_write_5m, cache_write_1h, cache_read, output, is_reference
         FROM price ORDER BY id"
    )
    .fetch_all(conn)
    .await?;
    let mut reference: Option<String> = None;
    let mut table = Vec::with_capacity(rows.len());
    for row in rows {
        if row.is_reference != 0 {
            match &reference {
                Some(model) if *model != row.model => {
                    return Err(Error::Corrupt(format!(
                        "two reference models in the price table: {model} and {}",
                        row.model
                    )));
                }
                _ => reference = Some(row.model.clone()),
            }
        }
        table.push(PriceRow {
            model: row.model,
            valid_from_ms: row.valid_from_ms,
            valid_to_ms: row.valid_to_ms,
            prompt_threshold: row
                .prompt_threshold
                .map(|t| from_db(t, "prompt threshold"))
                .transpose()?,
            rates: Rates {
                input: from_db(row.input, "price")?,
                cache_write_5m: from_db(row.cache_write_5m, "price")?,
                cache_write_1h: from_db(row.cache_write_1h, "price")?,
                cache_read: from_db(row.cache_read, "price")?,
                output: from_db(row.output, "price")?,
            },
        });
    }
    let reference =
        reference.ok_or_else(|| Error::Corrupt("the price table has no reference model".into()))?;
    Ok(PriceTable::new(table, &reference)?)
}

#[cfg(test)]
mod tests {
    use bosun_core::default_table;

    use super::*;
    use crate::writer::tests::{record, source_file, write};
    use crate::{Options, PriceChange, Store, Write};

    /// Every column of every message row, in key order.
    async fn message_rows(store: &Store) -> Vec<String> {
        sqlx::query!(
            r#"SELECT id AS "id!: i64", provider, message_key, session_id, source_file_id, ts_ms,
                      model_id, cwd, repo_id, input_tokens, cache_write_5m_tokens,
                      cache_write_1h_tokens, cache_read_tokens, output_tokens, usage_json,
                      parser_version
               FROM message ORDER BY id"#
        )
        .fetch_all(store.reader().pool())
        .await
        .unwrap()
        .into_iter()
        .map(|r| format!("{r:?}"))
        .collect()
    }

    #[tokio::test]
    async fn the_price_table_is_seeded_from_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        assert_eq!(store.reader().price_table().await.unwrap(), default_table());
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn grouped_bills_partition_the_bill() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let mut elsewhere = record("m3", "f2", 3_000, 30, &[]);
        elsewhere.cwd = "/elsewhere".to_owned();
        elsewhere.model = "claude-sonnet-5".to_owned();
        store
            .writer()
            .write(vec![
                Write::Repo(crate::Repo {
                    path: "/work/repo".to_owned(),
                    remote: None,
                    default_branch: None,
                }),
                source_file("f1"),
                source_file("f2"),
                write(record("m1", "f1", 1_000, 10, &[])),
                write(record("m2", "f1", 2_000, 20, &[])),
                write(elsewhere),
            ])
            .await
            .unwrap();
        let reader = store.reader();
        let whole = reader.bill(0, i64::MAX).await.unwrap();
        for by in [By::Session, By::Repo, By::Model] {
            let (_, bills) = reader.bills(0, i64::MAX, by).await.unwrap();
            assert_eq!(bills.len(), 2, "{by:?}: {bills:?}");
            let messages: u64 = bills.values().map(|b| b.messages).sum();
            let output: u64 = bills.values().map(|b| b.tokens.output).sum();
            let cost: Cost = bills.values().map(|b| b.cost).sum();
            assert_eq!(
                (messages, output, cost),
                (whole.messages, whole.tokens.output, whole.cost),
                "{by:?}"
            );
        }
        let (_, by_session) = reader.bills(0, i64::MAX, By::Session).await.unwrap();
        let f1 = &by_session[&Group::Session {
            session: "session-f1".to_owned(),
            agent: None,
        }];
        assert_eq!((f1.messages, f1.tokens.output), (2, 30));
        let (_, by_repo) = reader.bills(0, i64::MAX, By::Repo).await.unwrap();
        assert_eq!(by_repo[&Group::Repo(None)].tokens.output, 30);
        let (_, in_range) = reader.bills(1_500, 3_000, By::Model).await.unwrap();
        assert_eq!(
            in_range.keys().collect::<Vec<_>>(),
            [&Group::Model("claude-opus-5-5".to_owned())]
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn changing_a_price_changes_the_bill_and_no_message_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let mut unknown = record("m3", "f1", 3_000, 9, &[]);
        unknown.model = "no-such-model".to_owned();
        store
            .writer()
            .write(vec![
                source_file("f1"),
                write(record("m1", "f1", 1_000, 100, &["a"])),
                write(record("m2", "f1", 2_000, 200, &[])),
                write(unknown),
            ])
            .await
            .unwrap();

        let before = store.reader().bill(0, i64::MAX).await.unwrap();
        let rows_before = message_rows(&store).await;
        assert_eq!(before.messages, 3);
        assert_eq!(
            before.unpriced,
            BTreeMap::from([("no-such-model".to_owned(), 1)])
        );

        // Opus 5.5 at twice its price from 1 500 ms on: m1 keeps its price, m2 doubles.
        let opus = default_table()
            .rows()
            .iter()
            .find(|r| r.model == "claude-opus-5-5")
            .unwrap()
            .rates;
        let double = Rates {
            input: opus.input * 2,
            cache_write_5m: opus.cache_write_5m * 2,
            cache_write_1h: opus.cache_write_1h * 2,
            cache_read: opus.cache_read * 2,
            output: opus.output * 2,
        };
        store
            .writer()
            .write(vec![Write::ChangePrice(PriceChange {
                model: "claude-opus-5-5".to_owned(),
                prompt_threshold: None,
                from_ms: 1_500,
                rates: double,
            })])
            .await
            .unwrap();

        let after = store.reader().bill(0, i64::MAX).await.unwrap();
        let m2 = store.reader().bill(2_000, 2_001).await.unwrap();
        let m2_before = opus.cost(&record("m2", "f1", 2_000, 200, &[]).usage.tokens);
        assert_eq!(
            m2.cost,
            m2_before + m2_before,
            "m2 is priced at the new rate"
        );
        assert_eq!(after.cost, before.cost + m2_before, "m1 keeps the old rate");
        assert_eq!(
            after.tokens, before.tokens,
            "raw tokens do not depend on prices"
        );
        assert_eq!(
            message_rows(&store).await,
            rows_before,
            "a message row changed"
        );

        // A change that does not follow the current row is refused, and nothing changes.
        let err = store
            .writer()
            .write(vec![Write::ChangePrice(PriceChange {
                model: "claude-opus-5-5".to_owned(),
                prompt_threshold: None,
                from_ms: 1_000,
                rates: opus,
            })])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PriceChange(_)), "{err}");
        assert_eq!(store.reader().bill(0, i64::MAX).await.unwrap(), after);
        store.close().await.unwrap();
    }
}
