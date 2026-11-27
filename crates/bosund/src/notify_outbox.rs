//! The notification drainer of one identity (#9, "Delivery through the outbox"; #208): it
//! sends the identity's queued notices on the identity's own channels.
//!
//! A notice is queued in the store in the transaction of its event (a transition into Needs
//! you, `bosun_store`'s `record_transition`; a repository paused by the fast-death breaker and a
//! drained Ready column, the loop), one row per subscribed channel, deduplicated by the event's
//! key. Every [`POLL`] this task looks at each live channel of its identity:
//!
//! - its pending rows go through [`bosun_notify::policy::decide`]: a row older than a day is
//!   dropped with a log line; nothing goes out before the channel's minimum gap since its last
//!   message; then every due row goes in one message (a burst is coalesced).
//! - the message is rendered at send time from the rows ([`bosun_notify::render`]: ids, states,
//!   reasons and numbers, plain text, capped at [`CAP`] characters, scrubbed), and sent through
//!   the channel's notifier, built with its secret from the identity's credential store, on the
//!   blocking pool.
//! - accepted: the rows are marked sent, and the channel's last send, in one batch. A failure
//!   that may pass ([`bosun_notify::SendError::Retry`]): the rows are retried after
//!   [`bosun_notify::policy::backoff`]. A permanent one, or a channel that cannot be built: the
//!   rows are dropped with a log line.
//!
//! While the kill switch is thrown (`Control::board_writes_stopped`) nothing is sent; the rows
//! wait. The drainer reads only its own identity's database, so a channel of one identity
//! never carries another's notice (#2).
//!
//! # At least once
//!
//! A row is marked sent only after its channel accepted the message. A daemon that stops
//! between the two sends that message again at its next start: the services take no
//! idempotency key, so delivery is at least once. That is the decision, not a bug.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use bosun_notify::policy::{self, Pending};
use bosun_notify::secret::Secrets;
use bosun_notify::{Kinds, Notice, SendError};
use bosun_store::{ChannelRow, NoticeRow, Reader, Write, Writer};
use tokio::task::JoinHandle;

use crate::control::Control;
use crate::executor::{blocking, now_ms};

/// How often the drainer looks at its channels.
#[cfg(not(test))]
const POLL: Duration = Duration::from_secs(2);
#[cfg(test)]
const POLL: Duration = Duration::from_millis(50);

/// The longest message, in characters.
pub(crate) const CAP: usize = 3_500;

/// The readers of every identity this daemon opened, for `mutant_notify_shared_channel` only:
/// the real drainer never reads another identity's database.
pub(crate) type Everyone = Arc<Mutex<Vec<Reader>>>;

/// One identity's drainer.
pub(crate) struct Drainer {
    pub(crate) identity: String,
    pub(crate) reader: Reader,
    pub(crate) writer: Writer,
    pub(crate) secrets: Arc<dyn Secrets>,
    /// The daemon's control, once the daemon attached it; none in tests that serve no API.
    pub(crate) control: Arc<OnceLock<Control>>,
    pub(crate) everyone: Everyone,
}

impl Drainer {
    /// Runs until aborted.
    pub(crate) fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.pass().await;
                tokio::time::sleep(POLL).await;
            }
        })
    }

    fn stopped(&self) -> bool {
        self.control
            .get()
            .is_some_and(Control::board_writes_stopped)
    }

    fn log(&self, line: &str) {
        crate::log::log!("bosund: identity {}: {line}", self.identity);
    }

    /// One look at every live channel.
    async fn pass(&self) {
        if self.stopped() {
            return;
        }
        let channels = match self.reader.channels().await {
            Ok(channels) => channels,
            Err(e) => {
                self.log(&format!("the notification channels cannot be read: {e}"));
                return;
            }
        };
        for channel in channels {
            if self.stopped() {
                return;
            }
            self.drain(&channel).await;
        }
    }

    async fn write(&self, write: Write) {
        if let Err(e) = self.writer.write(vec![write]).await {
            self.log(&format!("the notification outbox cannot be written: {e}"));
        }
    }

    async fn drop_rows(&self, channel: &ChannelRow, rows: &[&NoticeRow], why: &str) {
        if rows.is_empty() {
            return;
        }
        for row in rows {
            self.log(&format!(
                "notice {} to channel {} dropped: {why}",
                row.key, channel.name
            ));
        }
        self.write(Write::NoticesDropped {
            ids: rows.iter().map(|r| r.id).collect(),
            why: why.to_owned(),
            at_ms: now_ms(),
        })
        .await;
    }

    /// Drops, sends or leaves the pending rows of `channel`.
    async fn drain(&self, channel: &ChannelRow) {
        let pending = match self.reader.pending_notices(&channel.name).await {
            Ok(pending) if pending.is_empty() => return,
            Ok(pending) => pending,
            Err(e) => {
                self.log(&format!(
                    "the notices of channel {} cannot be read: {e}",
                    channel.name
                ));
                return;
            }
        };
        let now = now_ms();
        let gap = i64::try_from(channel.min_gap_ms).unwrap_or(i64::MAX);
        let decision = policy::decide(
            &pending
                .iter()
                .map(|r| Pending {
                    id: r.id,
                    created_ms: r.created_ms,
                    retry_at_ms: r.retry_at_ms,
                })
                .collect::<Vec<_>>(),
            channel.last_sent_ms,
            gap,
            now,
        );
        let rows = |ids: &[i64]| -> Vec<&NoticeRow> {
            pending.iter().filter(|r| ids.contains(&r.id)).collect()
        };
        self.drop_rows(
            channel,
            &rows(&decision.drop),
            "older than the maximum age of a notice",
        )
        .await;
        let send = rows(&decision.send);
        if send.is_empty() {
            return;
        }
        let notices: Vec<Notice> = send
            .iter()
            .map(|r| Notice {
                event: r.event.clone(),
                repo: r.repo.clone(),
                item: r.item.clone(),
                facts: r.facts.clone(),
            })
            .collect();
        let text = bosun_notify::render(&self.identity, &notices, CAP);
        let ids: Vec<i64> = send.iter().map(|r| r.id).collect();
        match self.deliver(channel, &text).await {
            Ok(()) => {
                self.write(Write::NoticesSent {
                    ids,
                    at_ms: now_ms(),
                })
                .await;
            }
            Err(SendError::Retry { after, error }) => {
                let tries = send.iter().map(|r| r.tries).max().unwrap_or(0) + 1;
                let wait = policy::backoff(tries, after);
                let retry_at_ms =
                    now_ms().saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
                self.log(&format!(
                    "channel {}: a send of {} notice(s) failed, retried in {} s: {error}",
                    channel.name,
                    ids.len(),
                    wait.as_secs()
                ));
                self.write(Write::NoticesFailed {
                    ids,
                    error,
                    retry_at_ms,
                })
                .await;
            }
            Err(SendError::Permanent(why)) => {
                self.drop_rows(channel, &send, &format!("refused: {why}"))
                    .await;
            }
        }
    }

    /// Sends `text` on `channel`; its result is the channel's.
    async fn deliver(&self, channel: &ChannelRow, text: &str) -> Result<(), SendError> {
        let mut others = Vec::new();
        // Mutant (`mutant_notify_shared_channel`): the message also goes out on the live
        // channels of every identity the daemon serves, as a shared channel would.
        if cfg!(mutant_notify_shared_channel) {
            let everyone = self
                .everyone
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            for reader in everyone {
                if let Ok(theirs) = reader.channels().await {
                    others.extend(theirs.into_iter().filter(|c| c != channel));
                }
            }
        }
        for other in others {
            let _ = self.send(&other, text.to_owned()).await;
        }
        self.send(channel, text.to_owned()).await
    }

    /// Sends `text` on `channel`, with its secret from the identity's credential store.
    async fn send(&self, channel: &ChannelRow, text: String) -> Result<(), SendError> {
        let secrets = self.secrets.clone();
        let channel = channel.clone();
        blocking(move || {
            let secret = secrets
                .get(&channel.secret_entry)
                .map_err(|e| SendError::Retry {
                    after: None,
                    error: format!("the credential store: {e}"),
                })?;
            let notifier = bosun_notify::build(
                &channel.kind,
                &channel.params,
                secret.as_deref(),
                Kinds { fake: cfg!(test) },
            )
            .map_err(SendError::Permanent)?;
            drop(secret);
            notifier.send(&text)
        })
        .await
    }
}
