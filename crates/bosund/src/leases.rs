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
//! - An item lets go of its leases while it waits (E24): Parked on a rate limit, or with its
//!   landing's fast-forward deferred. It takes them again, all or nothing, before its next
//!   session, verdict or landing re-verdict; each grant is a new `lease_wait` row per resource,
//!   and each release closes the holder's open rows.
//! - The live leases are journalled after every change, by writing a temporary file, syncing
//!   it and renaming it over the journal (E15). The journal is per daemon (E25), named after
//!   its operator token ([`journal_path`]): two daemons of one user share nothing, and the
//!   `machine` scope means one daemon's every identity and repository. Each entry names the
//!   daemon that recorded it (pid and start time) and, while one of the item's sessions runs
//!   under the session shim, that shim ([`Leases::set_shim`]).
//! - When the manager is built, an entry is kept only if its shim or its command's process
//!   still runs (E25): the entries of another daemon that still runs are never adopted. A
//!   journal that does not parse is moved to `<journal>.corrupt`, never overwritten.
//! - At run time a lease whose holder is gone is released on every tick of a loop
//!   ([`Leases::release_stale`]): an item's lease is stale only when neither this daemon
//!   recorded it nor its shim still runs (E16); a command's when its process is gone. A holder
//!   that asks again for the set it holds (an item re-attached after a restart) is re-stamped
//!   with this daemon and its store, so its lease outlives its session's shim.
//! - The wait of a granted request is written to the requesting repository's store, one
//!   `lease_wait` row per resource: filed, granted, and released with the holder (E15: the
//!   per-identity databases keep only the wait history).
//! - A `bosun with` command leases through the API ([`dispatch`], `lease.acquire` and
//!   `lease.release` under the operator token): it polls its request
//!   ([`Leases::acquire_command`]) instead of awaiting it, its wait goes to the store of the
//!   identity it names with no item, and its lease and request end with its process.
//! - A resource with a `probe` is probed before each claim of a card that needs it (#8): the
//!   manager keeps the health of each instance as each identity probed it
//!   ([`crate::health`]), in memory, under its lock. It is not journalled: a restarted daemon
//!   probes again before its first claim.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{AcquireResult, Method, ReleaseResult, WaitingFor, method};
use bosun_config::Config;
use bosun_core::{ItemId, Scope};
use bosun_proc::{ProcessKey, StartTime};
use bosun_store::{Write, Writer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::api::{call, failed};
use crate::executor::now_ms;
use crate::health::{Check, Health, Key, Recorded, Streak};
use crate::state::{Served, State};

/// The lease journal of the daemon whose operator token is `operator_file` (E25): the token's
/// name with its extension replaced by `leases.json`, in the token's directory. The token is
/// per endpoint, its directory is not (every daemon of a user may share it), so two daemons
/// never share a journal: `bosun.token` -> `bosun.leases.json`, `bosun.sock.operator` ->
/// `bosun.sock.leases.json`.
pub(crate) fn journal_path(operator_file: &Path) -> PathBuf {
    if cfg!(mutant_leases_one_journal_per_directory) {
        // Mutant (`mutant_leases_one_journal_per_directory`): one journal per directory.
        return operator_file.with_file_name("leases.json");
    }
    operator_file.with_extension("leases.json")
}

/// Who holds a lease: an item, or a `bosun with` command (E16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Holder {
    /// An item, from its claim through its teardown, except while it is Parked or its
    /// landing's fast-forward is deferred (E24).
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
    /// The identity that asked for it: the only one shown who holds it ([`Leases::seen_by`]).
    /// `None` for an entry of a journal written before it was recorded: `busy` to everyone.
    identity: Option<String>,
}

/// A request the model has not granted yet.
#[derive(Debug)]
struct Waiter {
    holder: Holder,
    resources: Vec<String>,
    writer: Option<Writer>,
    /// The identity that asked.
    identity: String,
    grant: oneshot::Sender<i64>,
}

#[derive(Debug)]
struct Inner {
    model: bosun_core::Leases<Holder>,
    granted: Vec<Entry>,
    waiting: Vec<Waiter>,
    /// The probed instances' health, by instance and identity (#2: each identity its own).
    health: Health,
}

/// The lease manager of one daemon.
#[derive(Debug)]
pub(crate) struct Leases {
    inner: Mutex<Inner>,
    /// `[resource.<name>]`: each resource's scope, by name.
    scopes: std::collections::BTreeMap<String, Scope>,
    /// Every resource instance, in the global order.
    instances: Vec<Instance>,
    /// The journal file.
    journal: PathBuf,
    /// This daemon's process.
    daemon: ProcessKey,
}

/// One resource instance of the table.
#[derive(Debug, Clone)]
struct Instance {
    /// `<name>`, `<name>/<identity>` or `<name>/<repo>`.
    name: String,
    scope: Scope,
    /// `[resource.<name>] kind`.
    kind: String,
    capacity: usize,
    /// The identity or repository it is of; `None` for the machine scope.
    of: Option<String>,
    /// `[resource.<name>] probe` and `probe_timeout`.
    probe: Option<(String, Duration)>,
}

/// A probed resource instance a card would lease ([`Leases::probed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probed {
    /// The instance: `<name>`, `<name>/<identity>` or `<name>/<repo>`.
    pub(crate) instance: String,
    /// Its probe command.
    pub(crate) command: String,
    /// How long the probe may run.
    pub(crate) timeout: Duration,
}

/// The resource instances one identity can lease and who holds them, as it may see them
/// ([`Leases::seen_by`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    /// The machine-scope instances, the identity's own and its repositories'.
    pub(crate) resources: Vec<bosun_api::ResourceView>,
    /// Each item of the identity whose request is not granted, with the instances it waits
    /// for that are full (every one it asks for when none is: it queues behind another
    /// request).
    pub(crate) waiting: Vec<(ItemId, Vec<bosun_api::WaitingFor>)>,
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
    /// The identity that asked; absent from a journal written before it was recorded, so such
    /// an entry is `busy` to every identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity: Option<String>,
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
    /// The manager of the resources of `config`, journalled to `journal` ([`journal_path`]):
    /// the journal found there is loaded, and an entry is kept only if its shim or its
    /// command's process still runs (E25).
    ///
    /// # Errors
    ///
    /// The resources do not make a lease table (a duplicate instance, a capacity of 0).
    pub(crate) fn new(config: &Config, journal: &Path) -> Result<Self, String> {
        let mut instances = Vec::new();
        let mut table = Vec::new();
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
                table.push(Instance {
                    name: instance.clone(),
                    scope: resource.scope,
                    kind: resource.kind.clone(),
                    capacity,
                    of: suffix.cloned(),
                    probe: resource
                        .probe
                        .clone()
                        .map(|command| (command, resource.probe_timeout)),
                });
                instances.push(bosun_core::Resource::new(
                    instance,
                    capacity,
                    resource.scope,
                ));
            }
        }
        let model = bosun_core::Leases::new(instances).map_err(|e| e.to_string())?;
        table.sort_by(|a, b| a.name.cmp(&b.name));
        let this = Self {
            inner: Mutex::new(Inner {
                model,
                granted: Vec::new(),
                waiting: Vec::new(),
                health: Health::default(),
            }),
            scopes,
            instances: table,
            journal: journal.to_owned(),
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
                health: Health::default(),
            }),
            scopes: std::collections::BTreeMap::new(),
            instances: Vec::new(),
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
        let bytes = match std::fs::read(&self.journal) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                crate::log::log!("bosund: the lease journal {}: {e}", self.journal.display());
                return;
            }
        };
        let journal: Journal = match serde_json::from_slice(&bytes) {
            Ok(journal) => journal,
            Err(e) => {
                // E25: a journal that does not parse is moved aside, never overwritten.
                let aside = corrupt_path(&self.journal);
                // Mutant (`mutant_leases_overwrite_corrupt_journal`): it is not moved aside.
                let moved = if cfg!(mutant_leases_overwrite_corrupt_journal) {
                    Ok(())
                } else {
                    // A rename refused (the file open elsewhere) falls back to a copy.
                    std::fs::rename(&self.journal, &aside)
                        .or_else(|_| std::fs::write(&aside, &bytes))
                };
                match moved {
                    Ok(()) => crate::log::log!(
                        "bosund: the lease journal {} cannot be read, it is moved to {} and its \
                         leases are dropped: {e}",
                        self.journal.display(),
                        aside.display()
                    ),
                    Err(keep) => crate::log::log!(
                        "bosund: the lease journal {} cannot be read ({e}) nor kept as {}: \
                         {keep}; its leases are dropped",
                        self.journal.display(),
                        aside.display()
                    ),
                }
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
                identity: stored.identity.clone(),
            };
            // Mutant (`mutant_loop_keep_stale_leases`): every entry loaded is kept.
            if !cfg!(mutant_loop_keep_stale_leases) && !self.lives(&entry, true) {
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

    /// Whether `entry`'s holder still lives (E16, E25). A command lives while its process runs.
    /// An item's lives while its shim runs, or, at run time (not `loading`), while this daemon
    /// recorded it. Another daemon that recorded it does not count: at load a recording daemon
    /// that still runs can only be another daemon (after a restart the old one is gone), and
    /// its entries are its own.
    fn lives(&self, entry: &Entry, loading: bool) -> bool {
        match &entry.holder {
            Holder::Command(key) => bosun_proc::is_running(*key),
            Holder::Item(_) => {
                // Mutant (`mutant_leases_adopt_running_daemon`): a running recorder keeps it.
                let recorder = if cfg!(mutant_leases_adopt_running_daemon) {
                    entry.daemon == self.daemon || bosun_proc::is_running(entry.daemon)
                } else {
                    !loading && entry.daemon == self.daemon
                };
                recorder || entry.shim.is_some_and(bosun_proc::is_running)
            }
        }
    }

    /// The instances `request` names, in the global order.
    fn instances(&self, request: &Request) -> Result<Vec<String>, String> {
        self.instances_of(&request.names, &request.repo, &request.identity)
    }

    /// The instances the plain `names` resolve to for `repo` and `identity`, in the global
    /// order.
    fn instances_of(
        &self,
        names: &[String],
        repo: &str,
        identity: &str,
    ) -> Result<Vec<String>, String> {
        let mut out = BTreeSet::new();
        for name in names {
            let scope = self
                .scopes
                .get(name)
                .ok_or_else(|| format!("unknown resource `{name}`"))?;
            out.insert(match scope {
                Scope::Machine => name.clone(),
                Scope::Identity => format!("{name}/{identity}"),
                Scope::Repository => format!("{name}/{repo}"),
            });
        }
        Ok(out.into_iter().collect())
    }

    /// Of the instances the plain `names` resolve to for `repo` and `identity`, those with a
    /// probe, in the global order. A name no `[resource.<name>]` configures is left out: lint
    /// refuses it, and the claim's request says so.
    pub(crate) fn probed(&self, names: &[String], repo: &str, identity: &str) -> Vec<Probed> {
        let known: Vec<String> = names
            .iter()
            .filter(|n| self.scopes.contains_key(*n))
            .cloned()
            .collect();
        self.instances_of(&known, repo, identity)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let instance = self.instances.iter().find(|i| i.name == name)?;
                let (command, timeout) = instance.probe.clone()?;
                Some(Probed {
                    instance: name,
                    command,
                    timeout,
                })
            })
            .collect()
    }

    /// Whether `identity`'s probe of `instance` is to run at `now_ms` ([`Health::begin`]),
    /// under the lock: a due probe of a failing streak has its next time moved first, so two
    /// loops of one identity never run it at once.
    pub(crate) fn probe_begin(&self, instance: &str, identity: &str, now_ms: i64) -> Check {
        self.lock()
            .health
            .begin(&Key::new(instance, identity), now_ms)
    }

    /// Records the result of `identity`'s probe of `instance` ([`Health::record`]).
    pub(crate) fn probe_record(
        &self,
        instance: &str,
        identity: &str,
        now_ms: i64,
        result: Result<(), String>,
    ) -> Recorded {
        self.lock()
            .health
            .record(&Key::new(instance, identity), now_ms, result)
    }

    /// Whether `identity`'s failing streak of `instance` is to be told now: once per streak.
    pub(crate) fn probe_tell(&self, instance: &str, identity: &str) -> bool {
        self.lock().health.tell(&Key::new(instance, identity))
    }

    /// The failing streaks of `identity`'s probes, by instance; never another identity's.
    #[allow(dead_code, reason = "#221 shows them in the API")]
    pub(crate) fn failing_of(&self, identity: &str) -> Vec<(String, Streak)> {
        self.lock().health.failing_of(identity)
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
        if let Some(entry) = inner.granted.iter_mut().find(|e| e.holder == holder) {
            if entry.resources == resources {
                // A holder that asks again (a re-attached item after a restart) is now this
                // daemon's: its lease outlives its session's shim, and its release is written
                // to its store.
                // Mutant (`mutant_leases_reask_keeps_old_daemon`): it is not re-stamped.
                if !cfg!(mutant_leases_reask_keeps_old_daemon) {
                    let changed = entry.daemon != self.daemon;
                    entry.daemon = self.daemon;
                    if let Some(writer) = &request.writer {
                        entry.writer = Some(writer.clone());
                    }
                    let identity = Some(request.identity.clone());
                    let changed = changed || entry.identity != identity;
                    entry.identity = identity;
                    // The daemon and the identity are journalled; the store is not.
                    if changed {
                        self.save(&inner);
                    }
                }
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
            identity: request.identity.clone(),
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
                identity: Some(waiter.identity),
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

    /// The processes the leases name, of every identity: each `bosun with` command that holds
    /// or waits for a lease, and each session shim a lease lives by. The process sweep spares
    /// them and what descends from them.
    pub(crate) fn processes(&self) -> Vec<ProcessKey> {
        let inner = self.lock();
        let commands = |holder: &Holder| match holder {
            Holder::Command(key) => Some(*key),
            Holder::Item(_) => None,
        };
        inner
            .granted
            .iter()
            .flat_map(|e| [commands(&e.holder), e.shim])
            .chain(inner.waiting.iter().map(|w| commands(&w.holder)))
            .flatten()
            .collect()
    }

    /// Releases every lease whose holder is gone (E16), and withdraws the request of a
    /// command whose process is gone; returns those holders.
    pub(crate) fn release_stale(&self) -> Vec<Holder> {
        let mut inner = self.lock();
        let mut dead: Vec<Holder> = inner
            .granted
            .iter()
            .filter(|e| !self.lives(e, false))
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

    /// What `identity`, whose repositories are `repos`, may see of the leases, read under the
    /// lock: its instances with their holders and waiting counts, and its items' requests that
    /// wait. A holder is named only to the identity that asked for its lease; to any other it
    /// is `busy` (#8: "busy" is all one identity learns of another).
    pub(crate) fn seen_by(&self, identity: &str, repos: &[String]) -> Seen {
        let inner = self.lock();
        let holders = |instance: &str| -> Vec<String> {
            inner
                .granted
                .iter()
                .filter(|e| e.resources.iter().any(|r| r == instance))
                .map(|e| shown(e, identity))
                .collect()
        };
        let resources = self
            .instances
            .iter()
            .filter(|i| match (i.scope, &i.of) {
                (Scope::Machine, _) => true,
                (Scope::Identity, Some(of)) => of == identity,
                (Scope::Repository, Some(of)) => repos.contains(of),
                (_, None) => false,
            })
            .map(|i| bosun_api::ResourceView {
                name: i.name.clone(),
                kind: i.kind.clone(),
                capacity: i.capacity as u64,
                scope: scope_name(i.scope).to_owned(),
                holders: holders(&i.name),
                waiting: inner.model.queue(&i.name).count() as u64,
            })
            .collect();
        let waiting = inner
            .waiting
            .iter()
            .filter(|w| w.identity == identity)
            .filter_map(|w| {
                let Holder::Item(item) = &w.holder else {
                    return None;
                };
                let asked: Vec<bosun_api::WaitingFor> = w
                    .resources
                    .iter()
                    .map(|r| bosun_api::WaitingFor {
                        resource: r.clone(),
                        holders: holders(r),
                    })
                    .collect();
                let full: Vec<bosun_api::WaitingFor> = asked
                    .iter()
                    .filter(|r| {
                        let capacity = inner.model.resource(&r.resource).map_or(1, |x| x.capacity);
                        r.holders.len() >= capacity
                    })
                    .cloned()
                    .collect();
                Some((item.clone(), if full.is_empty() { asked } else { full }))
            })
            .collect();
        Seen { resources, waiting }
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
                    identity: e.identity.clone(),
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
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        // On disk before the rename, so a power cut leaves the old journal or the new one,
        // never an empty or torn one. No test covers it: a power cut cannot be simulated here.
        file.sync_all()?;
    }
    // On Windows a rename over a file being read is refused for a moment. The retries run
    // under the manager's lock, for at most half a second, as they always did.
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

/// The holder of `entry` as `identity` sees it: `item <id>` or `bosun with <pid>` when that
/// identity asked for the lease, else `busy`.
fn shown(entry: &Entry, identity: &str) -> String {
    // Mutant (`mutant_api_lease_holder_across_identities`): every holder is named.
    let own = cfg!(mutant_api_lease_holder_across_identities)
        || entry.identity.as_deref() == Some(identity);
    if !own {
        return bosun_api::BUSY.to_owned();
    }
    match &entry.holder {
        Holder::Item(item) => format!("item {item}"),
        Holder::Command(key) => format!("bosun with {}", key.pid),
    }
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Machine => "machine",
        Scope::Identity => "identity",
        Scope::Repository => "repository",
    }
}

/// What the items of `served`'s identity wait for (`status.overview`, `item.get`): for each
/// item whose request is not granted, the full instances it waits for, with their holders as
/// the identity may see them. One read of the lease manager, under its lock.
pub(crate) fn waits_of(state: &State, served: &Served) -> Vec<(ItemId, Vec<WaitingFor>)> {
    if cfg!(mutant_api_wait_unnamed) {
        // Mutant (`mutant_api_wait_unnamed`): what an item waits for is left out.
        return Vec::new();
    }
    let repos: Vec<String> = served.repos.iter().map(|(n, _)| n.clone()).collect();
    state.leases.seen_by(&served.name, &repos).waiting
}

/// What `item` waits for in `waits` ([`waits_of`]); empty when it waits for nothing.
pub(crate) fn waiting_in(waits: &[(ItemId, Vec<WaitingFor>)], item: &ItemId) -> Vec<WaitingFor> {
    waits
        .iter()
        .find(|(id, _)| id == item)
        .map_or_else(Vec::new, |(_, waits)| waits.clone())
}

/// What item `item` of `served`'s identity waits for ([`waits_of`]).
pub(crate) fn waiting_of(state: &State, served: &Served, item: &ItemId) -> Vec<WaitingFor> {
    waiting_in(&waits_of(state, served), item)
}

/// Where a journal that does not parse is moved: `<journal>.corrupt`.
fn corrupt_path(journal: &Path) -> PathBuf {
    let mut name = journal.as_os_str().to_owned();
    name.push(".corrupt");
    PathBuf::from(name)
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

    /// The journal of a daemon whose operator token is `<dir>/bosun.token`.
    fn journal_at(dir: &Path) -> PathBuf {
        journal_path(&dir.join("bosun.token"))
    }

    /// This process's key, as the journal stores it.
    fn me() -> StoredKey {
        let pid = std::process::id();
        let start = bosun_proc::start_time(pid).unwrap().unwrap().raw();
        StoredKey { pid, start }
    }

    /// The journal's entries as stored.
    fn stored(path: &Path) -> Vec<Stored> {
        let journal: Journal =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        journal.leases
    }

    /// `holder`'s request is granted within 5 s.
    async fn granted_at_once(leases: &Leases, holder: &Holder, names: &[&str]) {
        tokio::time::timeout(
            Duration::from_secs(5),
            leases.acquire(request(holder, names)),
        )
        .await
        .unwrap_or_else(|_| panic!("{holder} was not granted {names:?} at once"))
        .unwrap();
    }

    /// Two daemons of one user on two endpoints: their operator tokens share a directory, their
    /// lease journals do not (E25), so each is granted the same machine resource and journals
    /// only its own lease. `mutant_leases_one_journal_per_directory` names one journal for both.
    #[tokio::test]
    async fn two_daemons_on_two_endpoints_keep_two_journals() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "[resource.a]\nkind = 'device'\n");
        let tokens = dir.path().join("operator");
        let (one, two) = (
            journal_path(&tokens.join("one.token")),
            journal_path(&tokens.join("two.token")),
        );
        assert_ne!(one, two);
        assert_eq!(one.parent(), Some(tokens.as_path()));
        assert_eq!(two.parent(), Some(tokens.as_path()));
        assert_eq!(
            journal_path(Path::new("x/bosun.sock.operator")),
            Path::new("x/bosun.sock.leases.json")
        );

        let first = Leases::new(&config, &one).unwrap();
        let holder_one = Holder::Item(ItemId::new("demo#1"));
        granted_at_once(&first, &holder_one, &["a"]).await;

        let second = Leases::new(&config, &two).unwrap();
        let holder_two = Holder::Item(ItemId::new("demo#2"));
        granted_at_once(&second, &holder_two, &["a"]).await;

        assert_eq!(journalled(&one), [entry("item demo#1", &["a"])]);
        assert_eq!(journalled(&two), [entry("item demo#2", &["a"])]);
        assert_eq!(first.holds(&holder_one), ["a"]);
    }

    /// A journal entry recorded by another daemon that still runs (a child of the test stands
    /// in for it) and with no shim is not adopted (E25): the new manager does not hold it, and
    /// another holder is granted the resource at once. `mutant_leases_adopt_running_daemon`
    /// keeps the entry while its recorder runs, so the request waits.
    #[tokio::test]
    async fn an_entry_of_another_running_daemon_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "[resource.a]\nkind = 'device'\n");
        let state = dir.path().join("state");
        // A process that runs until its stdin closes: `git hash-object --stdin`.
        let mut child = std::process::Command::new("git")
            .args(["hash-object", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let other = ProcessKey {
            pid: child.id(),
            start: bosun_proc::start_time(child.id()).unwrap().unwrap(),
        };
        assert!(bosun_proc::is_running(other));
        let journal = Journal {
            leases: vec![Stored {
                holder: StoredHolder::Item("demo#1".to_owned()),
                resources: vec!["a".to_owned()],
                daemon: other.into(),
                shim: None,
                granted_ms: 1,
                identity: None,
            }],
        };
        write_journal(&journal_at(&state), &journal).unwrap();

        let leases = Leases::new(&config, &journal_at(&state)).unwrap();
        let theirs = Holder::Item(ItemId::new("demo#1"));
        let mine = Holder::Item(ItemId::new("demo#2"));
        let held = leases.holds(&theirs);
        let asked = tokio::time::timeout(
            Duration::from_secs(2),
            leases.acquire(request(&mine, &["a"])),
        )
        .await;

        // The child exits when its stdin closes; it is reaped (and killed if it lingers).
        drop(child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(10), async {
            while child.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_err()
        {
            let _ = child.kill();
        }
        let _ = child.wait();

        assert_eq!(held, NONE);
        asked
            .expect("the resource is held for another daemon")
            .unwrap();
        assert_eq!(leases.holds(&mine), ["a"]);
        assert_eq!(journalled(leases.journal()), [entry("item demo#2", &["a"])]);
    }

    /// After a restart an item's entry is kept because its shim runs; its re-attached session
    /// asks again for the same set and is granted at once. Its entry is re-stamped with this
    /// daemon, so when the session's shim ends the lease stays. `mutant_leases_reask_keeps_old_
    /// daemon` leaves the dead daemon's key: the next tick releases the lease.
    #[tokio::test]
    async fn a_holder_that_asks_again_after_a_restart_keeps_its_lease_when_its_shim_ends() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "[resource.a]\nkind = 'device'\n");
        let state = dir.path().join("state");
        let me = me();
        // This pid with another start time: the daemon that recorded it, gone.
        let gone = StoredKey {
            pid: me.pid,
            start: me.start.wrapping_add(1),
        };
        let journal = Journal {
            leases: vec![Stored {
                holder: StoredHolder::Item("demo#1".to_owned()),
                resources: vec!["a".to_owned()],
                daemon: gone,
                shim: Some(me),
                granted_ms: 1,
                identity: None,
            }],
        };
        write_journal(&journal_at(&state), &journal).unwrap();

        let leases = Leases::new(&config, &journal_at(&state)).unwrap();
        let item = Holder::Item(ItemId::new("demo#1"));
        assert_eq!(leases.holds(&item), ["a"]);
        granted_at_once(&leases, &item, &["a"]).await;
        leases.set_shim(&item, None);

        assert_eq!(leases.release_stale(), []);
        assert_eq!(leases.holds(&item), ["a"]);
        let entries = stored(leases.journal());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].daemon, me);
        assert_eq!(entries[0].shim, None);
    }

    /// A journal that does not parse is moved to `<journal>.corrupt` with its bytes as they
    /// were (E25); the journal becomes a valid empty one and the manager holds nothing.
    /// `mutant_leases_overwrite_corrupt_journal` writes the empty journal over it.
    #[tokio::test]
    async fn a_journal_that_does_not_parse_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "[resource.a]\nkind = 'device'\n");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let path = journal_at(&state);
        let bad = br#"{"leases": [ {"holder": "#;
        std::fs::write(&path, bad).unwrap();
        // An older `.corrupt` is replaced.
        let aside = corrupt_path(&path);
        assert_eq!(
            aside.file_name().unwrap(),
            std::ffi::OsStr::new("bosun.leases.json.corrupt")
        );
        std::fs::write(&aside, b"older").unwrap();

        let leases = Leases::new(&config, &path).unwrap();
        assert_eq!(std::fs::read(&aside).unwrap(), bad);
        assert_eq!(stored(&path), []);
        let holder = Holder::Item(ItemId::new("demo#1"));
        assert_eq!(leases.holds(&holder), NONE);
        granted_at_once(&leases, &holder, &["a"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_waits_for_its_whole_set_and_is_served_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            dir.path(),
            "[resource.a]\nkind = 'device'\n\n[resource.b]\nkind = 'device'\n",
        );
        let state = dir.path().join("state");
        let leases = Arc::new(Leases::new(&config, &journal_at(&state)).unwrap());
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
                    identity: None,
                },
                Stored {
                    holder: StoredHolder::Item("demo#2".to_owned()),
                    resources: vec!["b".to_owned()],
                    daemon: gone,
                    shim: Some(me),
                    granted_ms: 2,
                    identity: None,
                },
                Stored {
                    holder: StoredHolder::Command(gone),
                    resources: vec!["c".to_owned()],
                    daemon: gone,
                    shim: None,
                    granted_ms: 3,
                    identity: None,
                },
            ],
        };
        write_journal(&journal_at(&state), &journal).unwrap();

        let leases = Leases::new(&config, &journal_at(&state)).unwrap();
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
