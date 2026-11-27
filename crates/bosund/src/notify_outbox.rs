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
//! - a mark the store refused (sent, failed or dropped after a send) is kept in memory and
//!   written again first at every pass; until it lands nothing is sent on that channel, so its
//!   rows, still pending in the store, never go out twice.
//! - a service that names its own wait (Telegram's `retry_after`) holds the whole channel, not
//!   only the rows it refused: no row of the channel is sent before the wait ends. The hold is
//!   in memory; a restart forgets it, and the refused rows still wait for their retry.
//!
//! A stop ([`Stop`]) lets the pass in flight end, its send and its mark, and ends the loop
//! there; the daemon aborts it only after [`STOP_DEADLINE`].
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use bosun_notify::policy::{self, Pending};
use bosun_notify::secret::Secrets;
use bosun_notify::{Kinds, Notice, SendError};
use bosun_store::{ChannelRow, NoticeRow, Reader, Write, Writer};
use tokio::sync::watch;
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

/// How long a stop waits for the pass in flight: a send's own timeout and a few seconds for its
/// mark.
pub(crate) const STOP_DEADLINE: Duration =
    Duration::from_secs(bosun_notify::SEND_TIMEOUT.as_secs() + 5);

/// Asks a drainer to end after its pass in flight; a dropped one asks it too.
pub(crate) type Stop = watch::Sender<bool>;

/// The faults a test throws into a drainer: while `refuse_writes` is set, every write of the
/// drainer fails, as a store that refuses writes would.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Faults {
    pub(crate) refuse_writes: std::sync::atomic::AtomicBool,
}

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
    #[cfg(test)]
    pub(crate) faults: Arc<Faults>,
}

/// What a drainer keeps across its passes, per channel name.
#[derive(Debug, Default)]
struct Memory {
    /// The mark of a send the store refused, to write before anything else is sent.
    unmarked: BTreeMap<String, Write>,
    /// The end of the wait the channel's service asked for, in ms.
    held_until: BTreeMap<String, i64>,
}

impl Drainer {
    /// Runs until its [`Stop`] says so (or is dropped), at the end of a pass.
    pub(crate) fn spawn(self) -> (JoinHandle<()>, Stop) {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut memory = Memory::default();
            loop {
                self.pass(&mut memory, &stopping).await;
                if *stopping.borrow() {
                    return;
                }
                tokio::select! {
                    () = tokio::time::sleep(POLL) => {}
                    _ = stopping.changed() => return,
                }
            }
        });
        (task, stop)
    }

    fn stopped(&self) -> bool {
        self.control
            .get()
            .is_some_and(Control::board_writes_stopped)
    }

    fn log(&self, line: &str) {
        crate::log::log!("bosund: identity {}: {line}", self.identity);
    }

    /// One look at every live channel; a stop asked for ends it before the next channel.
    async fn pass(&self, memory: &mut Memory, stopping: &watch::Receiver<bool>) {
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
            if self.stopped() || *stopping.borrow() {
                return;
            }
            self.drain(&channel, memory).await;
        }
    }

    /// Writes `write`; `false`, with a log line, when the store refused it.
    async fn write(&self, write: Write) -> bool {
        #[cfg(test)]
        if self
            .faults
            .refuse_writes
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.log("the notification outbox cannot be written: refused by the test");
            return false;
        }
        match self.writer.write(vec![write]).await {
            Ok(()) => true,
            Err(e) => {
                self.log(&format!("the notification outbox cannot be written: {e}"));
                false
            }
        }
    }

    /// Writes the mark of a send on `channel`; a refused one is kept in `memory`, and the
    /// channel sends nothing until it lands.
    async fn mark(&self, channel: &ChannelRow, memory: &mut Memory, write: Write) {
        if !self.write(write.clone()).await {
            memory.unmarked.insert(channel.name.clone(), write);
        }
    }

    async fn drop_rows(&self, channel: &ChannelRow, rows: &[&NoticeRow], why: &str) {
        if rows.is_empty() {
            return;
        }
        self.log_dropped(channel, rows, why);
        self.write(dropped(rows, why)).await;
    }

    fn log_dropped(&self, channel: &ChannelRow, rows: &[&NoticeRow], why: &str) {
        for row in rows {
            self.log(&format!(
                "notice {} to channel {} dropped: {why}",
                row.key, channel.name
            ));
        }
    }

    /// Drops, sends or leaves the pending rows of `channel`.
    // Long: the mark and the hold first, then one arm per outcome of the send.
    #[allow(clippy::too_many_lines)]
    async fn drain(&self, channel: &ChannelRow, memory: &mut Memory) {
        // Mutant (`mutant_outbox_send_before_mark_lands`): a refused mark is forgotten, so the
        // rows it marks, still pending in the store, are sent again.
        if cfg!(mutant_outbox_send_before_mark_lands) {
            memory.unmarked.remove(&channel.name);
        }
        if let Some(mark) = memory.unmarked.get(&channel.name) {
            if !self.write(mark.clone()).await {
                return;
            }
            memory.unmarked.remove(&channel.name);
        }
        let now = now_ms();
        // Mutant (`mutant_outbox_hold_only_failed_rows`): the service's wait holds only the
        // rows it refused, and a row queued meanwhile goes out at once.
        if !cfg!(mutant_outbox_hold_only_failed_rows)
            && memory
                .held_until
                .get(&channel.name)
                .is_some_and(|until| *until > now)
        {
            return;
        }
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
                let mark = Write::NoticesSent {
                    ids,
                    at_ms: now_ms(),
                };
                self.mark(channel, memory, mark).await;
            }
            Err(SendError::Retry { after, error }) => {
                let tries = send.iter().map(|r| r.tries).max().unwrap_or(0) + 1;
                let wait = policy::backoff(tries, after);
                let at = now_ms();
                let retry_at_ms = at.saturating_add(ms(wait));
                if let Some(after) = after {
                    memory
                        .held_until
                        .insert(channel.name.clone(), at.saturating_add(ms(after)));
                }
                self.log(&format!(
                    "channel {}: a send of {} notice(s) failed, retried in {} s: {error}",
                    channel.name,
                    ids.len(),
                    wait.as_secs()
                ));
                let failed = Write::NoticesFailed {
                    ids,
                    error,
                    retry_at_ms,
                };
                self.mark(channel, memory, failed).await;
            }
            Err(SendError::Permanent(why)) => {
                let why = format!("refused: {why}");
                self.log_dropped(channel, &send, &why);
                self.mark(channel, memory, dropped(&send, &why)).await;
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

/// The mark of `rows` dropped, saying `why`.
fn dropped(rows: &[&NoticeRow], why: &str) -> Write {
    Write::NoticesDropped {
        ids: rows.iter().map(|r| r.id).collect(),
        why: why.to_owned(),
        at_ms: now_ms(),
    }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
