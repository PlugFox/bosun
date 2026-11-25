//! The daemon's lease manager (#8, "Leases"; E1, E15, E16): one per daemon, shared by every
//! repository's loop, over the pure model of [`bosun_core::Leases`].
//!
//! - The resources are `config.resources`, read when the manager is built. A resource is
//!   instantiated by its scope: a `machine` resource is one instance named `<name>`; an
//!   `identity` resource one per identity, `<name>/<identity>`; a `repository` resource one per
//!   repository, `<name>/<repo>`. A request names the plain names and gets the instances of its
//!   own identity and repository.
//! - A holder is an item or a `bosun with` command ([`Holder`]), never a shim (E16).
//! - [`Leases::acquire`] files a request and resolves when the model grants it: all or nothing,
//!   in one global order, first come first served. Dropping its future withdraws the request.
//!   A holder that already holds exactly that set is granted at once (a later session of the
//!   same item, a recovered item asking again).
//! - The live leases are journalled after every change to [`JOURNAL`] in the daemon's own
//!   directory, by writing a temporary file and renaming it over the journal (E15). Each entry
//!   names the daemon that recorded it (pid and start time) and, while one of the item's
//!   sessions runs under the session shim, that shim ([`Leases::set_shim`]).
//! - A lease whose holder is gone is released when the manager is built and on every tick of a
//!   loop ([`Leases::release_stale`]): an item's lease is stale only when neither the daemon
//!   that recorded it nor its shim still runs (E16); a command's while its process runs.
//! - The wait of a granted request is written to the requesting repository's store, one
//!   `lease_wait` row per resource: filed, granted, and released with the holder (E15: the
//!   per-identity databases keep only the wait history).
//! - A `bosun with` command leases through the API ([`dispatch`], `lease.acquire` and
//!   `lease.release` under the operator token): it polls its request
//!   ([`Leases::acquire_command`]) instead of awaiting it, its wait goes to the store of the
//!   identity it names with no item, and its lease and request end with its process.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{AcquireResult, Method, ReleaseResult, method};
use bosun_config::Config;
use bosun_core::{ItemId, Scope};
use bosun_proc::{ProcessKey, StartTime};
use bosun_store::{Write, Writer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::api::{call, failed};
use crate::executor::now_ms;
use crate::state::State;

/// The journal's file name in the daemon's directory.
pub(crate) const JOURNAL: &str = "leases.json";

/// Who holds a lease: an item, or a `bosun with` command (E16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Holder {
    /// An item, from its first session through its landing re-verdict.
    Item(ItemId),
    /// A `bosun with` process.
    Command(ProcessKey),
}

impl Holder {
    /// The holder as the store and the log name it: `item demo#1`, `command 1234`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Item(item) => format!("item {item}"),
            Self::Command(key) => format!("command {}", key.pid),
        }
    }

    fn item(&self) -> Option<ItemId> {
        match self {
            Self::Item(item) => Some(item.clone()),
            Self::Command(_) => None,
        }
    }
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// What a request is for, and where its wait is written.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) holder: Holder,
    /// The resources by their plain names, as a card's `needs` lists them.
    pub(crate) names: Vec<String>,
    /// The requesting repository: its instances of `repository` resources.
    pub(crate) repo: String,
    /// The repository's identity: its instances of `identity` resources.
    pub(crate) identity: String,
    /// The requesting repository's store; `None`: the wait is not recorded.
    pub(crate) writer: Option<Writer>,
}

/// A live lease: what the journal keeps of it.
#[derive(Debug, Clone)]
struct Entry {
    holder: Holder,
    /// The resource instances, in the global order.
    resources: Vec<String>,
    /// The daemon that recorded it.
    daemon: ProcessKey,
    /// The shim of the item's running session, in worktree mode ([`Leases::set_shim`]).
    shim: Option<ProcessKey>,
    granted_ms: i64,
    /// Where its release is written; `None` for an entry loaded from the journal.
    writer: Option<Writer>,
}

/// A request the model has not granted yet.
#[derive(Debug)]
struct Waiter {
    holder: Holder,
    resources: Vec<String>,
    writer: Option<Writer>,
    grant: oneshot::Sender<i64>,
}

#[derive(Debug)]
struct Inner {
    model: bosun_core::Leases<Holder>,
    granted: Vec<Entry>,
    waiting: Vec<Waiter>,
}

/// The lease manager of one daemon.
#[derive(Debug)]
pub(crate) struct Leases {
    inner: Mutex<Inner>,
    /// `[resource.<name>]`: each resource's scope, by name.
    scopes: std::collections::BTreeMap<String, Scope>,
    /// The journal file.
    journal: PathBuf,
    /// This daemon's process.
    daemon: ProcessKey,
}

/// A journal entry as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stored {
    pub(crate) holder: StoredHolder,
    pub(crate) resources: Vec<String>,
    pub(crate) daemon: StoredKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shim: Option<StoredKey>,
    pub(crate) granted_ms: i64,
}

/// A holder as the journal stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredHolder {
    Item(String),
    Command(StoredKey),
}

/// A process key as the journal stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredKey {
    pub(crate) pid: u32,
    pub(crate) start: u64,
}

impl From<ProcessKey> for StoredKey {
    fn from(key: ProcessKey) -> Self {
        Self {
            pid: key.pid,
            start: key.start.raw(),
        }
    }
}

impl From<StoredKey> for ProcessKey {
    fn from(key: StoredKey) -> Self {
        Self {
            pid: key.pid,
            start: StartTime::from_raw(key.start),
        }
    }
}

/// The journal file's contents.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Journal {
    leases: Vec<Stored>,
}

/// This process's key.
fn own_key() -> ProcessKey {
    let pid = std::process::id();
    let start = bosun_proc::start_time(pid)
        .ok()
        .flatten()
        .unwrap_or(StartTime::from_raw(0));
    ProcessKey { pid, start }
}

impl Leases {
    /// The manager of the resources of `config`, journalled in `dir`: the journal found there
    /// is loaded, and an entry is kept only while its holder lives ([`Leases::release_stale`]).
    ///
    /// # Errors
    ///
    /// The resources do not make a lease table (a duplicate instance, a capacity of 0).
    pub(crate) fn new(config: &Config, dir: &Path) -> Result<Self, String> {
        let mut instances = Vec::new();
        let mut scopes = std::collections::BTreeMap::new();
        for (name, resource) in &config.resources {
            let capacity = usize::try_from(resource.capacity).unwrap_or(usize::MAX);
            scopes.insert(name.clone(), resource.scope);
            let suffixes: Vec<Option<&String>> = match resource.scope {
                Scope::Machine => vec![None],
                Scope::Identity => config.identities.keys().map(Some).collect(),
                Scope::Repository => config.repos.keys().map(Some).collect(),
            };
            for suffix in suffixes {
                let instance = suffix.map_or_else(|| name.clone(), |s| format!("{name}/{s}"));
                instances.push(bosun_core::Resource::new(
                    instance,
                    capacity,
                    resource.scope,
                ));
            }
        }
        let model = bosun_core::Leases::new(instances).map_err(|e| e.to_string())?;
        let this = Self {
            inner: Mutex::new(Inner {
                model,
                granted: Vec::new(),
                waiting: Vec::new(),
            }),
            scopes,
            journal: dir.join(JOURNAL),
            daemon: own_key(),
        };
        this.load();
        Ok(this)
    }

    /// A manager of no resource: it grants nothing, so its journal is never written.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            inner: Mutex::new(Inner {
                model: bosun_core::Leases::new(Vec::new()).expect("an empty table is valid"),
                granted: Vec::new(),
                waiting: Vec::new(),
            }),
            scopes: std::collections::BTreeMap::new(),
            journal: PathBuf::new(),
            daemon: own_key(),
        }
    }

    /// The names of the resources, as `needs` lists them.
    pub(crate) fn names(&self) -> BTreeSet<String> {
        self.scopes.keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads the journal: each entry whose holder lives is held again; the rest are released.
    fn load(&self) {
        let text = match std::fs::read_to_string(&self.journal) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                crate::log::log!("bosund: the lease journal {}: {e}", self.journal.display());
                return;
            }
        };
        let journal: Journal = match serde_json::from_str(&text) {
            Ok(journal) => journal,
            Err(e) => {
                crate::log::log!(
                    "bosund: the lease journal {} cannot be read, its leases are dropped: {e}",
                    self.journal.display()
                );
                Journal::default()
            }
        };
        let now = now_ms();
        let mut inner = self.lock();
        for stored in journal.leases {
            let holder = match &stored.holder {
                StoredHolder::Item(item) => Holder::Item(ItemId::new(item.clone())),
                StoredHolder::Command(key) => Holder::Command((*key).into()),
            };
            let entry = Entry {
                holder: holder.clone(),
                resources: stored.resources.clone(),
                daemon: stored.daemon.into(),
                shim: stored.shim.map(Into::into),
                granted_ms: stored.granted_ms,
                writer: None,
            };
            // Mutant (`mutant_loop_keep_stale_leases`): every entry loaded is kept.
            if !cfg!(mutant_loop_keep_stale_leases) && !self.lives(&entry) {
                crate::log::log!(
                    "bosund: the lease of {holder} on {} is released: its holder is gone",
                    stored.resources.join(", ")
                );
                continue;
            }
            let taken = inner
                .model
                .acquire(holder.clone(), &stored.resources, now)
                .is_ok()
                && {
                    inner.model.serve(now);
                    inner.model.is_granted(&holder)
                };
            if taken {
                inner.granted.push(entry);
            } else {
                inner.model.release(&holder);
                crate::log::log!(
                    "bosund: the journalled lease of {holder} on {} cannot be held again; \
                     it is dropped",
                    stored.resources.join(", ")
                );
            }
        }
        self.save(&inner);
    }

    /// Whether `entry`'s holder still lives (E16).
    fn lives(&self, entry: &Entry) -> bool {
        match &entry.holder {
            Holder::Command(key) => bosun_proc::is_running(*key),
            Holder::Item(_) => {
                entry.daemon == self.daemon
                    || bosun_proc::is_running(entry.daemon)
                    || entry.shim.is_some_and(bosun_proc::is_running)
            }
        }
    }

    /// The instances `request` names, in the global order.
    fn instances(&self, request: &Request) -> Result<Vec<String>, String> {
        let mut out = BTreeSet::new();
        for name in &request.names {
            let scope = self
                .scopes
                .get(name)
                .ok_or_else(|| format!("unknown resource `{name}`"))?;
            out.insert(match scope {
                Scope::Machine => name.clone(),
                Scope::Identity => format!("{name}/{}", request.identity),
                Scope::Repository => format!("{name}/{}", request.repo),
            });
        }
        Ok(out.into_iter().collect())
    }

    /// Files `request` and resolves when it is granted. Dropping the future withdraws it.
    ///
    /// # Errors
    ///
    /// An unknown resource (one added to the configuration after the daemon started), or a
    /// holder that holds or waits for another set.
    pub(crate) async fn acquire(&self, request: Request) -> Result<(), String> {
        let Some(filed) = self.file(&request)? else {
            return Ok(());
        };
        let mut guard = Withdraw {
            leases: self,
            holder: Some(request.holder.clone()),
        };
        let granted = filed.granted.await;
        guard.holder = None;
        record(&request, &filed.resources, filed.filed_ms, granted).await
    }

    /// Files `request` under the lock: `None` when its holder already holds exactly that set,
    /// else what its grant is awaited on.
    fn file(&self, request: &Request) -> Result<Option<Pending>, String> {
        let resources = self.instances(request)?;
        let holder = request.holder.clone();
        let (grant, granted) = oneshot::channel();
        let filed_ms = now_ms();
        let mut inner = self.lock();
        if let Some(entry) = inner.granted.iter().find(|e| e.holder == holder) {
            if entry.resources == resources {
                return Ok(None);
            }
            return Err(format!(
                "{holder} holds {} and asks for {}",
                entry.resources.join(", "),
                resources.join(", ")
            ));
        }
        inner
            .model
            .acquire(holder.clone(), &resources, filed_ms)
            .map_err(|e| format!("{holder}: {e}"))?;
        inner.waiting.push(Waiter {
            holder,
            resources: resources.clone(),
            writer: request.writer.clone(),
            grant,
        });
        self.serve(&mut inner);
        Ok(Some(Pending {
            resources,
            filed_ms,
            granted,
        }))
    }

    /// A `bosun with` command's request, polled: the first call files it, a later one with the
    /// same set only says whether it is granted (`true`). The wait is recorded when it is
    /// granted; the request lasts until [`Leases::release`] or until its process is gone
    /// ([`Leases::release_stale`]).
    ///
    /// # Errors
    ///
    /// An unknown resource, or a holder that holds or waits for another set.
    pub(crate) fn acquire_command(self: &Arc<Self>, request: Request) -> Result<bool, String> {
        let resources = self.instances(&request)?;
        let holder = request.holder.clone();
        {
            let inner = self.lock();
            let held = inner
                .granted
                .iter()
                .find(|e| e.holder == holder)
                .map(|e| (true, &e.resources));
            let waits = inner
                .waiting
                .iter()
                .find(|w| w.holder == holder)
                .map(|w| (false, &w.resources));
            if let Some((granted, asked)) = held.or(waits) {
                if *asked == resources {
                    return Ok(granted);
                }
                return Err(format!(
                    "{holder} {} {} and asks for {}",
                    if granted { "holds" } else { "waits for" },
                    asked.join(", "),
                    resources.join(", ")
                ));
            }
        }
        let Some(filed) = self.file(&request)? else {
            return Ok(true);
        };
        let granted_now = self.lock().granted.iter().any(|e| e.holder == holder);
        // The grant is awaited apart, as the command polls; a withdrawn request ends the task.
        tokio::spawn(async move {
            let granted = filed.granted.await;
            if granted.is_ok()
                && let Err(e) = record(&request, &filed.resources, filed.filed_ms, granted).await
            {
                crate::log::log!("bosund: {e}");
            }
        });
        Ok(granted_now)
    }

    /// Of `names`: those no `[resource.<name>]` configures, and whether one of the others is
    /// of the repository scope.
    pub(crate) fn check(&self, names: &[String]) -> (Vec<String>, bool) {
        let unknown = names
            .iter()
            .filter(|n| !self.scopes.contains_key(*n))
            .cloned()
            .collect();
        let repository = names
            .iter()
            .any(|n| self.scopes.get(n) == Some(&Scope::Repository));
        (unknown, repository)
    }

    /// Runs the model's attempts, records what it granted and tells their waiters; journals a
    /// change.
    fn serve(&self, inner: &mut Inner) {
        let now = now_ms();
        let granted = inner.model.serve(now);
        if granted.is_empty() {
            return;
        }
        for holder in granted {
            let Some(at) = inner.waiting.iter().position(|w| w.holder == holder) else {
                continue;
            };
            let waiter = inner.waiting.remove(at);
            inner.granted.push(Entry {
                holder,
                resources: waiter.resources,
                daemon: self.daemon,
                shim: None,
                granted_ms: now,
                writer: waiter.writer,
            });
            let _ = waiter.grant.send(now);
        }
        self.save(inner);
    }

    /// Releases what `holder` holds and withdraws its request, under the lock; returns the
    /// released entry.
    fn release_locked(&self, inner: &mut Inner, holder: &Holder) -> Option<Entry> {
        if !inner.model.release(holder) {
            return None;
        }
        inner.waiting.retain(|w| &w.holder != holder);
        let entry = inner
            .granted
            .iter()
            .position(|e| &e.holder == holder)
            .map(|at| inner.granted.remove(at));
        self.save(inner);
        self.serve(inner);
        entry
    }

    /// Releases what `holder` holds, withdraws its request, and records the release; whether
    /// it held or waited for anything.
    pub(crate) async fn release(&self, holder: &Holder) -> bool {
        let (known, entry) = {
            let mut inner = self.lock();
            let known = inner.granted.iter().any(|e| &e.holder == holder)
                || inner.waiting.iter().any(|w| &w.holder == holder);
            (known, self.release_locked(&mut inner, holder))
        };
        if let Some(Entry {
            writer: Some(writer),
            ..
        }) = entry
        {
            let write = Write::LeaseReleased {
                holder: holder.label(),
                at_ms: now_ms(),
            };
            if let Err(e) = writer.write(vec![write]).await {
                crate::log::log!("bosund: the release of {holder}'s lease cannot be stored: {e}");
            }
        }
        known
    }

    /// Records `shim` as the session shim of `holder`'s lease (E16: the lease lives while the
    /// shim does, whatever became of the daemon), or clears it (`None`); journalled. Nothing
    /// happens when `holder` holds no lease.
    pub(crate) fn set_shim(&self, holder: &Holder, shim: Option<ProcessKey>) {
        let mut inner = self.lock();
        let Some(entry) = inner.granted.iter_mut().find(|e| &e.holder == holder) else {
            return;
        };
        if entry.shim == shim {
            return;
        }
        entry.shim = shim;
        self.save(&inner);
    }

    /// Releases every lease whose holder is gone (E16), and withdraws the request of a
    /// command whose process is gone; returns those holders.
    pub(crate) fn release_stale(&self) -> Vec<Holder> {
        let mut inner = self.lock();
        let mut dead: Vec<Holder> = inner
            .granted
            .iter()
            .filter(|e| !self.lives(e))
            .map(|e| e.holder.clone())
            .collect();
        dead.extend(
            inner
                .waiting
                .iter()
                .filter(
                    |w| matches!(w.holder, Holder::Command(key) if !bosun_proc::is_running(key)),
                )
                .map(|w| w.holder.clone()),
        );
        for holder in &dead {
            crate::log::log!("bosund: the lease of {holder} is released: its holder is gone");
            self.release_locked(&mut inner, holder);
        }
        dead
    }

    /// The resource instances `holder` holds by a granted lease.
    #[cfg(test)]
    pub(crate) fn holds(&self, holder: &Holder) -> Vec<String> {
        self.lock()
            .granted
            .iter()
            .find(|e| &e.holder == holder)
            .map_or_default(|e| e.resources.clone())
    }

    /// `holder` has a request waiting.
    #[cfg(test)]
    pub(crate) fn is_waiting(&self, holder: &Holder) -> bool {
        self.lock().waiting.iter().any(|w| &w.holder == holder)
    }

    /// The journal file.
    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Path {
        &self.journal
    }

    /// Writes the live leases to the journal: a temporary file renamed over it.
    fn save(&self, inner: &Inner) {
        let journal = Journal {
            leases: inner
                .granted
                .iter()
                .map(|e| Stored {
                    holder: match &e.holder {
                        Holder::Item(item) => StoredHolder::Item(item.0.clone()),
                        Holder::Command(key) => StoredHolder::Command((*key).into()),
                    },
                    resources: e.resources.clone(),
                    daemon: e.daemon.into(),
                    shim: e.shim.map(Into::into),
                    granted_ms: e.granted_ms,
                })
                .collect(),
        };
        if let Err(e) = write_journal(&self.journal, &journal) {
            crate::log::log!(
                "bosund: the lease journal {} cannot be written: {e}",
                self.journal.display()
            );
        }
    }
}

fn write_journal(path: &Path, journal: &Journal) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(journal).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.saving");
    std::fs::write(&temp, text)?;
    // On Windows a rename over a file being read is refused for a moment.
    let mut tries = 0;
    loop {
        match std::fs::rename(&temp, path) {
            Ok(()) => return Ok(()),
            Err(_) if tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// The holder of the running process `pid`; refused when it is not running.
fn command(pid: u32) -> Result<Holder, ErrorObject> {
    match bosun_proc::start_time(pid) {
        Ok(Some(start)) => Ok(Holder::Command(ProcessKey { pid, start })),
        _ => Err(ErrorObject::new(
            code::INVALID_PARAMS,
            format!("no process {pid} is running"),
        )),
    }
}

/// The `lease.*` methods (#8): a `bosun with` command's leases. The operator token is checked
/// before anything else; then the leases of every process that is gone are released, so a
/// command that died does not block the next one until a loop ticks.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    let leases = &state.leases;
    match name {
        method::LeaseAcquire::NAME => {
            call::<method::LeaseAcquire, _, _>(p, |p| async move {
                if !cfg!(mutant_api_lease_without_operator) {
                    // Mutant (`mutant_api_lease_without_operator`): the token is not checked.
                    state.control.authorize(&p.operator)?;
                }
                leases.release_stale();
                let served = state.identity(&p.identity)?;
                let (unknown, repository) = leases.check(&p.resources);
                let refuse = |message: String| ErrorObject::new(code::INVALID_PARAMS, message);
                if p.resources.is_empty() {
                    return Err(refuse("no resource is named".to_owned()));
                }
                if !unknown.is_empty() {
                    return Err(refuse(format!("no resource named {}", unknown.join(", "))));
                }
                if let Some(repo) = &p.repo
                    && !served.repos.iter().any(|(n, _)| n == repo)
                {
                    return Err(refuse(format!(
                        "identity {} has no repository named {repo:?}",
                        served.name
                    )));
                }
                if repository && p.repo.is_none() {
                    return Err(refuse(
                        "a repository-scope resource needs the repository (`repo`)".to_owned(),
                    ));
                }
                let holder = command(p.pid)?;
                if cfg!(mutant_api_lease_acquire_always_grants) {
                    // Mutant (`mutant_api_lease_acquire_always_grants`): the manager is not asked.
                    return Ok(AcquireResult { granted: true });
                }
                let request = Request {
                    holder,
                    names: p.resources.clone(),
                    repo: p.repo.clone().unwrap_or_default(),
                    identity: served.name.clone(),
                    writer: Some(served.store.writer().clone()),
                };
                let granted = leases.acquire_command(request).map_err(failed)?;
                Ok(AcquireResult { granted })
            })
            .await
        }
        method::LeaseRelease::NAME => {
            call::<method::LeaseRelease, _, _>(p, |p| async move {
                if !cfg!(mutant_api_lease_without_operator) {
                    state.control.authorize(&p.operator)?;
                }
                leases.release_stale();
                // A process that is gone was released above.
                let released = match command(p.pid) {
                    Ok(holder) => leases.release(&holder).await,
                    Err(_) => false,
                };
                Ok(ReleaseResult { released })
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

/// A request filed and not granted yet.
struct Pending {
    /// Its resource instances, in the global order.
    resources: Vec<String>,
    filed_ms: i64,
    /// Resolves with the grant's time; fails when the request is withdrawn.
    granted: oneshot::Receiver<i64>,
}

/// Writes the wait of `request`, granted (or withdrawn) as `granted` says, to its store: one
/// row per resource.
async fn record(
    request: &Request,
    resources: &[String],
    filed_ms: i64,
    granted: Result<i64, oneshot::error::RecvError>,
) -> Result<(), String> {
    let holder = &request.holder;
    let granted_ms = granted.map_err(|_| format!("the request of {holder} was dropped"))?;
    if let Some(writer) = &request.writer {
        let label = holder.label();
        let mut writes = Vec::new();
        for resource in resources {
            writes.push(Write::LeaseFiled {
                item: holder.item(),
                holder: label.clone(),
                resource: resource.clone(),
                filed_ms,
            });
            writes.push(Write::LeaseGranted {
                holder: label.clone(),
                resource: resource.clone(),
                filed_ms,
                at_ms: granted_ms,
            });
        }
        if let Err(e) = writer.write(writes).await {
            crate::log::log!("bosund: the lease wait of {holder} cannot be stored: {e}");
        }
    }
    Ok(())
}

/// Withdraws a request whose `acquire` future was dropped before it was granted.
struct Withdraw<'a> {
    leases: &'a Leases,
    holder: Option<Holder>,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        if let Some(holder) = self.holder.take() {
            let mut inner = self.leases.lock();
            self.leases.release_locked(&mut inner, &holder);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    const NONE: [&str; 0] = [];

    /// A configuration of one identity and one repository with `resources` (TOML).
    fn config(dir: &Path, resources: &str) -> Config {
        for sub in ["claude", "db", "gh", "repo"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[identity.test]\n\
                 claude_config_dir = '{claude}'\n\
                 database = '{db}'\n\
                 github_login = 'synthetic-login'\n\
                 gh_config_dir = '{gh}'\n\n\
                 [repo.demo]\n\
                 path = '{repo}'\n\
                 identity = 'test'\n\n\
                 {resources}",
                claude = dir.join("claude").display(),
                db = dir.join("db").join("bosun.db").display(),
                gh = dir.join("gh").display(),
                repo = dir.join("repo").display(),
            ),
        )
        .unwrap();
        bosun_config::load(&path, &bosun_config::SystemProbe::new()).unwrap()
    }

    fn request(holder: &Holder, names: &[&str]) -> Request {
        Request {
            holder: holder.clone(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            repo: "demo".to_owned(),
            identity: "test".to_owned(),
            writer: None,
        }
    }

    /// The journal's entries as (holder label, resources).
    fn journalled(path: &Path) -> Vec<(String, Vec<String>)> {
        let journal: Journal =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        journal
            .leases
            .into_iter()
            .map(|s| {
                let holder = match s.holder {
                    StoredHolder::Item(item) => format!("item {item}"),
                    StoredHolder::Command(key) => format!("command {}", key.pid),
                };
                (holder, s.resources)
            })
            .collect()
    }

    fn entry(label: &str, resources: &[&str]) -> (String, Vec<String>) {
        (
            label.to_owned(),
            resources.iter().map(|r| (*r).to_owned()).collect(),
        )
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_waits_for_its_whole_set_and_is_served_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            dir.path(),
            "[resource.a]\nkind = 'device'\n\n[resource.b]\nkind = 'device'\n",
        );
        let state = dir.path().join("state");
        let leases = Arc::new(Leases::new(&config, &state).unwrap());
        let journal = leases.journal().to_owned();
        let (first, second, third, fourth) = (
            Holder::Item(ItemId::new("demo#1")),
            Holder::Item(ItemId::new("demo#2")),
            Holder::Item(ItemId::new("demo#3")),
            Holder::Item(ItemId::new("demo#4")),
        );

        leases.acquire(request(&first, &["a"])).await.unwrap();
        assert_eq!(leases.holds(&first), ["a"]);
        assert_eq!(journalled(&journal), [entry("item demo#1", &["a"])]);

        // The second asks for both: `b` is free, `a` is not, and it holds neither meanwhile.
        let wait_second = tokio::spawn({
            let (leases, second) = (leases.clone(), second.clone());
            async move { leases.acquire(request(&second, &["b", "a"])).await }
        });
        settle().await;
        assert!(leases.is_waiting(&second));
        assert_eq!(leases.holds(&second), NONE);
        assert_eq!(journalled(&journal), [entry("item demo#1", &["a"])]);

        // The third asks for `a` after the second: it queues behind it.
        let wait_third = tokio::spawn({
            let (leases, third) = (leases.clone(), third.clone());
            async move { leases.acquire(request(&third, &["a"])).await }
        });
        settle().await;
        assert!(leases.is_waiting(&third));

        // A fourth asks for `b` and gives up: its request is withdrawn with its future.
        {
            let gave_up = tokio::time::timeout(
                Duration::from_millis(100),
                leases.acquire(request(&fourth, &["b"])),
            )
            .await;
            assert!(gave_up.is_err(), "the fourth was granted: {gave_up:?}");
        }
        assert!(!leases.is_waiting(&fourth));
        assert_eq!(leases.holds(&fourth), NONE);

        // The first releases: the second gets both; the third still waits.
        leases.release(&first).await;
        wait_second.await.unwrap().unwrap();
        assert_eq!(leases.holds(&second), ["a", "b"]);
        assert!(leases.is_waiting(&third));
        assert_eq!(journalled(&journal), [entry("item demo#2", &["a", "b"])]);

        // Asking again for the same set is granted at once.
        leases.acquire(request(&second, &["a", "b"])).await.unwrap();

        leases.release(&second).await;
        wait_third.await.unwrap().unwrap();
        assert_eq!(leases.holds(&third), ["a"]);
        assert_eq!(journalled(&journal), [entry("item demo#3", &["a"])]);

        leases.release(&third).await;
        assert_eq!(journalled(&journal), []);
    }

    #[tokio::test]
    async fn a_lease_whose_owners_are_gone_is_released_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            dir.path(),
            "[resource.a]\nkind = 'device'\n\n[resource.b]\nkind = 'device'\n\n\
             [resource.c]\nkind = 'device'\n",
        );
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let pid = std::process::id();
        let start = bosun_proc::start_time(pid).unwrap().unwrap().raw();
        let me = StoredKey { pid, start };
        // This pid with another start time: a process that is gone.
        let gone = StoredKey {
            pid,
            start: start.wrapping_add(1),
        };
        let journal = Journal {
            leases: vec![
                Stored {
                    holder: StoredHolder::Item("demo#1".to_owned()),
                    resources: vec!["a".to_owned()],
                    daemon: gone,
                    shim: None,
                    granted_ms: 1,
                },
                Stored {
                    holder: StoredHolder::Item("demo#2".to_owned()),
                    resources: vec!["b".to_owned()],
                    daemon: gone,
                    shim: Some(me),
                    granted_ms: 2,
                },
                Stored {
                    holder: StoredHolder::Command(gone),
                    resources: vec!["c".to_owned()],
                    daemon: gone,
                    shim: None,
                    granted_ms: 3,
                },
            ],
        };
        write_journal(&state.join(JOURNAL), &journal).unwrap();

        let leases = Leases::new(&config, &state).unwrap();
        assert_eq!(leases.holds(&Holder::Item(ItemId::new("demo#1"))), NONE);
        assert_eq!(leases.holds(&Holder::Item(ItemId::new("demo#2"))), ["b"]);
        assert_eq!(leases.holds(&Holder::Command(gone.into())), NONE);
        assert_eq!(journalled(leases.journal()), [entry("item demo#2", &["b"])]);
        // What was released is free: another holder gets it at once.
        let other = Holder::Item(ItemId::new("demo#3"));
        tokio::time::timeout(
            Duration::from_secs(5),
            leases.acquire(request(&other, &["a", "c"])),
        )
        .await
        .unwrap()
        .unwrap();
    }
}
