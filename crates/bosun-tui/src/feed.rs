//! What the view is fed from: one request connection and one event subscription per identity.
//!
//! The TUI keeps no state of the daemon's beyond the last answers. A watcher task per identity
//! follows its event stream ([`Subscription`], which reconnects by itself from its cursor)
//! and signals each event or resync; the run loop then refetches `status.overview` and
//! `queue.get` through [`Feed::refresh`]. A daemon restart is a broken request connection and
//! a `restarted` resync: [`Feed`] drops the broken connection and opens a new one, so the view
//! picks up the new daemon without a relaunch. While the daemon is away the view keeps what it
//! had and says it is reconnecting.
//!
//! The Session tab has a subscription of its own ([`watch_tail`]), opted into the live tail of
//! the item it follows ([`Tailing`]). Its lines go into the view as they come; a resync means
//! lines were missed, and the view is then replaced by a full `session.tail`, whose lines are
//! contiguous up to its head, so the tail shown has no gap ([`on_tail`]).

use std::time::Duration;

use bosun_api::{
    Event, EventBody, EventFilter, IdentityParams, Method, PaceParams, QueueParams, Tail,
    TailParams, method,
};
use bosun_client::actions::{self, Action, Outcome, Target};
use bosun_client::{Client, Delivery, Endpoint, Error, Subscription};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{App, Link};
use crate::session::SessionView;

/// How long one request may take before the connection counts as broken.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The event kinds that change nothing the Overview or the Queue shows; the rest each ask
/// for a refresh.
const QUIET_KINDS: &[&str] = &["session_output", "usage_delta", "ingest_progress"];

/// Something changed for an identity: refetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Changed(pub(crate) String);

/// The request side: one connection to the daemon, opened again when it breaks.
pub(crate) struct Feed {
    endpoint: Endpoint,
    client: Option<Client>,
    /// Reads the operator token at each action: the daemon writes a fresh one at every start.
    operator: fn(&Endpoint) -> Result<String, String>,
    /// A connection was opened once; the mutant `mutant_tui_no_reconnect` never opens another.
    opened: bool,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field("endpoint", &self.endpoint)
            .field("connected", &self.client.is_some())
            .finish_non_exhaustive()
    }
}

/// A failure of the connection rather than a refusal of the request.
fn broken(e: &Error) -> bool {
    !matches!(e, Error::Rpc(_) | Error::UnsupportedVersion { .. })
}

impl Feed {
    pub(crate) fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            client: None,
            operator: bosun_client::operator::read,
            opened: false,
        }
    }

    /// With another source of the operator token (tests).
    #[cfg(test)]
    pub(crate) fn with_operator(
        mut self,
        operator: fn(&Endpoint) -> Result<String, String>,
    ) -> Self {
        self.operator = operator;
        self
    }

    async fn client(&mut self) -> Result<&mut Client, Error> {
        if self.client.is_none() {
            #[cfg(mutant_tui_no_reconnect)]
            if self.opened {
                return Err(Error::Closed);
            }
            self.client = Some(Client::connect(&self.endpoint).await?);
            self.opened = true;
        }
        self.client.as_mut().ok_or(Error::Closed)
    }

    /// Calls `M`; a broken connection is dropped and the call made once more on a new one, so
    /// the first call after a daemon restart already reaches the new daemon.
    async fn call<M: Method>(&mut self, params: &M::Params) -> Result<M::Result, Error> {
        let mut retried = false;
        loop {
            let result = match self.client().await {
                Ok(client) => tokio::time::timeout(CALL_TIMEOUT, client.call::<M>(params))
                    .await
                    .unwrap_or(Err(Error::Timeout)),
                Err(e) => Err(e),
            };
            match result {
                Err(e) if broken(&e) => {
                    self.client = None;
                    if retried {
                        return Err(e);
                    }
                    retried = true;
                }
                other => return other,
            }
        }
    }

    /// Refetches every identity's Overview and Queue into `app`. `now_ms` is the local clock.
    pub(crate) async fn refresh(&mut self, app: &mut App, now_ms: i64) {
        let names: Vec<String> = app.identities.iter().map(|(n, _)| n.clone()).collect();
        for (i, name) in names.iter().enumerate() {
            let ask = IdentityParams {
                identity: name.clone(),
            };
            let overview = self.call::<method::StatusOverview>(&ask).await;
            let queue = match overview {
                Ok(_) => Some(
                    self.call::<method::QueueGet>(&QueueParams {
                        identity: name.clone(),
                        repo: None,
                    })
                    .await,
                ),
                Err(_) => None,
            };
            let view = &mut app.identities[i].1;
            match (overview, queue) {
                (Ok(o), Some(Ok(q))) => {
                    app.daemon = Some(o.daemon_version.clone());
                    view.overview = Some(o);
                    view.queue = Some(q);
                    view.fetched_ms = now_ms;
                    view.error = None;
                    app.link = Link::Up;
                }
                (Err(e), _) | (_, Some(Err(e))) if broken(&e) => {
                    app.link = Link::Down(e.to_string());
                    break;
                }
                (Err(e), _) | (_, Some(Err(e))) => {
                    view.error = Some(e.to_string());
                    app.link = Link::Up;
                }
                (Ok(_), None) => {}
            }
        }
        app.clamp();
    }

    /// Refetches every identity's `tokens.pace` into `app`, for the Tokens tab.
    pub(crate) async fn pace(&mut self, app: &mut App) {
        for i in 0..app.identities.len() {
            let ask = PaceParams {
                identity: app.identities[i].0.clone(),
            };
            let answer = self.call::<method::TokensPace>(&ask).await;
            let view = &mut app.identities[i].1;
            match answer {
                Ok(pace) => {
                    view.pace = Some(pace);
                    view.pace_error = None;
                }
                Err(e) => {
                    view.pace_error = Some(e.to_string());
                    if broken(&e) {
                        app.link = Link::Down(e.to_string());
                        return;
                    }
                }
            }
        }
    }

    /// Refetches every identity's `lease.list` into `app`, for the Resources tab.
    pub(crate) async fn resources(&mut self, app: &mut App) {
        for i in 0..app.identities.len() {
            let ask = IdentityParams {
                identity: app.identities[i].0.clone(),
            };
            let answer = self.call::<method::LeaseList>(&ask).await;
            let view = &mut app.identities[i].1;
            match answer {
                Ok(resources) => {
                    view.resources = Some(resources);
                    view.resources_error = None;
                }
                Err(e) => {
                    view.resources_error = Some(e.to_string());
                    if broken(&e) {
                        app.link = Link::Down(e.to_string());
                        return;
                    }
                }
            }
        }
    }

    /// Fetches the tail of the item the Session tab follows: in full, replacing what it shows,
    /// or from the line after the last it has.
    pub(crate) async fn tail(&mut self, app: &mut App, full: bool) {
        let Some(view) = app.session.as_ref() else {
            return;
        };
        let ask = TailParams {
            identity: view.identity.clone(),
            item: view.item.clone(),
            from_seq: (!full && view.epoch.is_some()).then_some(view.seen + 1),
        };
        let answer = self.call::<method::SessionTail>(&ask).await;
        let Some(view) = app.session.as_mut() else {
            return;
        };
        match answer {
            Ok(tail) if full => view.replace(tail),
            Ok(tail) => view.extend(tail),
            Err(e) => view.error = Some(e.to_string()),
        }
    }

    /// Performs an action the user confirmed; what to say about it.
    pub(crate) async fn perform(&mut self, action: Action, target: &Target) -> String {
        let label = action.spec().label;
        // Connect first, then read the token: the daemon writes a fresh one at every start.
        if let Err(e) = self.client().await {
            self.client = None;
            return format!("{label}: {e}");
        }
        let operator = match (self.operator)(&self.endpoint) {
            Ok(token) => token,
            Err(e) => return format!("{label}: {e}"),
        };
        let Ok(client) = self.client().await else {
            return format!("{label}: the daemon closed the connection");
        };
        let done = tokio::time::timeout(
            CALL_TIMEOUT,
            actions::perform(client, action, &operator, target),
        )
        .await
        .unwrap_or(Err(Error::Timeout));
        match done {
            Ok(outcome) => describe(&outcome, target),
            Err(e) => {
                if broken(&e) {
                    self.client = None;
                }
                format!("{label}: {e}")
            }
        }
    }
}

/// What an action's answer says, in a line.
fn describe(outcome: &Outcome, target: &Target) -> String {
    match outcome {
        Outcome::Control(status) => {
            let paused = match &target.repo {
                Some(repo) => status.repos.iter().any(|r| &r.repo == repo && r.paused),
                None => status.paused,
            };
            let what = target.repo.as_deref().unwrap_or("every repository");
            format!(
                "{what} ({}) is {}",
                status.identity,
                if paused { "paused" } else { "not paused" }
            )
        }
        Outcome::Kill(k) if k.killed => {
            "the kill switch is thrown: every child is killed, every item parks".to_owned()
        }
        Outcome::Kill(_) => "the kill switch is released".to_owned(),
        Outcome::Restart(r) => format!("{}: the `{}` label is on its card", r.item, r.label),
    }
}

/// The subscription filter of a watcher: every kind but the heavy ones, no live tail.
fn filter() -> EventFilter {
    EventFilter {
        kinds: Some(
            EventBody::KINDS
                .iter()
                .filter(|k| !QUIET_KINDS.contains(k))
                .map(|k| (*k).to_owned())
                .collect(),
        ),
        tail: None,
    }
}

/// Follows `identity`'s events and signals each event or resync on `tx`. A daemon that is not
/// there is waited for; a subscription that ends is opened again, and its opening is itself a
/// signal (events may have been missed meanwhile). Ends when `tx` is closed.
pub(crate) fn watch(
    endpoint: Endpoint,
    identity: String,
    tx: mpsc::UnboundedSender<Changed>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(100);
        loop {
            match Subscription::open(endpoint.clone(), identity.clone(), filter()).await {
                Ok(mut subscription) => {
                    backoff = Duration::from_millis(100);
                    if tx.send(Changed(identity.clone())).is_err() {
                        return;
                    }
                    // `next` reconnects by itself; an error is a refusal or a protocol fault.
                    while subscription.next().await.is_ok() {
                        if tx.send(Changed(identity.clone())).is_err() {
                            return;
                        }
                    }
                }
                Err(_) if tx.is_closed() => return,
                Err(_) => {}
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    })
}

/// What the tail watcher of an item signals.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TailSignal {
    /// The subscription is open: lines printed before it may not have been fetched.
    Opened,
    /// An event of the tail.
    Event(Event),
    /// The daemon says events were missed.
    Resync,
}

/// A [`TailSignal`] of the watcher of `item`; a signal of an item no longer followed is
/// dropped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TailChanged {
    pub(crate) identity: String,
    pub(crate) item: String,
    pub(crate) signal: TailSignal,
}

/// The tail subscription's filter: the item's output lines, and its sessions starting and
/// ending.
fn tail_filter(item: &str) -> EventFilter {
    EventFilter {
        kinds: Some(
            ["session_output", "session_started", "session_ended"]
                .map(str::to_owned)
                .to_vec(),
        ),
        tail: Some(Tail {
            item: Some(item.to_owned()),
        }),
    }
}

/// Follows the live tail of `item` and signals each line, session start and end, and resync on
/// `tx`. Like [`watch`], it waits for a daemon that is not there, and its opening is a signal.
pub(crate) fn watch_tail(
    endpoint: Endpoint,
    identity: String,
    item: String,
    tx: mpsc::UnboundedSender<TailChanged>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let send = |signal| {
            tx.send(TailChanged {
                identity: identity.clone(),
                item: item.clone(),
                signal,
            })
            .is_ok()
        };
        let mut backoff = Duration::from_millis(100);
        loop {
            match Subscription::open(endpoint.clone(), identity.clone(), tail_filter(&item)).await {
                Ok(mut subscription) => {
                    backoff = Duration::from_millis(100);
                    if !send(TailSignal::Opened) {
                        return;
                    }
                    // `next` reconnects by itself after a resync or a broken connection.
                    while let Ok(delivery) = subscription.next().await {
                        let signal = match delivery {
                            Delivery::Event(event) => TailSignal::Event(event),
                            Delivery::Resync(_) => TailSignal::Resync,
                        };
                        if !send(signal) {
                            return;
                        }
                    }
                }
                Err(_) if tx.is_closed() => return,
                Err(_) => {}
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    })
}

/// The tail watcher of the item the Session tab follows.
#[derive(Debug)]
pub(crate) struct Tailing {
    endpoint: Endpoint,
    tx: mpsc::UnboundedSender<TailChanged>,
    watcher: Option<JoinHandle<()>>,
}

impl Tailing {
    pub(crate) fn new(endpoint: Endpoint, tx: mpsc::UnboundedSender<TailChanged>) -> Self {
        Self {
            endpoint,
            tx,
            watcher: None,
        }
    }

    /// Follows the item [`App::followed`] names. When it is another item than the view's, the
    /// view starts afresh with a watcher of its own, and the tail is fetched in full. While no
    /// item is in flight the last one followed stays on screen.
    pub(crate) async fn follow(&mut self, app: &mut App, feed: &mut Feed) {
        let Some((identity, item)) = app.followed() else {
            return;
        };
        let same = app
            .session
            .as_ref()
            .is_some_and(|s| s.identity == identity && s.item == item);
        if same && self.watcher.is_some() {
            return;
        }
        self.stop();
        self.watcher = Some(watch_tail(
            self.endpoint.clone(),
            identity.clone(),
            item.clone(),
            self.tx.clone(),
        ));
        app.session = Some(SessionView::new(identity, item));
        feed.tail(app, true).await;
    }

    fn stop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

impl Drop for Tailing {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Takes one signal of the tail watcher: a line goes into the view; a session starting or
/// ending, and an opening, fetch what the view lacks; a resync replaces the view with a full
/// fetch. The mutant `mutant_tui_ignore_resync` drops the resync, which leaves a gap.
pub(crate) async fn on_tail(app: &mut App, feed: &mut Feed, changed: TailChanged) {
    let Some(view) = app.session.as_mut() else {
        return;
    };
    if view.identity != changed.identity || view.item != changed.item {
        return;
    }
    match changed.signal {
        TailSignal::Event(event) => match event.body {
            EventBody::SessionOutput { .. } => view.push(event),
            _ => feed.tail(app, false).await,
        },
        TailSignal::Opened => feed.tail(app, false).await,
        TailSignal::Resync => {
            if cfg!(mutant_tui_ignore_resync) {
                return;
            }
            view.resyncs += 1;
            feed.tail(app, true).await;
        }
    }
}

#[cfg(test)]
#[path = "feed_tests.rs"]
pub(crate) mod tests;
