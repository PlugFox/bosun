//! The identity's notification channels (#9, the decision on channels; migration 20) and its
//! notification outbox (#9, "Delivery through the outbox"; migration 22).
//!
//! A channel row holds its kind, its non-secret parameters, the events it gets and its minimum
//! gap, and the name of the credential-store entry its secret lives in
//! ([`NewChannel::secret_entry`]), never the secret. [`Write::ChannelAdded`] refuses a name a
//! live channel has; [`Write::ChannelRemoved`] keeps the row, marked removed, frees the name,
//! and drops its pending notices.
//!
//! A notice ([`NewNotice`], [`Write::NoticeQueued`]) is fanned out, in the transaction that
//! queues it, to one row per live channel whose events have it; the pair of the channel and
//! the notice's key is unique, so a replayed event queues nothing more. A transition into
//! Needs you queues its notice itself (from `work::record_transition`), so every path that
//! sends an item there notifies. The daemon's drainer reads a channel's pending rows
//! ([`Reader::pending_notices`]) and marks them sent, failed or dropped.
//!
//! [`Write::ChannelAdded`]: crate::Write::ChannelAdded
//! [`Write::ChannelRemoved`]: crate::Write::ChannelRemoved
//! [`Write::NoticeQueued`]: crate::Write::NoticeQueued

use std::collections::BTreeMap;

use sqlx::sqlite::SqliteConnection;

use crate::error::{Error, from_db, to_db};
use crate::read::Reader;

/// A notice to queue ([`crate::Write::NoticeQueued`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotice {
    /// The event's dedup key: a second notice with it queues nothing.
    pub key: String,
    /// `needs_you`, `repo_paused` or `ready_drained`.
    pub event: String,
    /// The repository's configuration name.
    pub repo: String,
    pub item: Option<String>,
    /// The numbers and words its text needs, built by the daemon, never agent text.
    pub facts: BTreeMap<String, String>,
    pub at_ms: i64,
}

/// A pending notice of one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRow {
    pub id: i64,
    pub key: String,
    pub event: String,
    pub repo: String,
    pub item: Option<String>,
    pub facts: BTreeMap<String, String>,
    pub created_ms: i64,
    pub tries: u32,
    pub last_error: Option<String>,
    pub retry_at_ms: Option<i64>,
}

/// A channel to add ([`crate::Write::ChannelAdded`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChannel {
    /// `[a-z0-9-]{1,32}`, unique among the live channels.
    pub name: String,
    pub kind: String,
    /// The kind's non-secret parameters.
    pub params: BTreeMap<String, String>,
    /// The event names it gets.
    pub events: Vec<String>,
    pub min_gap_ms: u64,
    /// The credential-store entry of its secret: a name, never the secret.
    pub secret_entry: String,
    pub at_ms: i64,
}

/// A live channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    pub name: String,
    pub kind: String,
    pub params: BTreeMap<String, String>,
    pub events: Vec<String>,
    pub min_gap_ms: u64,
    pub secret_entry: String,
    pub last_sent_ms: Option<i64>,
    pub created_ms: i64,
    pub changed_ms: i64,
}

/// Whether `name` is a channel name: `[a-z0-9-]{1,32}`.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) async fn add(conn: &mut SqliteConnection, new: &NewChannel) -> Result<(), Error> {
    if !valid_name(&new.name) {
        return Err(Error::Refused(format!(
            "{:?} is not a channel name: 1 to 32 of a-z, 0-9 and -",
            new.name
        )));
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!: i64" FROM notify_channel
           WHERE name = ? AND removed_ms IS NULL"#,
        new.name,
    )
    .fetch_one(&mut *conn)
    .await?;
    if taken > 0 {
        return Err(Error::Refused(format!(
            "a channel named {} exists: remove it first",
            new.name
        )));
    }
    let params = serde_json::to_string(&new.params)
        .map_err(|e| Error::Corrupt(format!("channel params: {e}")))?;
    let events = serde_json::to_string(&new.events)
        .map_err(|e| Error::Corrupt(format!("channel events: {e}")))?;
    let min_gap_ms = to_db(new.min_gap_ms, "the minimum gap")?;
    sqlx::query!(
        "INSERT INTO notify_channel (name, kind, params, events, min_gap_ms, secret_entry,
                                     created_ms, changed_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        new.name,
        new.kind,
        params,
        events,
        min_gap_ms,
        new.secret_entry,
        new.at_ms,
        new.at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn remove(
    conn: &mut SqliteConnection,
    name: &str,
    at_ms: i64,
) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE notify_outbox SET dropped_ms = ?, last_error = 'the channel was removed'
         WHERE sent_ms IS NULL AND dropped_ms IS NULL AND channel_id IN (
             SELECT id FROM notify_channel WHERE name = ? AND removed_ms IS NULL)",
        at_ms,
        name,
    )
    .execute(&mut *conn)
    .await?;
    let removed = sqlx::query!(
        "UPDATE notify_channel SET removed_ms = ?, changed_ms = ?
         WHERE name = ? AND removed_ms IS NULL",
        at_ms,
        at_ms,
        name,
    )
    .execute(conn)
    .await?
    .rows_affected();
    if removed == 0 {
        return Err(Error::UnknownRow(format!("notification channel {name}")));
    }
    Ok(())
}

/// A process-wide count that makes every key of `mutant_notify_no_dedup_key` fresh.
static FRESH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Queues `notice`: one row per live channel whose events have it, none for a key a channel
/// has already.
pub(crate) async fn queue(conn: &mut SqliteConnection, notice: &NewNotice) -> Result<(), Error> {
    let facts = serde_json::to_string(&notice.facts)
        .map_err(|e| Error::Corrupt(format!("notice facts: {e}")))?;
    // Mutant (`mutant_notify_no_dedup_key`): every row gets a fresh key, so the unique index
    // never matches a replay.
    let key = if cfg!(mutant_notify_no_dedup_key) {
        let n = FRESH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{}#{}-{n}", notice.key, std::process::id())
    } else {
        notice.key.clone()
    };
    // Mutant (`mutant_store_notify_every_channel`): the fan-out ignores the channels' events
    // and their removal.
    if cfg!(mutant_store_notify_every_channel) {
        sqlx::query!(
            "INSERT OR IGNORE INTO notify_outbox (channel_id, key, event, repo, item, facts,
                                                  created_ms)
             SELECT id, ?, ?, ?, ?, ?, ? FROM notify_channel",
            key,
            notice.event,
            notice.repo,
            notice.item,
            facts,
            notice.at_ms,
        )
        .execute(conn)
        .await?;
    } else {
        sqlx::query!(
            "INSERT OR IGNORE INTO notify_outbox (channel_id, key, event, repo, item, facts,
                                                  created_ms)
             SELECT c.id, ?, ?, ?, ?, ?, ? FROM notify_channel c
             WHERE c.removed_ms IS NULL
               AND EXISTS (SELECT 1 FROM json_each(c.events) e WHERE e.value = ?)",
            key,
            notice.event,
            notice.repo,
            notice.item,
            facts,
            notice.at_ms,
            notice.event,
        )
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// Queues the notice of a transition into Needs you: key `needs_you:<transition key>`, facts
/// the reason (the item machine's word) and the attempt. `work::record_transition` calls it in
/// the transition's transaction.
pub(crate) async fn queue_needs_you(
    conn: &mut SqliteConnection,
    item_row: i64,
    transition: &bosun_core::Transition,
    reason: &str,
    at_ms: i64,
) -> Result<(), Error> {
    let repo = sqlx::query_scalar!(
        r#"SELECT coalesce(r.name, r.path) AS "repo!: String"
           FROM item i JOIN repo r ON r.id = i.repo_id WHERE i.id = ?"#,
        item_row,
    )
    .fetch_one(&mut *conn)
    .await?;
    let notice = NewNotice {
        key: format!("needs_you:{}", transition.key),
        event: "needs_you".to_owned(),
        repo,
        item: Some(transition.key.item.to_string()),
        facts: BTreeMap::from([
            ("reason".to_owned(), reason.to_owned()),
            ("attempt".to_owned(), transition.attempt.to_string()),
        ]),
        at_ms,
    };
    queue(conn, &notice).await
}

/// Marks `ids` sent at `at_ms`, and their channels as having sent then.
pub(crate) async fn sent(
    conn: &mut SqliteConnection,
    ids: &[i64],
    at_ms: i64,
) -> Result<(), Error> {
    for id in ids {
        sqlx::query!(
            "UPDATE notify_channel SET last_sent_ms = ?
             WHERE id = (SELECT channel_id FROM notify_outbox WHERE id = ?)",
            at_ms,
            id,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "UPDATE notify_outbox SET sent_ms = ? WHERE id = ? AND sent_ms IS NULL",
            at_ms,
            id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Marks `ids` failed: one more try, the error, and when to try again.
pub(crate) async fn failed(
    conn: &mut SqliteConnection,
    ids: &[i64],
    error: &str,
    retry_at_ms: i64,
) -> Result<(), Error> {
    for id in ids {
        sqlx::query!(
            "UPDATE notify_outbox SET tries = tries + 1, last_error = ?, retry_at_ms = ?
             WHERE id = ? AND sent_ms IS NULL AND dropped_ms IS NULL",
            error,
            retry_at_ms,
            id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Marks `ids` dropped, never to be sent, saying why.
pub(crate) async fn dropped(
    conn: &mut SqliteConnection,
    ids: &[i64],
    why: &str,
    at_ms: i64,
) -> Result<(), Error> {
    for id in ids {
        sqlx::query!(
            "UPDATE notify_outbox SET dropped_ms = ?, last_error = ?
             WHERE id = ? AND sent_ms IS NULL AND dropped_ms IS NULL",
            at_ms,
            why,
            id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

impl Reader {
    /// The pending notices of the live channel `channel`, oldest first; none for a name no
    /// live channel has.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn pending_notices(&self, channel: &str) -> Result<Vec<NoticeRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT o.id AS "id!: i64", o.key, o.event, o.repo, o.item, o.facts, o.created_ms,
                      o.tries, o.last_error, o.retry_at_ms
               FROM notify_outbox o JOIN notify_channel c ON c.id = o.channel_id
               WHERE c.name = ? AND c.removed_ms IS NULL
                 AND o.sent_ms IS NULL AND o.dropped_ms IS NULL
               ORDER BY o.created_ms, o.id"#,
            channel,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(NoticeRow {
                    facts: serde_json::from_str(&row.facts).map_err(|e| {
                        Error::Corrupt(format!("the facts of notice {}: {e}", row.key))
                    })?,
                    tries: u32::try_from(row.tries).map_err(|_| {
                        Error::Corrupt(format!("the tries of notice {}: {}", row.key, row.tries))
                    })?,
                    id: row.id,
                    key: row.key,
                    event: row.event,
                    repo: row.repo,
                    item: row.item,
                    created_ms: row.created_ms,
                    last_error: row.last_error,
                    retry_at_ms: row.retry_at_ms,
                })
            })
            .collect()
    }

    /// The live notification channels, by name.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn channels(&self) -> Result<Vec<ChannelRow>, Error> {
        let rows = sqlx::query!(
            "SELECT name, kind, params, events, min_gap_ms, secret_entry, last_sent_ms,
                    created_ms, changed_ms
             FROM notify_channel WHERE removed_ms IS NULL ORDER BY name"
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ChannelRow {
                    params: serde_json::from_str(&row.params).map_err(|e| {
                        Error::Corrupt(format!("the params of channel {}: {e}", row.name))
                    })?,
                    events: serde_json::from_str(&row.events).map_err(|e| {
                        Error::Corrupt(format!("the events of channel {}: {e}", row.name))
                    })?,
                    min_gap_ms: from_db(row.min_gap_ms, "the minimum gap")?,
                    name: row.name,
                    kind: row.kind,
                    secret_entry: row.secret_entry,
                    last_sent_ms: row.last_sent_ms,
                    created_ms: row.created_ms,
                    changed_ms: row.changed_ms,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Options, Store, Write};

    fn channel(name: &str, at_ms: i64) -> NewChannel {
        NewChannel {
            name: name.to_owned(),
            kind: "fake".to_owned(),
            params: BTreeMap::from([("sink".to_owned(), "s".to_owned())]),
            events: vec!["needs_you".to_owned()],
            min_gap_ms: 60_000,
            secret_entry: format!("notify/a/{name}"),
            at_ms,
        }
    }

    fn subscribed(name: &str, events: &[&str], at_ms: i64) -> NewChannel {
        NewChannel {
            events: events.iter().map(|e| (*e).to_owned()).collect(),
            ..channel(name, at_ms)
        }
    }

    /// #9: a transition into Needs you queues its notice in its own transaction, one row per
    /// live channel subscribed to `needs_you`, keyed by the transition, once across a replay
    /// (`mutant_store_notify_every_channel` fans out to every channel).
    #[tokio::test]
    async fn a_needs_you_transition_queues_one_row_per_subscribed_channel() {
        use bosun_core::{Ctx, Event, Item, ItemId, State, step};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let w = store.writer();
        w.write(vec![
            Write::ChannelAdded(subscribed("ops", &["needs_you"], 1)),
            Write::ChannelAdded(subscribed("quiet", &["ready_drained"], 1)),
            Write::ChannelAdded(subscribed("gone", &["needs_you"], 1)),
        ])
        .await
        .unwrap();
        w.write(vec![Write::ChannelRemoved {
            name: "gone".to_owned(),
            at_ms: 2,
        }])
        .await
        .unwrap();

        let id = ItemId::new("demo#1");
        w.write(vec![Write::Item(crate::NewItem {
            item: id.clone(),
            name: "demo".to_owned(),
            repo: "/work/demo".to_owned(),
            card_key: Some("card-1".to_owned()),
            at_ms: 3,
        })])
        .await
        .unwrap();
        let ctx = Ctx::at(10);
        let claimed = step(&Item::new(id.clone()), &Event::Claim, &ctx).unwrap();
        let linted = step(&claimed.item, &Event::LintFailed, &ctx).unwrap();
        assert_eq!(linted.item.state, State::NeedsYou(bosun_core::Reason::Lint));
        let transitions = [&claimed, &linted].map(|s| Write::Transition {
            stepped: s.clone(),
            at_ms: 10,
        });
        w.write(transitions.to_vec()).await.unwrap();

        let reader = store.reader();
        let ops = reader.pending_notices("ops").await.unwrap();
        assert_eq!(ops.len(), 1, "{ops:#?}");
        assert_eq!(
            ops[0].key,
            format!("needs_you:demo#1/t{}", linted.transition.key.seq)
        );
        assert_eq!(ops[0].event, "needs_you");
        assert_eq!(ops[0].repo, "demo");
        assert_eq!(ops[0].item.as_deref(), Some("demo#1"));
        assert_eq!(ops[0].facts["reason"], "lint");
        assert_eq!(reader.pending_notices("quiet").await.unwrap(), []);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM notify_outbox")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(rows, 1, "the removed channel got no row");

        // A replay of the same transition queues nothing more.
        w.write(vec![transitions[1].clone()]).await.unwrap();
        assert_eq!(reader.pending_notices("ops").await.unwrap(), ops);

        w.write(vec![Write::NoticeQueued(NewNotice {
            key: "ready_drained:demo/demo#1".to_owned(),
            event: "ready_drained".to_owned(),
            repo: "demo".to_owned(),
            item: Some("demo#1".to_owned()),
            facts: BTreeMap::new(),
            at_ms: 20,
        })])
        .await
        .unwrap();
        let quiet = reader.pending_notices("quiet").await.unwrap();
        assert_eq!(quiet.len(), 1, "{quiet:#?}");
        assert_eq!(quiet[0].event, "ready_drained");
        assert_eq!(reader.pending_notices("ops").await.unwrap(), ops);

        // Sent: no longer pending, and the channel's last send is recorded.
        w.write(vec![Write::NoticesSent {
            ids: vec![ops[0].id],
            at_ms: 30,
        }])
        .await
        .unwrap();
        assert_eq!(reader.pending_notices("ops").await.unwrap(), []);
        let live = reader.channels().await.unwrap();
        let ops_row = live.iter().find(|c| c.name == "ops").unwrap();
        assert_eq!(ops_row.last_sent_ms, Some(30));
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_channel_is_added_once_per_live_name_and_removed_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let w = store.writer();
        w.write(vec![Write::ChannelAdded(channel("ops", 1))])
            .await
            .unwrap();
        let again = w.write(vec![Write::ChannelAdded(channel("ops", 2))]).await;
        assert!(matches!(again, Err(Error::Refused(_))), "{again:?}");
        let bad = w.write(vec![Write::ChannelAdded(channel("Ops!", 2))]).await;
        assert!(matches!(bad, Err(Error::Refused(_))), "{bad:?}");
        let live = store.reader().channels().await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].params["sink"], "s");
        assert_eq!(live[0].events, ["needs_you"]);
        assert_eq!(live[0].secret_entry, "notify/a/ops");

        w.write(vec![Write::ChannelRemoved {
            name: "ops".to_owned(),
            at_ms: 3,
        }])
        .await
        .unwrap();
        assert_eq!(
            store.reader().channels().await.unwrap(),
            Vec::<ChannelRow>::new()
        );
        let unknown = w
            .write(vec![Write::ChannelRemoved {
                name: "ops".to_owned(),
                at_ms: 4,
            }])
            .await;
        assert!(matches!(unknown, Err(Error::UnknownRow(_))), "{unknown:?}");
        // The name is free again.
        w.write(vec![Write::ChannelAdded(channel("ops", 5))])
            .await
            .unwrap();
        assert_eq!(store.reader().channels().await.unwrap()[0].created_ms, 5);
        store.close().await.unwrap();
    }
}
