//! A subscription to an identity's event stream that reconnects by itself (#4, #69).
//!
//! [`Subscription::next`] returns the events in sequence order. When the connection breaks it
//! reconnects, naming the epoch and the sequence after the last one it delivered, so the
//! daemon either replays every missed event or answers with a resync, which `next` returns as
//! [`Delivery::Resync`]: the caller refetches its state and goes on. There is never a silent
//! gap.
//!
//! An event whose kind this client does not know is skipped, and its sequence still counts as
//! seen (#12: new event kinds are additive).

use std::fmt;
use std::time::Duration;

use crate::{CONNECT_TIMEOUT, Client, Endpoint, Error, Reader, Stream, Writer, framing};
use bosun_api::event::{EVENT, RESYNC};
use bosun_api::rpc::Request;
use bosun_api::{Event, EventBody, EventFilter, Resync, SubscribeParams, method};
use serde::Deserialize;
use serde_json::Value;

/// Where a subscription stands: the daemon's epoch and the last sequence delivered (or known
/// to be covered by a resync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

/// What [`Subscription::next`] returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    /// The next event.
    Event(Event),
    /// Events were missed: refetch the state. The stream goes on after `head`.
    Resync(Resync),
}

/// The first wait before a reconnect; it doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

enum Source {
    Endpoint(Endpoint),
    /// One stream, used once: no reconnect.
    Stream(Option<Box<dyn Stream>>),
}

/// The open connection of a subscription.
struct Connection {
    reader: Reader,
    /// Held so the daemon sees the connection open; nothing more is written.
    _writer: Writer,
}

/// A subscription to one identity's events.
pub struct Subscription {
    source: Source,
    identity: String,
    filter: EventFilter,
    cursor: Option<Cursor>,
    connection: Option<Connection>,
    pending: Option<Resync>,
    skipped: u64,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("identity", &self.identity)
            .field("filter", &self.filter)
            .field("cursor", &self.cursor)
            .field("connected", &self.connection.is_some())
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

/// An event as read, before its body is known to be of a kind this client knows.
#[derive(Deserialize)]
struct RawEvent {
    seq: u64,
    ts_ms: i64,
    body: Value,
}

impl Subscription {
    /// Subscribes to the events `identity` publishes from now on. Connects at once, so a
    /// daemon that is not running is an error here; later reconnects wait for it.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`], or the daemon refused the subscription (an unknown identity).
    pub async fn open(
        endpoint: Endpoint,
        identity: impl Into<String>,
        filter: EventFilter,
    ) -> Result<Self, Error> {
        Self::start(Source::Endpoint(endpoint), identity.into(), filter, None).await
    }

    /// Subscribes from `cursor`, as a client that saw events before (a restarted TUI). The
    /// first delivery is a resync when the daemon no longer holds the events after it.
    ///
    /// # Errors
    ///
    /// As [`Subscription::open`].
    pub async fn resume(
        endpoint: Endpoint,
        identity: impl Into<String>,
        filter: EventFilter,
        cursor: Cursor,
    ) -> Result<Self, Error> {
        Self::start(
            Source::Endpoint(endpoint),
            identity.into(),
            filter,
            Some(cursor),
        )
        .await
    }

    /// Subscribes over an open stream, from `cursor` when given. It does not reconnect: once
    /// the stream ends, [`Subscription::next`] answers [`Error::Closed`].
    ///
    /// # Errors
    ///
    /// As [`Subscription::open`].
    pub async fn over(
        stream: Box<dyn Stream>,
        identity: impl Into<String>,
        filter: EventFilter,
        cursor: Option<Cursor>,
    ) -> Result<Self, Error> {
        Self::start(
            Source::Stream(Some(stream)),
            identity.into(),
            filter,
            cursor,
        )
        .await
    }

    async fn start(
        source: Source,
        identity: String,
        filter: EventFilter,
        cursor: Option<Cursor>,
    ) -> Result<Self, Error> {
        let mut subscription = Self {
            source,
            identity,
            filter,
            cursor,
            connection: None,
            pending: None,
            skipped: 0,
        };
        subscription.subscribe().await?;
        Ok(subscription)
    }

    /// Where the subscription stands; pass it to [`Subscription::resume`] to go on later.
    #[must_use]
    pub fn cursor(&self) -> Option<&Cursor> {
        self.cursor.as_ref()
    }

    /// Events skipped because their kind is unknown to this client.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Drops the connection; the next call to [`Subscription::next`] reconnects from the
    /// cursor.
    pub fn disconnect(&mut self) {
        self.connection = None;
    }

    /// The next event, or a resync. Waits for the daemon while it is away: wrap the call in a
    /// timeout to give up.
    ///
    /// # Errors
    ///
    /// The daemon refused the subscription on a reconnect, speaks an unknown API version, or
    /// sent something that is not a valid notification; [`Error::Closed`] when the stream of
    /// [`Subscription::over`] ended.
    pub async fn next(&mut self) -> Result<Delivery, Error> {
        loop {
            if let Some(resync) = self.pending.take() {
                return Ok(Delivery::Resync(resync));
            }
            let Some(connection) = self.connection.as_mut() else {
                self.reconnect().await?;
                continue;
            };
            // The end of the stream, or a broken one: reconnect from the cursor.
            let Ok(Some(line)) = framing::read_line(&mut connection.reader).await else {
                self.connection = None;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let note: Request = serde_json::from_str(&line)
                .map_err(|e| Error::Protocol(format!("a notification that does not parse: {e}")))?;
            match note.method.as_str() {
                EVENT => {
                    if let Some(event) = self.event(note.params)? {
                        return Ok(Delivery::Event(event));
                    }
                }
                RESYNC => {
                    let resync: Resync = serde_json::from_value(note.params)
                        .map_err(|e| Error::Protocol(format!("{RESYNC}: {e}")))?;
                    self.cursor = Some(Cursor {
                        epoch: resync.epoch.clone(),
                        seq: resync.head,
                    });
                    // The daemon closes the connection after a resync.
                    self.connection = None;
                    return Ok(Delivery::Resync(resync));
                }
                // A notification of a later version.
                _ => {}
            }
        }
    }

    /// Reads one event; `None` for a kind this client does not know.
    fn event(&mut self, params: Value) -> Result<Option<Event>, Error> {
        let raw: RawEvent =
            serde_json::from_value(params).map_err(|e| Error::Protocol(format!("{EVENT}: {e}")))?;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.seq = raw.seq;
        }
        let kind = raw
            .body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let known = EventBody::KINDS.contains(&kind.as_str());
        match serde_json::from_value::<EventBody>(raw.body) {
            Ok(body) => Ok(Some(Event {
                seq: raw.seq,
                ts_ms: raw.ts_ms,
                body,
            })),
            Err(e) if known => Err(Error::Protocol(format!("{EVENT} {kind}: {e}"))),
            Err(_) => {
                self.skipped += 1;
                Ok(None)
            }
        }
    }

    /// Reconnects, waiting while the daemon is not there.
    async fn reconnect(&mut self) -> Result<(), Error> {
        let mut backoff = FIRST_BACKOFF;
        loop {
            match self.subscribe().await {
                Ok(()) => return Ok(()),
                Err(Error::NotRunning { .. } | Error::Io(_) | Error::Timeout | Error::Closed)
                    if matches!(self.source, Source::Endpoint(_)) =>
                {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Connects and subscribes from the cursor.
    async fn subscribe(&mut self) -> Result<(), Error> {
        let stream = match &mut self.source {
            Source::Endpoint(endpoint) => {
                tokio::time::timeout(CONNECT_TIMEOUT, crate::open(endpoint))
                    .await
                    .map_err(|_| Error::Timeout)??
            }
            Source::Stream(stream) => stream.take().ok_or(Error::Closed)?,
        };
        let params = SubscribeParams {
            identity: self.identity.clone(),
            epoch: self.cursor.as_ref().map(|c| c.epoch.clone()),
            from_seq: self.cursor.as_ref().map(|c| c.seq + 1),
            filter: self.filter.clone(),
        };
        let answer = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut client = Client::over(stream).await?;
            let result = client.call::<method::EventsSubscribe>(&params).await?;
            Ok::<_, Error>((client, result))
        })
        .await
        .map_err(|_| Error::Timeout)?;
        let (client, result) = answer?;
        if let Some(reason) = result.resync {
            self.pending = Some(Resync {
                epoch: result.epoch.clone(),
                head: result.head,
                reason,
            });
            self.cursor = Some(Cursor {
                epoch: result.epoch,
                seq: result.head,
            });
        } else if self.cursor.is_none() {
            self.cursor = Some(Cursor {
                epoch: result.epoch,
                seq: result.head,
            });
        }
        let (reader, writer) = client.into_parts();
        self.connection = Some(Connection {
            reader,
            _writer: writer,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bosun_api::rpc::Response;
    use bosun_api::{HelloResult, SubscribeResult};
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};

    use super::*;

    /// A fake daemon: answers `hello` and `events.subscribe`, then writes `lines`.
    async fn daemon(theirs: DuplexStream, lines: Vec<Value>) {
        let (read, mut write) = tokio::io::split(theirs);
        let mut read = BufReader::new(read);
        for _ in 0..2 {
            let line = framing::read_line(&mut read).await.unwrap().unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            let result = if request.method == "hello" {
                serde_json::to_value(HelloResult {
                    api_version: bosun_api::API_VERSION,
                    daemon_version: "0.0.0".to_owned(),
                    pid: 1,
                })
            } else {
                serde_json::to_value(SubscribeResult {
                    identity: "i".to_owned(),
                    epoch: "e".to_owned(),
                    head: 0,
                    resync: None,
                })
            };
            let response = Response::result(request.id.unwrap(), result.unwrap());
            framing::write_message(&mut write, &response).await.unwrap();
        }
        for line in lines {
            framing::write_message(&mut write, &line).await.unwrap();
        }
        write.flush().await.unwrap();
        let _ = framing::read_line(&mut read).await;
    }

    fn event(seq: u64, body: &Value) -> Value {
        json!({"jsonrpc": "2.0", "method": EVENT, "params": {"seq": seq, "ts_ms": 0, "body": body}})
    }

    #[tokio::test]
    async fn an_unknown_event_kind_is_skipped() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let lines = vec![
            event(
                1,
                &json!({"type": "from_a_later_version", "anything": [1, 2]}),
            ),
            json!({"jsonrpc": "2.0", "method": "events.from_a_later_version", "params": {}}),
            event(
                2,
                &json!({"type": "limit_resumed", "item": "a", "attempt": 1}),
            ),
        ];
        let fake = tokio::spawn(daemon(theirs, lines));
        let mut sub = Subscription::over(Box::new(ours), "i", EventFilter::default(), None)
            .await
            .unwrap();
        let Delivery::Event(e) = sub.next().await.unwrap() else {
            panic!("a resync");
        };
        assert_eq!(e.seq, 2);
        assert_eq!(e.body.kind(), "limit_resumed");
        assert_eq!(sub.skipped(), 1);
        assert_eq!(sub.cursor().unwrap().seq, 2);
        fake.abort();
    }

    #[tokio::test]
    async fn a_known_kind_that_does_not_parse_is_an_error() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let lines = vec![event(1, &json!({"type": "limit_resumed", "item": 3}))];
        let fake = tokio::spawn(daemon(theirs, lines));
        let mut sub = Subscription::over(Box::new(ours), "i", EventFilter::default(), None)
            .await
            .unwrap();
        assert!(matches!(sub.next().await, Err(Error::Protocol(_))));
        fake.abort();
    }
}
