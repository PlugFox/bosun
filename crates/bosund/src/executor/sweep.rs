//! The process sweep (#8, "Process containment: the sweep"; E12): after a session of an
//! attempt has ended (normally, by a kill, or by a crash) and before the item's next step, the
//! known strays the repository lists (`repo.<name>.sweep.processes`) that started at or after
//! the attempt began are killed. Not the worktree sweep ([`super::workspace`]), which closes
//! worktrees nobody owns.
//!
//! On Windows a session's descendants are in its Job Object and die with it; the sweep is for
//! what gets out: a process started through a service, a scheduler or another daemon, and on
//! Unix one that left the process group. A process that started before the attempt is never
//! touched, whatever its name: the attempt's start is the wall clock the store recorded on its
//! row ([`bosun_store::AttemptRow::started_ms`]), and [`bosun_proc::Named::started_unix_ms`]
//! rounds down, so a doubt spares the process. Each kill goes through
//! [`bosun_proc::kill_process`] with the listed pid and start time, so a pid reused meanwhile
//! is not taken for the stray, and it kills that one process: never its group, which may be a
//! pipeline of the user's own.
//!
//! The loops of other repositories and identities run as the same user at the same time, so a
//! process of a listed name may be another session's (#2: identities are isolated). A process
//! is spared when it is inside or descends from anything else the daemon runs: the trees every
//! loop has started and not yet dropped ([`Live`]: sessions, verdict and setup runs, re-attached
//! sessions, each by its root and its container: another item's job on Windows, its process
//! group on Unix), the session shims the identity's store holds as not gone (a session a
//! previous daemon left running that its loop has not re-attached yet), and the live `bosun
//! with` commands and session shims the lease manager knows. When a container cannot be
//! listed, nothing is killed that time: a doubt spares.
//!
//! A session a graceful stop leaves running under its shim (E22) has not ended: nothing is
//! swept then; the sweep runs when the re-attached session ends.
//!
//! Each kill is reported ([`Shared::trouble`]) with the name and the pid, and the item's
//! [`NOTE_SWEEP`] note, a JSON array of `{ name, pid }`, gets an entry for it, across the
//! item's attempts, up to [`NOTE_CAP`] entries. A listing that fails is reported and that name
//! is skipped.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bosun_config::Config;
use bosun_core::ItemId;
use bosun_proc::{ChildSpec, ContainerId, ProcessKey, Spawned, Spawner, StartTime, Tree};
use bosun_store::Write;

use super::{Shared, ShimSessions, blocking, now_ms};

/// The kind of the item note that lists what the sweep killed (`Write::ItemNote`).
pub(crate) const NOTE_SWEEP: &str = "sweep";

/// How many kills the [`NOTE_SWEEP`] note keeps; later ones are only reported.
pub(crate) const NOTE_CAP: usize = 100;

/// One entry of the [`NOTE_SWEEP`] note.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Swept {
    /// The configured name the process was found under.
    pub(crate) name: String,
    /// Its pid.
    pub(crate) pid: u32,
}

/// A live tree as the sweep spares it: its root and, when it can be named, its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guarded {
    pub(crate) root: ProcessKey,
    pub(crate) container: Option<ContainerId>,
}

/// The trees the daemon's loops have started and not dropped, of every repository: one per
/// daemon, shared by the loops as the lease manager is. The sweep never kills what is inside or
/// descends from one of them.
#[derive(Debug, Default)]
pub(crate) struct Live {
    next: AtomicU64,
    trees: Mutex<BTreeMap<u64, Guarded>>,
}

impl Live {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Guarded>> {
        self.trees.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Guards `guarded` until the returned id is passed to [`Live::release`].
    pub(crate) fn guard(&self, guarded: Guarded) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, guarded);
        id
    }

    /// Stops guarding what [`Live::guard`] returned `id` for.
    pub(crate) fn release(&self, id: u64) {
        self.lock().remove(&id);
    }

    /// `spawned` with its tree guarded until the tree is dropped.
    pub(crate) fn track(self: &Arc<Self>, spawned: Spawned) -> Spawned {
        let Spawned {
            tree,
            stdout,
            stderr,
        } = spawned;
        let id = self.guard(Guarded {
            root: tree.key(),
            container: tree.container(),
        });
        Spawned {
            tree: Box::new(Tracked {
                inner: tree,
                live: self.clone(),
                id,
            }),
            stdout,
            stderr,
        }
    }

    /// What is guarded now.
    pub(crate) fn guarded(&self) -> Vec<Guarded> {
        self.lock().values().copied().collect()
    }
}

/// A tree [`Live`] guards while it lives.
#[derive(Debug)]
struct Tracked {
    inner: Box<dyn Tree>,
    live: Arc<Live>,
    id: u64,
}

impl Tree for Tracked {
    fn key(&self) -> ProcessKey {
        self.inner.key()
    }

    fn kill_tree(&mut self) -> io::Result<()> {
        self.inner.kill_tree()
    }

    fn members(&self) -> io::Result<Vec<u32>> {
        self.inner.members()
    }

    fn wait(&mut self) -> bosun_proc::WaitFuture<'_> {
        self.inner.wait()
    }

    fn container(&self) -> Option<ContainerId> {
        self.inner.container()
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.live.release(self.id);
    }
}

/// A [`Spawner`] whose every tree [`Live`] guards.
#[derive(Debug)]
pub(crate) struct Tracking {
    pub(crate) inner: Arc<dyn Spawner>,
    pub(crate) live: Arc<Live>,
}

impl Spawner for Tracking {
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned> {
        self.inner
            .spawn(spec)
            .map(|spawned| self.live.track(spawned))
    }
}

/// The [`ShimSessions`] whose every session [`Live`] guards.
pub(crate) struct TrackingShim {
    pub(crate) inner: Arc<dyn ShimSessions>,
    pub(crate) live: Arc<Live>,
}

impl ShimSessions for TrackingShim {
    fn spawn_in(
        &self,
        shim: &bosun_proc::shim::ShimSpawner,
        spec: &ChildSpec,
        dir: &std::path::Path,
    ) -> io::Result<Spawned> {
        self.inner
            .spawn_in(shim, spec, dir)
            .map(|spawned| self.live.track(spawned))
    }
}

/// What a sweep spares: the roots of live trees, commands and shims, and their containers.
#[derive(Debug, Default)]
struct Spared {
    roots: HashSet<ProcessKey>,
    containers: Vec<ContainerId>,
}

impl Spared {
    /// Whether `key` is a root, inside a container or descends from a root; a doubt (a
    /// container or a parent that cannot be read) spares it, and says why.
    fn spares(&self, key: ProcessKey, members: &HashSet<u32>) -> Result<bool, String> {
        if self.roots.contains(&key) || members.contains(&key.pid) {
            return Ok(true);
        }
        let ancestors = bosun_proc::ancestors(key)
            .map_err(|e| format!("the parents of pid {} cannot be read: {e}", key.pid))?;
        Ok(ancestors.iter().any(|a| self.roots.contains(a)))
    }

    /// The pids inside the containers, read now.
    fn members(&self) -> Result<HashSet<u32>, String> {
        let mut members = HashSet::new();
        for container in &self.containers {
            members.extend(
                container
                    .members()
                    .map_err(|e| format!("a live container cannot be listed: {e}"))?,
            );
        }
        Ok(members)
    }
}

/// Sweeps after a session of `attempt` of `item` has ended: see the module documentation.
/// Nothing happens for a repository without `repo.<name>.sweep`.
pub(crate) async fn after_session(shared: &Shared, config: &Config, item: &ItemId, attempt: u32) {
    let Some(sweep) = config
        .repos
        .get(&shared.options.repo)
        .and_then(|repo| repo.sweep.as_ref())
    else {
        return;
    };
    if sweep.processes.is_empty() {
        return;
    }
    let started = match shared.deps.reader.attempts(item).await {
        Ok(attempts) => attempts
            .iter()
            .find(|a| a.attempt.number == attempt)
            .map(|row| row.started_ms),
        Err(e) => {
            shared.trouble(format!(
                "the process sweep of {item} is skipped: its attempts do not read: {e}"
            ));
            return;
        }
    };
    let Some(started_ms) = started else {
        shared.trouble(format!(
            "the process sweep of {item} is skipped: attempt {attempt} has no row"
        ));
        return;
    };
    // The session shims the store holds as not gone, of every item but this one.
    let shims = match shared.deps.reader.processes().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| &row.item != item)
            .map(|row| {
                (
                    ProcessKey {
                        pid: row.pid,
                        start: StartTime::from_raw(row.pid_started_ms.cast_unsigned()),
                    },
                    std::path::PathBuf::from(row.record_path),
                )
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            shared.trouble(format!(
                "the process sweep of {item} is skipped: the session shims do not read: {e}"
            ));
            return;
        }
    };
    let mut guard = Spared::default();
    for guarded in shared.deps.live.guarded() {
        guard.roots.insert(guarded.root);
        guard.containers.extend(guarded.container);
    }
    guard.roots.extend(shared.deps.leases.processes());
    let names: Vec<String> = sweep.processes.iter().cloned().collect();
    let (killed, problems) = blocking(move || {
        for (key, record) in shims {
            guard.roots.insert(key);
            // A record that does not read leaves the shim's root, which still covers its tree
            // through the parents.
            if let Ok(record) = bosun_proc::shim::Record::read(&record) {
                guard.roots.insert(record.child);
                guard.containers.extend(record.container());
            }
        }
        kill_strays(&names, started_ms, &guard)
    })
    .await;
    for problem in problems {
        shared.trouble(format!("the process sweep of {item}: {problem}"));
    }
    if killed.is_empty() || cfg!(mutant_loop_sweep_unlogged) {
        // Mutant (`mutant_loop_sweep_unlogged`): the kills are neither reported nor noted.
        return;
    }
    for swept in &killed {
        shared.trouble(format!(
            "the process sweep of {item} (attempt {attempt}) killed {} (pid {}), started \
             during the attempt",
            swept.name, swept.pid
        ));
    }
    note(shared, item, killed).await;
}

/// Kills, for each of `names`, every process of the user with that name that started at or
/// after `started_ms` and that `spared` does not spare. Returns what was killed and what went
/// wrong, in words.
fn kill_strays(names: &[String], started_ms: i64, spared: &Spared) -> (Vec<Swept>, Vec<String>) {
    let own = std::process::id();
    let (mut killed, mut problems) = (Vec::new(), Vec::new());
    for name in names {
        let found = match bosun_proc::processes_named(name) {
            Ok(found) => found,
            Err(e) => {
                problems.push(format!("the processes named {name} cannot be listed: {e}"));
                continue;
            }
        };
        // Mutant (`mutant_loop_sweep_ignores_start_time`): a process that started before
        // the attempt is killed as well.
        let candidates: Vec<_> = found
            .into_iter()
            .filter(|p| {
                (p.started_unix_ms >= started_ms || cfg!(mutant_loop_sweep_ignores_start_time))
                    && p.key.pid != own
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        // Read after the listing, so a container holds whatever joined it before.
        let members = match spared.members() {
            Ok(members) => members,
            Err(e) => {
                problems.push(format!("nothing named {name} is killed: {e}"));
                continue;
            }
        };
        for process in candidates {
            // Mutant (`mutant_loop_sweep_kills_other_sessions`): a process of another live
            // session, run or `bosun with` command is killed as well.
            if !cfg!(mutant_loop_sweep_kills_other_sessions) {
                match spared.spares(process.key, &members) {
                    Ok(false) => {}
                    Ok(true) => {
                        problems.push(format!(
                            "{name} (pid {}) is spared: it belongs to another live session, \
                             run or `bosun with` command",
                            process.key.pid
                        ));
                        continue;
                    }
                    Err(e) => {
                        problems.push(format!("{name} (pid {}) is spared: {e}", process.key.pid));
                        continue;
                    }
                }
            }
            match bosun_proc::kill_process(process.key) {
                Ok(true) => killed.push(Swept {
                    name: name.clone(),
                    pid: process.key.pid,
                }),
                // Gone since the listing, or its pid has changed hands: nothing to kill.
                Ok(false) => {}
                Err(e) => problems.push(format!(
                    "{name} (pid {}) cannot be killed: {e}",
                    process.key.pid
                )),
            }
        }
    }
    (killed, problems)
}

/// Appends `killed` to the item's [`NOTE_SWEEP`] note, up to [`NOTE_CAP`] entries. Writing a
/// note kind again replaces its body, so the stored entries are read first; a body that does
/// not read is reported and replaced.
async fn note(shared: &Shared, item: &ItemId, killed: Vec<Swept>) {
    let mut entries: Vec<Swept> = match shared.deps.reader.item_note(item, NOTE_SWEEP).await {
        Ok(None) => Vec::new(),
        Ok(Some(body)) => serde_json::from_str(&body).unwrap_or_else(|e| {
            shared.trouble(format!(
                "the sweep note of {item} does not read and is started again: {e}"
            ));
            Vec::new()
        }),
        Err(e) => {
            shared.trouble(format!("the sweep note of {item} cannot be read: {e}"));
            return;
        }
    };
    let room = NOTE_CAP.saturating_sub(entries.len());
    if room == 0 {
        return;
    }
    entries.extend(killed.into_iter().take(room));
    let note = Write::ItemNote {
        item: item.clone(),
        kind: NOTE_SWEEP.to_owned(),
        body: serde_json::to_string(&entries).unwrap_or_default(),
        at_ms: now_ms(),
    };
    if let Err(e) = shared.deps.writer.write(vec![note]).await {
        shared.trouble(format!("the sweep note of {item} cannot be stored: {e}"));
    }
}
