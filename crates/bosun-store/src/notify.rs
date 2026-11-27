//! The identity's notification channels (#9, the decision on channels; migration 20).
//!
//! A channel row holds its kind, its non-secret parameters, the events it gets and its minimum
//! gap, and the name of the credential-store entry its secret lives in
//! ([`NewChannel::secret_entry`]), never the secret. [`Write::ChannelAdded`] refuses a name a
//! live channel has; [`Write::ChannelRemoved`] keeps the row, marked removed, and frees the
//! name.
//!
//! [`Write::ChannelAdded`]: crate::Write::ChannelAdded
//! [`Write::ChannelRemoved`]: crate::Write::ChannelRemoved

use std::collections::BTreeMap;

use sqlx::sqlite::SqliteConnection;

use crate::error::{Error, from_db, to_db};
use crate::read::Reader;

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

impl Reader {
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
