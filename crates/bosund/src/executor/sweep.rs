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
//! rounds down, so a doubt spares the process. Each kill goes through [`bosun_proc::reap`]
//! with the listed pid and start time, so a pid reused meanwhile is not taken for the stray.
//!
//! A session a graceful stop leaves running under its shim (E22) has not ended: nothing is
//! swept then; the sweep runs when the re-attached session ends.
//!
//! Each kill is reported ([`Shared::trouble`]) with the name and the pid, and the item's
//! [`NOTE_SWEEP`] note, a JSON array of `{ name, pid }`, gets an entry for it, across the
//! item's attempts, up to [`NOTE_CAP`] entries. A listing that fails is reported and that name
//! is skipped.

use bosun_config::Config;
use bosun_core::ItemId;
use bosun_store::Write;

use super::{Shared, blocking, now_ms};

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
    let names: Vec<String> = sweep.processes.iter().cloned().collect();
    let (killed, problems) = blocking(move || kill_strays(&names, started_ms)).await;
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
/// after `started_ms`. Returns what was killed and what went wrong, in words.
fn kill_strays(names: &[String], started_ms: i64) -> (Vec<Swept>, Vec<String>) {
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
        for process in found {
            // Mutant (`mutant_loop_sweep_ignores_start_time`): a process that started before
            // the attempt is killed as well.
            let older = process.started_unix_ms < started_ms;
            if (older && !cfg!(mutant_loop_sweep_ignores_start_time)) || process.key.pid == own {
                continue;
            }
            match bosun_proc::reap(process.key) {
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
