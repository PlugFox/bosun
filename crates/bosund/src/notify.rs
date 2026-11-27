//! The `notify.channel.*` methods (#9, the decision on channels): an identity's notification
//! channels, their rows in its own store and their secrets in the OS credential store.
//!
//! Every method works on the identity it names: its store's rows and the credential entries
//! `notify/<identity>/<name>`, nothing of another identity's. `add`, `remove` and `test` take
//! the operator token.
//!
//! - `add` checks the channel through [`bosun_notify::build`], writes the secret to the
//!   credential store, then the row; if the row cannot be written, the entry is deleted again.
//! - `remove` deletes the entry, then marks the row removed.
//! - `list` reads each entry only to say whether it is there, and drops the value at once.
//! - `test` sends one message now, outside any outbox.
//!
//! The [`Handle`] also holds the identity's notification drainer
//! ([`crate::notify_outbox`]), which delivers the notices its outbox queues.
//!
//! Adds and removes of one identity run one at a time ([`Handle`]), so a refused add never
//! deletes the entry of a channel another add just wrote. The secret is in no log line, error,
//! event or result. The credential store is called on the blocking pool: the OS may take its
//! time (a locked keychain, a D-Bus round trip).

use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};

use bosun_api::notify::EVENTS;
use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    ChannelAdded, ChannelRemoved, ChannelTested, ChannelView, Channels, Method, method,
};
use bosun_notify::secret::Secrets;
use bosun_notify::{Kinds, Notifier};
use bosun_store::{ChannelRow, NewChannel, Store, Write};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::api::{call, failed};
use crate::control::Control;
use crate::executor::{blocking, now_ms};
use crate::notify_outbox::{Drainer, Everyone};
use crate::state::{Served, State};

/// The notification side of one identity: its credential store and the drainer of its
/// notification outbox ([`crate::notify_outbox`]).
#[derive(Debug)]
pub(crate) struct Handle {
    secrets: Arc<dyn Secrets>,
    /// Held across an add or a remove: its entry and its row change together.
    writes: tokio::sync::Mutex<()>,
    /// The daemon's control, for the kill switch: attached once the daemon has it.
    control: Arc<OnceLock<Control>>,
    /// The drainer's task; aborted at shutdown.
    drainer: StdMutex<Option<JoinHandle<()>>>,
}

impl Handle {
    /// The OS credential store; in tests, one in memory, so no test reaches the OS's.
    pub(crate) fn new() -> Self {
        #[cfg(test)]
        let secrets: Arc<dyn Secrets> = Arc::new(bosun_notify::secret::MemorySecrets::new());
        #[cfg(not(test))]
        let secrets: Arc<dyn Secrets> = Arc::new(bosun_notify::secret::OsSecrets::new());
        Self {
            secrets,
            writes: tokio::sync::Mutex::new(()),
            control: Arc::default(),
            drainer: StdMutex::new(None),
        }
    }

    /// A handle of `identity` whose drainer runs on `store`; `store`'s reader joins
    /// `everyone`.
    pub(crate) fn started(identity: &str, store: &Store, everyone: &Everyone) -> Self {
        let handle = Self::new();
        handle.start(identity, store, everyone.clone());
        everyone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(store.reader().clone());
        handle
    }

    /// Starts the drainer of `identity`'s outbox in `store`.
    fn start(&self, identity: &str, store: &Store, everyone: Everyone) {
        let task = Drainer {
            identity: identity.to_owned(),
            reader: store.reader().clone(),
            writer: store.writer().clone(),
            secrets: self.secrets.clone(),
            control: self.control.clone(),
            everyone,
        }
        .spawn();
        if let Some(old) = self
            .drainer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(task)
        {
            old.abort();
        }
    }

    /// Gives the drainer the daemon's control: from now on the kill switch holds its sends.
    pub(crate) fn attach(&self, control: Control) {
        let _ = self.control.set(control);
    }

    /// Stops the drainer, before the store closes.
    pub(crate) async fn stop(&self) {
        let task = self
            .drainer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    /// The identity's credential store.
    #[cfg(test)]
    pub(crate) fn secrets(&self) -> &Arc<dyn Secrets> {
        &self.secrets
    }

    async fn put(&self, entry: String, secret: String) -> Result<(), ErrorObject> {
        let secrets = self.secrets.clone();
        blocking(move || secrets.put(&entry, &secret))
            .await
            .map_err(failed)
    }

    async fn get(&self, entry: String) -> Result<Option<String>, ErrorObject> {
        let secrets = self.secrets.clone();
        blocking(move || secrets.get(&entry)).await.map_err(failed)
    }

    async fn present(&self, entry: String) -> Result<bool, ErrorObject> {
        // The value is dropped here, unread.
        Ok(self.get(entry).await?.is_some())
    }

    async fn delete(&self, entry: String) -> Result<(), ErrorObject> {
        let secrets = self.secrets.clone();
        blocking(move || secrets.delete(&entry))
            .await
            .map_err(failed)
    }
}

/// The kinds a channel may have here: `fake` in tests only.
fn kinds() -> Kinds {
    Kinds { fake: cfg!(test) }
}

fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code::INVALID_PARAMS, message.into())
}

fn authorize(state: &State, operator: &str) -> Result<(), ErrorObject> {
    // Mutant (`mutant_notify_no_operator_token`): the writes take any caller.
    if cfg!(mutant_notify_no_operator_token) {
        return Ok(());
    }
    state.control.authorize(operator)
}

fn view(row: ChannelRow, secret_present: bool) -> ChannelView {
    ChannelView {
        name: row.name,
        kind: row.kind,
        params: row.params,
        events: row.events,
        min_gap_s: row.min_gap_ms / 1_000,
        secret_entry: row.secret_entry,
        secret_present,
        last_sent_ms: row.last_sent_ms,
    }
}

/// The live channel `name` of `served`.
async fn channel(served: &Served, name: &str) -> Result<ChannelRow, ErrorObject> {
    served
        .store
        .reader()
        .channels()
        .await
        .map_err(failed)?
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            invalid(format!(
                "identity {} has no notification channel named {name:?}",
                served.name
            ))
        })
}

/// The live channels of `served`, each with whether its secret is present.
async fn list(served: &Served) -> Result<Vec<ChannelView>, ErrorObject> {
    let mut views = Vec::new();
    for row in served.store.reader().channels().await.map_err(failed)? {
        let present = served.notify.present(row.secret_entry.clone()).await?;
        views.push(view(row, present));
    }
    Ok(views)
}

/// Checks the events of a new channel: at least one, each known, none twice.
fn check_events(events: &[String]) -> Result<(), ErrorObject> {
    if events.is_empty() {
        return Err(invalid(format!(
            "a channel gets at least one event of {}",
            EVENTS.join(", ")
        )));
    }
    for (i, event) in events.iter().enumerate() {
        if !EVENTS.contains(&event.as_str()) {
            return Err(invalid(format!(
                "unknown event {event:?}; the known ones: {}",
                EVENTS.join(", ")
            )));
        }
        if events[..i].contains(event) {
            return Err(invalid(format!("the event {event} is named twice")));
        }
    }
    Ok(())
}

/// The `notify.*` group.
// Long: one arm per method.
#[allow(clippy::too_many_lines)]
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::NotifyChannelAdd::NAME => {
            call::<method::NotifyChannelAdd, _, _>(p, |p| async move {
                authorize(state, &p.operator)?;
                let served = state.identity(&p.identity)?;
                if !bosun_store::valid_channel_name(&p.name) {
                    return Err(invalid(format!(
                        "{:?} is not a channel name: 1 to 32 of a-z, 0-9 and -",
                        p.name
                    )));
                }
                check_events(&p.events)?;
                let min_gap_ms = p
                    .min_gap_s
                    .checked_mul(1_000)
                    .filter(|ms| i64::try_from(*ms).is_ok())
                    .ok_or_else(|| invalid("the minimum gap is too long"))?;
                bosun_notify::build(&p.kind, &p.params, p.secret.as_deref(), kinds())
                    .map_err(invalid)?;
                let _writes = served.notify.writes.lock().await;
                if channel(served, &p.name).await.is_ok() {
                    return Err(invalid(format!(
                        "identity {} has a channel named {} already: remove it first",
                        served.name, p.name
                    )));
                }
                let entry = bosun_notify::entry(&served.name, &p.name);
                #[allow(unused_mut, reason = "the mutant below changes it")]
                let mut params = p.params.clone();
                match &p.secret {
                    Some(secret) => {
                        // Mutant (`mutant_notify_secret_in_row`): the secret goes into the row.
                        if cfg!(mutant_notify_secret_in_row) {
                            params.insert("secret".to_owned(), secret.clone());
                        }
                        served.notify.put(entry.clone(), secret.clone()).await?;
                    }
                    // A kind with no secret: no entry is left from an earlier channel.
                    None => served.notify.delete(entry.clone()).await?,
                }
                let at_ms = now_ms();
                let new = NewChannel {
                    name: p.name.clone(),
                    kind: p.kind.clone(),
                    params,
                    events: p.events.clone(),
                    min_gap_ms,
                    secret_entry: entry.clone(),
                    at_ms,
                };
                if let Err(e) = served
                    .store
                    .writer()
                    .write(vec![Write::ChannelAdded(new)])
                    .await
                {
                    if let Err(deleted) = served.notify.delete(entry.clone()).await {
                        crate::log::log!(
                            "bosund: identity {}: the entry {entry} of a channel not added \
                             stays in the credential store: {}",
                            served.name,
                            deleted.message
                        );
                    }
                    return Err(match e {
                        bosun_store::Error::Refused(why) => invalid(why),
                        other => failed(other),
                    });
                }
                crate::log::log!(
                    "bosund: identity {}: notification channel {} ({}) added",
                    served.name,
                    p.name,
                    p.kind
                );
                let row = channel(served, &p.name).await?;
                Ok(ChannelAdded {
                    identity: served.name.clone(),
                    channel: view(row, p.secret.is_some()),
                })
            })
            .await
        }
        method::NotifyChannelList::NAME => {
            call::<method::NotifyChannelList, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                // Mutant (`mutant_notify_list_every_identity`): every served identity's
                // channels are listed.
                let channels = if cfg!(mutant_notify_list_every_identity) {
                    let mut all = Vec::new();
                    for other in state.identities.values() {
                        all.extend(list(other).await?);
                    }
                    all
                } else {
                    list(served).await?
                };
                Ok(Channels {
                    identity: served.name.clone(),
                    channels,
                })
            })
            .await
        }
        method::NotifyChannelRemove::NAME => {
            call::<method::NotifyChannelRemove, _, _>(p, |p| async move {
                authorize(state, &p.operator)?;
                let served = state.identity(&p.identity)?;
                let _writes = served.notify.writes.lock().await;
                let row = channel(served, &p.name).await?;
                served.notify.delete(row.secret_entry.clone()).await?;
                served
                    .store
                    .writer()
                    .write(vec![Write::ChannelRemoved {
                        name: row.name.clone(),
                        at_ms: now_ms(),
                    }])
                    .await
                    .map_err(failed)?;
                crate::log::log!(
                    "bosund: identity {}: notification channel {} removed",
                    served.name,
                    row.name
                );
                Ok(ChannelRemoved {
                    identity: served.name.clone(),
                    name: row.name,
                })
            })
            .await
        }
        method::NotifyChannelTest::NAME => {
            call::<method::NotifyChannelTest, _, _>(p, |p| async move {
                authorize(state, &p.operator)?;
                let served = state.identity(&p.identity)?;
                let row = channel(served, &p.name).await?;
                let secret = served.notify.get(row.secret_entry.clone()).await?;
                let tested = |error: Option<String>| ChannelTested {
                    identity: served.name.clone(),
                    name: row.name.clone(),
                    accepted: error.is_none(),
                    error,
                };
                let notifier =
                    match bosun_notify::build(&row.kind, &row.params, secret.as_deref(), kinds()) {
                        Ok(notifier) => notifier,
                        Err(e) => return Ok(tested(Some(e))),
                    };
                drop(secret);
                let text = bosun_notify::text(&format!(
                    "bosun {}: test message from channel {}",
                    served.name, row.name
                ));
                let sent = blocking(move || send(&*notifier, &text)).await;
                Ok(tested(sent.err()))
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

/// Sends `text` on `notifier`; the error as a result says it.
fn send(notifier: &dyn Notifier, text: &str) -> Result<(), String> {
    notifier.send(text).map_err(|e| e.to_string())
}
