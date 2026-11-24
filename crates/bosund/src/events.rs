//! The event stream of one identity (#4, #69): a monotonic sequence, a bounded ring, and the
//! subscribers that read it.
//!
//! [`Events::publish`] assigns the next sequence, serialises the notification once, pushes it
//! into the ring (dropping the oldest event when the ring is full) and wakes the subscribers.
//! It takes one short lock and never waits for a subscriber: each subscriber keeps its own
//! cursor into the ring, and one that falls behind the ring's oldest event is sent a resync and
//! dropped. A subscriber that stops reading altogether is dropped when a write to it has not
//! completed within [`Options::stall`]. So the publisher's latency does not depend on its
//! subscribers.
//!
//! The loop (#62) publishes through the same handle; [`transition`] and the other functions
//! here turn the item and attempt machines' types into event bodies.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bosun_api::event::{EVENT, RESYNC};
use bosun_api::rpc::JSONRPC;
use bosun_api::{Event, EventBody, EventFilter, Resync, ResyncReason, SubscribeResult};
use bosun_core::{Exit, ItemId, Outcome, ParkReason, Reason, RunResult, State, Transition};
use serde::Serialize;
use tokio::sync::watch;

/// How the ring is bounded and when a stalled subscriber is dropped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    /// Events held for replay; the oldest is dropped beyond this.
    pub(crate) capacity: usize,
    /// How long one write to a subscriber may take before it is dropped.
    pub(crate) stall: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            capacity: 4096,
            stall: Duration::from_secs(10),
        }
    }
}

/// One event in the ring, its notification line serialised once for every subscriber.
#[derive(Debug)]
pub(crate) struct Stored {
    pub(crate) seq: u64,
    ts_ms: i64,
    kind: &'static str,
    /// For a session output line, its item; output lines go to tail subscriptions only.
    output_of: Option<String>,
    /// The `events.event` notification, ended by `\n`.
    pub(crate) line: Arc<[u8]>,
}

#[derive(Debug)]
struct Ring {
    events: VecDeque<Arc<Stored>>,
    /// The sequence of the next event; the first is 1.
    next: u64,
}

impl Ring {
    /// The oldest sequence held, or the next one when the ring is empty.
    fn first(&self) -> u64 {
        self.events.front().map_or(self.next, |e| e.seq)
    }
}

#[derive(Debug)]
struct Inner {
    epoch: String,
    options: Options,
    ring: Mutex<Ring>,
    /// The sequence of the last event published.
    head: watch::Sender<u64>,
    /// Each live subscriber's last delivered sequence.
    subscribers: Mutex<Vec<Weak<AtomicU64>>>,
}

/// The event stream of one identity. Clones publish to the same stream.
#[derive(Debug, Clone)]
pub(crate) struct Events {
    inner: Arc<Inner>,
}

#[derive(Serialize)]
struct Notification<'a, T> {
    jsonrpc: &'a str,
    method: &'a str,
    params: &'a T,
}

/// A notification as one line.
pub(crate) fn notification<T: Serialize>(method: &str, params: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(&Notification {
        jsonrpc: JSONRPC,
        method,
        params,
    })
    .expect("an event always serialises");
    line.push(b'\n');
    line
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves the ring consistent: every write is one push/pop.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Events {
    /// An empty stream of the daemon run `epoch`.
    pub(crate) fn new(epoch: impl Into<String>, options: Options) -> Self {
        Self {
            inner: Arc::new(Inner {
                epoch: epoch.into(),
                options,
                ring: Mutex::new(Ring {
                    events: VecDeque::with_capacity(options.capacity.min(1 << 16)),
                    next: 1,
                }),
                head: watch::Sender::new(0),
                subscribers: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.inner.epoch
    }

    pub(crate) fn stall(&self) -> Duration {
        self.inner.options.stall
    }

    /// Publishes `body` and returns its sequence. Never waits for a subscriber.
    pub(crate) fn publish(&self, body: EventBody) -> u64 {
        let kind = body.kind();
        let output_of = match &body {
            EventBody::SessionOutput { item, .. } => Some(item.clone()),
            _ => None,
        };
        let seq = {
            let mut ring = lock(&self.inner.ring);
            let seq = ring.next;
            ring.next += 1;
            let ts_ms = now_ms();
            let event = Event { seq, ts_ms, body };
            let line = notification(EVENT, &event).into();
            ring.events.push_back(Arc::new(Stored {
                seq,
                ts_ms,
                kind,
                output_of,
                line,
            }));
            while ring.events.len() > self.inner.options.capacity.max(1) {
                ring.events.pop_front();
            }
            seq
        };
        self.inner.head.send_replace(seq);
        if cfg!(mutant_api_slow_subscriber_blocks) {
            // Mutant: the publisher waits for its subscribers to catch up.
            self.wait_for_subscribers(seq);
        }
        seq
    }

    /// The mutant's backpressure: up to a quarter of a second per event.
    fn wait_for_subscribers(&self, seq: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while std::time::Instant::now() < deadline
            && lock(&self.inner.subscribers)
                .iter()
                .filter_map(Weak::upgrade)
                .any(|delivered| delivered.load(Ordering::Relaxed) + 16 < seq)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The sequence of the last event published; 0 before the first.
    pub(crate) fn head(&self) -> u64 {
        *self.inner.head.borrow()
    }

    /// Live subscribers.
    pub(crate) fn subscribers(&self) -> usize {
        let mut subscribers = lock(&self.inner.subscribers);
        subscribers.retain(|s| s.strong_count() > 0);
        subscribers.len()
    }

    /// The output lines of `item` the ring still holds, from `from_seq` on (#74), with the head
    /// they reach and the time of the oldest event held. Every output line of the item from
    /// the first returned up to the head is among them: the ring drops only its oldest.
    pub(crate) fn output_of(&self, item: &str, from_seq: Option<u64>) -> Held {
        let ring = lock(&self.inner.ring);
        let from = from_seq.unwrap_or(0);
        let lines = ring
            .events
            .iter()
            .filter(|e| e.seq >= from && e.output_of.as_deref() == Some(item))
            .filter_map(|e| event_of(&e.line))
            .collect();
        Held {
            head: ring.next - 1,
            dropped: ring.first() > 1,
            gap: from_seq.is_some_and(|f| f < ring.first()),
            oldest_ms: ring.events.front().map(|e| e.ts_ms),
            lines,
        }
    }

    /// Subscribes from `from_seq` of `epoch`, or from now. The answer says where the stream
    /// starts, and whether the client must resync first.
    pub(crate) fn subscribe(
        &self,
        identity: &str,
        epoch: Option<&str>,
        from_seq: Option<u64>,
        filter: EventFilter,
    ) -> (SubscribeResult, Subscriber) {
        let ring = lock(&self.inner.ring);
        let head = ring.next - 1;
        let live = head + 1;
        let from_seq = if cfg!(mutant_api_no_event_seq) {
            // Mutant: the client's sequence is ignored; a reconnect starts from now.
            None
        } else {
            from_seq
        };
        let (start, resync) = match from_seq {
            None => (live, None),
            Some(_) if epoch != Some(self.epoch()) => (live, Some(ResyncReason::Restarted)),
            Some(seq) if seq < ring.first() || seq > live => (live, Some(ResyncReason::Gone)),
            Some(seq) => (seq, None),
        };
        // Subscribed under the ring's lock: nothing published after `head` is missed.
        let delivered = Arc::new(AtomicU64::new(start - 1));
        lock(&self.inner.subscribers).push(Arc::downgrade(&delivered));
        let mut head_rx = self.inner.head.subscribe();
        head_rx.mark_changed();
        drop(ring);
        let result = SubscribeResult {
            identity: identity.to_owned(),
            epoch: self.epoch().to_owned(),
            head,
            resync,
        };
        let subscriber = Subscriber {
            events: self.clone(),
            next: start,
            filter,
            delivered,
            head_rx,
        };
        (result, subscriber)
    }
}

/// What [`Events::output_of`] found.
#[derive(Debug)]
pub(crate) struct Held {
    /// The sequence of the last event published.
    pub(crate) head: u64,
    /// The ring has dropped events: it no longer holds everything since the daemon started.
    pub(crate) dropped: bool,
    /// The ring no longer holds the `from_seq` asked for.
    pub(crate) gap: bool,
    /// The time of the oldest event the ring holds; `None` when it is empty.
    pub(crate) oldest_ms: Option<i64>,
    pub(crate) lines: Vec<Event>,
}

/// The event of a stored notification line.
fn event_of(line: &[u8]) -> Option<Event> {
    #[derive(serde::Deserialize)]
    struct Line {
        params: Event,
    }
    serde_json::from_slice::<Line>(line).ok().map(|l| l.params)
}

/// What a subscriber reads next.
#[derive(Debug)]
pub(crate) enum Batch {
    /// Events in sequence order; empty when the subscriber is caught up.
    Events(Vec<Arc<Stored>>),
    /// The subscriber fell behind the ring: send this and drop it.
    Lagged(Vec<u8>),
}

/// One subscription's cursor into the ring.
#[derive(Debug)]
pub(crate) struct Subscriber {
    events: Events,
    /// The next sequence to look at.
    next: u64,
    filter: EventFilter,
    /// The last sequence written to the client; read by the registry.
    delivered: Arc<AtomicU64>,
    head_rx: watch::Receiver<u64>,
}

impl Subscriber {
    /// Up to `max` events from the cursor that pass the filter, or the resync when the ring
    /// moved past the cursor.
    pub(crate) fn take(&mut self, max: usize) -> Batch {
        self.head_rx.borrow_and_update();
        let ring = lock(&self.events.inner.ring);
        let first = ring.first();
        if self.next < first {
            let resync = Resync {
                epoch: self.events.epoch().to_owned(),
                head: ring.next - 1,
                reason: ResyncReason::Lagged,
            };
            return Batch::Lagged(notification(RESYNC, &resync));
        }
        let skip = usize::try_from(self.next - first).unwrap_or(usize::MAX);
        let mut batch = Vec::new();
        for stored in ring.events.iter().skip(skip) {
            self.next = stored.seq + 1;
            if admits(&self.filter, stored) {
                batch.push(stored.clone());
                if batch.len() >= max {
                    break;
                }
            }
        }
        if batch.is_empty() {
            // Everything up to here was filtered out: nothing is owed to the client.
            self.delivered.store(self.next - 1, Ordering::Relaxed);
        }
        Batch::Events(batch)
    }

    /// Records that every event up to `seq` reached the client.
    pub(crate) fn delivered(&self, seq: u64) {
        self.delivered.store(seq, Ordering::Relaxed);
    }

    /// Waits until something is published after the last [`Subscriber::take`].
    pub(crate) async fn changed(&mut self) {
        if self.head_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// How long one write may take.
    pub(crate) fn stall(&self) -> Duration {
        self.events.stall()
    }
}

fn admits(filter: &EventFilter, stored: &Stored) -> bool {
    if let Some(kinds) = &filter.kinds
        && !kinds.iter().any(|k| k == stored.kind)
    {
        return false;
    }
    let Some(item) = &stored.output_of else {
        return true;
    };
    if cfg!(mutant_api_tail_always_on) {
        // Mutant: output lines go to every subscription.
        return true;
    }
    filter
        .tail
        .as_ref()
        .is_some_and(|tail| tail.item.as_ref().is_none_or(|wanted| wanted == item))
}

/// The event of a transition of `item`'s machine, followed by a limit park or resume when the
/// transition is one.
pub(crate) fn transition(item: &ItemId, t: &Transition) -> Vec<EventBody> {
    let mut bodies = vec![EventBody::ItemTransition {
        item: item.to_string(),
        transition: t.key.seq,
        attempt: t.attempt,
        from: t.from.as_str().to_owned(),
        to: t.to.as_str().to_owned(),
        attempt_from: t.attempt_from.map(|s| s.as_str().to_owned()),
        attempt_to: t.attempt_to.map(|s| s.as_str().to_owned()),
        cause: t.cause.to_owned(),
        reason: reason_of(t.to),
    }];
    let limited = |s: State| matches!(s, State::Parked(p) if p.reason == ParkReason::Limit);
    if limited(t.to) && !limited(t.from) {
        let State::Parked(park) = t.to else {
            unreachable!("checked above")
        };
        bodies.push(EventBody::LimitParked {
            item: item.to_string(),
            attempt: t.attempt,
            until_ms: park.until_ms,
        });
    } else if limited(t.from) && !limited(t.to) {
        bodies.push(EventBody::LimitResumed {
            item: item.to_string(),
            attempt: t.attempt,
        });
    }
    bodies
}

/// Why an item is in `state`, for the states that have a reason.
pub(crate) fn reason_of(state: State) -> Option<String> {
    let name = match state {
        State::Parked(park) => match park.reason {
            ParkReason::Limit => "limit",
            ParkReason::Paused => "paused",
            ParkReason::Killed => "killed",
            ParkReason::DailyCap => "daily_cap",
        },
        State::Done(outcome) => match outcome {
            Outcome::Pass => "pass",
            Outcome::Flaky => "flaky",
            Outcome::UserClosed => "user_closed",
        },
        State::NeedsYou(reason) => match reason {
            Reason::Lint => "lint",
            Reason::Cycle => "cycle",
            Reason::Question => "question",
            Reason::Auth => "auth",
            Reason::Budget => "budget",
            Reason::ParkCap => "park_cap",
            Reason::ConfigChanged => "config_changed",
            Reason::AttemptsExhausted => "attempts_exhausted",
            Reason::Guard(_) => "guard",
            Reason::LandingRefused => "landing_refused",
            Reason::Setup => "setup",
            Reason::Handshake => "handshake",
            Reason::Identity => "identity",
        },
        _ => return None,
    };
    Some(name.to_owned())
}

/// The name of a session's exit as `session_ended` carries it; `killed` when the daemon
/// killed it.
pub(crate) fn exit_name(exit: Option<Exit>) -> &'static str {
    match exit {
        None => "killed",
        Some(Exit::Ok) => "ok",
        Some(Exit::Limit { .. }) => "limit",
        Some(Exit::Auth) => "auth",
        Some(Exit::Crash) => "crash",
    }
}

/// The name of a verdict run's result as `verdict_finished` carries it.
pub(crate) fn run_result_name(result: RunResult) -> &'static str {
    match result {
        RunResult::Pass => "pass",
        RunResult::Fail => "fail",
        RunResult::Timeout => "timeout",
    }
}

#[cfg(test)]
mod tests {
    use bosun_api::{OutputStream, Tail};
    use bosun_core::{Ctx, Event as ItemEvent, Item, step};

    use super::*;

    fn events(capacity: usize) -> Events {
        Events::new(
            "e",
            Options {
                capacity,
                stall: Duration::from_secs(1),
            },
        )
    }

    fn resumed(n: u32) -> EventBody {
        EventBody::LimitResumed {
            item: "a".to_owned(),
            attempt: n,
        }
    }

    fn output(item: &str) -> EventBody {
        EventBody::SessionOutput {
            item: item.to_owned(),
            attempt: 1,
            spawn: 1,
            stream: OutputStream::Stdout,
            line: "hello".to_owned(),
            tool: None,
        }
    }

    fn seqs(batch: Batch) -> Vec<u64> {
        match batch {
            Batch::Events(events) => events.iter().map(|e| e.seq).collect(),
            Batch::Lagged(_) => panic!("lagged"),
        }
    }

    #[test]
    fn sequences_are_monotonic_and_replayed_from_the_ring() {
        let events = events(4);
        for n in 1..=3 {
            assert_eq!(events.publish(resumed(n)), u64::from(n));
        }
        let (answer, mut sub) = events.subscribe("i", Some("e"), Some(2), EventFilter::default());
        assert_eq!((answer.head, answer.resync), (3, None));
        assert_eq!(seqs(sub.take(10)), [2, 3]);
        assert_eq!(seqs(sub.take(10)), Vec::<u64>::new());
        events.publish(resumed(4));
        assert_eq!(seqs(sub.take(10)), [4]);
    }

    #[test]
    fn a_sequence_no_longer_held_or_of_another_epoch_is_a_resync() {
        let events = events(2);
        for n in 1..=5 {
            events.publish(resumed(n));
        }
        let filter = EventFilter::default;
        let gone = events.subscribe("i", Some("e"), Some(3), filter()).0;
        assert_eq!(gone.resync, Some(ResyncReason::Gone));
        let held = events.subscribe("i", Some("e"), Some(4), filter()).0;
        assert_eq!(held.resync, None);
        let future = events.subscribe("i", Some("e"), Some(9), filter()).0;
        assert_eq!(future.resync, Some(ResyncReason::Gone));
        let restarted = events.subscribe("i", Some("old"), Some(4), filter()).0;
        assert_eq!(restarted.resync, Some(ResyncReason::Restarted));
        let live = events.subscribe("i", None, None, filter());
        assert_eq!((live.0.head, live.0.resync), (5, None));
        let mut live = live.1;
        assert_eq!(seqs(live.take(10)), Vec::<u64>::new());
    }

    /// #74: the output lines of an item the ring holds come back in order, and a `from_seq`
    /// the ring has moved past is a gap.
    #[test]
    fn the_held_output_of_an_item_is_contiguous_or_a_gap() {
        let events = events(4);
        let none = events.output_of("a", None);
        assert_eq!((none.head, none.dropped, none.gap), (0, false, false));
        assert!(none.lines.is_empty() && none.oldest_ms.is_none());
        for _ in 0..3 {
            events.publish(output("a"));
        }
        events.publish(output("b"));
        let held = events.output_of("a", Some(2));
        let seqs: Vec<u64> = held.lines.iter().map(|e| e.seq).collect();
        assert_eq!((seqs, held.head, held.gap), (vec![2, 3], 4, false));
        for _ in 0..2 {
            events.publish(output("a"));
        }
        // The ring holds 3..=6 now: from 2 is a gap, and every line still held comes back.
        let held = events.output_of("a", Some(2));
        let seqs: Vec<u64> = held.lines.iter().map(|e| e.seq).collect();
        assert_eq!((seqs, held.dropped, held.gap), (vec![3, 5, 6], true, true));
        assert!(!events.output_of("a", Some(3)).gap);
    }

    #[test]
    fn a_subscriber_behind_the_ring_lags() {
        let events = events(4);
        let (_, mut sub) = events.subscribe("i", None, None, EventFilter::default());
        for n in 1..=10 {
            events.publish(resumed(n));
        }
        let Batch::Lagged(line) = sub.take(10) else {
            panic!("not lagged");
        };
        let note: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(note["method"], RESYNC);
        assert_eq!(note["params"]["head"], 10);
        assert_eq!(note["params"]["reason"]["type"], "lagged");
    }

    #[test]
    fn output_lines_need_the_tail() {
        let events = events(16);
        let subscribe = |filter| events.subscribe("i", None, None, filter).1;
        let mut plain = subscribe(EventFilter::default());
        let mut every = subscribe(EventFilter {
            kinds: None,
            tail: Some(Tail::default()),
        });
        let mut one = subscribe(EventFilter {
            kinds: None,
            tail: Some(Tail {
                item: Some("b".to_owned()),
            }),
        });
        events.publish(output("a"));
        events.publish(resumed(1));
        events.publish(output("b"));
        assert_eq!(seqs(plain.take(10)), [2]);
        assert_eq!(seqs(every.take(10)), [1, 2, 3]);
        assert_eq!(seqs(one.take(10)), [2, 3]);
        let mut kinds = subscribe(EventFilter {
            kinds: Some(vec!["session_output".to_owned()]),
            tail: Some(Tail::default()),
        });
        events.publish(resumed(2));
        events.publish(output("a"));
        assert_eq!(seqs(kinds.take(10)), [5]);
    }

    #[test]
    fn dropped_subscribers_leave_the_registry() {
        let events = events(4);
        let (_, a) = events.subscribe("i", None, None, EventFilter::default());
        let (_, b) = events.subscribe("i", None, None, EventFilter::default());
        assert_eq!(events.subscribers(), 2);
        drop(a);
        assert_eq!(events.subscribers(), 1);
        drop(b);
        assert_eq!(events.subscribers(), 0);
    }

    #[test]
    fn a_limit_park_is_a_transition_and_a_limit_event() {
        let item = Item::new(ItemId::new("x"));
        let ctx = Ctx::at(0);
        let mut item = step(&item, &ItemEvent::Claim, &ctx).unwrap().item;
        item = step(&item, &ItemEvent::ClaimConfirmed, &ctx).unwrap().item;
        let attempt = item.attempt.as_ref().unwrap();
        let exited = ItemEvent::Exited {
            attempt: attempt.number,
            spawn: attempt.spawn,
            exit: Exit::Limit {
                reset_ms: Some(1000),
            },
            text: String::new(),
        };
        let parked = step(&item, &exited, &ctx).unwrap();
        let bodies = transition(&item.id, &parked.transition);
        assert!(matches!(
            &bodies[0],
            EventBody::ItemTransition { to, reason: Some(r), .. } if to == "parked" && r == "limit"
        ));
        assert!(matches!(
            bodies[1],
            EventBody::LimitParked {
                until_ms: Some(1000),
                ..
            }
        ));
        assert_eq!(exit_name(Some(Exit::Crash)), "crash");
        assert_eq!(exit_name(None), "killed");
        assert_eq!(run_result_name(RunResult::Timeout), "timeout");
    }
}
