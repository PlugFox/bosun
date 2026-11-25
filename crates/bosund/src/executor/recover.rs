//! Crash recovery (#7, #68): what the loop does at start with the items a previous daemon left
//! in a daemon-owned state.
//!
//! The plan is `bosun_core::recover`'s; this module gathers its facts and performs it, in this
//! order:
//!
//! 1. The board is read first, and a user's command on the card (a cancel, a move to Ready or
//!    Done, the `restart` label) made while the daemon was down is applied first: it replaces
//!    the resume. A board that cannot be read is asked again at the next tick; nothing is
//!    resumed meanwhile. A card missing from the first read is a cancel only when a second
//!    read confirms it.
//! 2. The children recorded for the repository's items (sessions and verdict runs, by pid and
//!    start time) that still run are killed (`bosun_proc::reap`); stage 1 adopts none (D1).
//!    Their rows are cleared. In worktree mode (#8, E3, E19) a `running` item's current
//!    session ran under the session shim and may have outlived the daemon: when its `process`
//!    row's record is readable, its run's end is not recorded, and its shim still runs or
//!    recorded the session's exit, the session is re-attached at the row's offsets
//!    (`bosun_proc::shim::attach`) and adopted instead: its agent is not reaped, the session
//!    task follows it to its end as if this daemon had spawned it (`session::attach`: the
//!    session id from the run row, the credential re-admitted from the row's digest, the tail
//!    file read and appended to), and the item records `Event::Reattached`, which spawns and
//!    counts nothing. A user command on the board still wins, and the session is then reaped.
//!    Every other `process` row of the repository's items is marked gone, and the agent its
//!    record names, if the record reads, is reaped by its pid and start time first: the row is
//!    written before the spawn and the attempt's child after it, so the row may be all that
//!    names it. A daemon that stopped gracefully (E22) leaves what one that crashed leaves: the
//!    same steps re-attach its sessions.
//! 3. Otherwise the item's lost work goes back to the machine as `Event::Restarted`: a
//!    `running` item is classified from its session's tail (the exit its run row recorded, else
//!    the output the session task kept on disk) and otherwise resumed once, uncounted; a
//!    `verifying` one runs
//!    its verdict again under a new run number, uncounted; a `landing` one redoes its keyed
//!    step; a `claimed` one redoes the claim, whose board writes are keyed too. A parked item
//!    waits for its tick.
//! 4. Only then does the outbox drainer start: rows a previous daemon sent but did not
//!    acknowledge go out again under the same keys, so the board shows one write.
//!
//! What the item runs from comes from the store, not from memory: the snapshot, the base
//! captured at claim (its refs and stash entries too), git's configuration recorded at claim
//! and the attempt's ignored files (#59, #99), the configuration and policy hashes. An item
//! whose base was not recorded (a snapshot from before #68) cannot be resumed: it goes to Needs
//! you. In worktree mode (#8) the item's worktree and the user's checkout at claim come from its
//! worktree row; an item whose row or directory is missing goes to Needs you the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use bosun_core::{Action, AttemptState, Child, Event, Exit, Facts, ItemId, RunResult, State};
use bosun_queue::{Card, Changes, Column, Snapshot};
use bosun_runner::{ProcessExit, Runner, Tail};
use bosun_store::{OutboxOp, ProcessRow, Reader, RecordedChild, StoredItem, Write, Writer};

use super::{
    Claimed, Current, Loop, blocking, commands, item_id, lookup, now_ms, outbox, park, session,
};

/// Records this start of the daemon in `writer`'s store and returns its number, above every
/// earlier one (#57). One per store: every loop of the identity recovers under it.
pub(crate) async fn next_boot(reader: &Reader, writer: &Writer) -> Result<u32, String> {
    let last = reader.last_boot().await.map_err(|e| e.to_string())?;
    let number = last.map_or(1, |n| n.saturating_add(1));
    writer
        .write(vec![Write::Boot {
            number,
            started_ms: now_ms(),
            pid: std::process::id(),
        }])
        .await
        .map_err(|e| e.to_string())?;
    Ok(number)
}

/// The key of a recorded child.
fn key_of(child: &RecordedChild) -> bosun_proc::ProcessKey {
    bosun_proc::ProcessKey {
        pid: child.pid,
        start: bosun_proc::StartTime::from_raw(child.started_ms.cast_unsigned()),
    }
}

/// A placeholder for a card the board did not show: only its id is read.
fn bare_card(id: bosun_queue::ItemId) -> Card {
    Card {
        id,
        title: String::new(),
        column: Column::InProgress,
        body: String::new(),
        labels: Vec::new(),
        draft: false,
        closed: false,
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

/// The exit `text` (a session's output kept on disk) shows, read as the runner reads a
/// session's tail: a successful `result` event the CLI printed before the daemon died is a
/// session that ended (the CLI exits 0 after it); a limit or an expired login is that; anything
/// else is a crash.
pub(crate) fn classify_tail(text: &str) -> Exit {
    let runner = bosun_runner::claude::ClaudeRunner::new("claude");
    let exit = runner.classify_exit(ProcessExit { code: Some(0) }, &Tail::from_text(text));
    session::exit_of(&exit)
}

impl Loop {
    /// Recovers every item of the repository a previous daemon left in flight, once per start;
    /// then, in worktree mode, sweeps the worktrees no item in flight owns (#8). `true` when
    /// done; `false` when the board or the store could not be read (asked again at the next
    /// tick).
    pub(crate) async fn recover(&mut self) -> bool {
        if !self.recover_items().await {
            return false;
        }
        if !self.halted {
            self.sweep_worktrees().await;
        }
        true
    }

    /// After the item in flight was rebuilt and its recorded children reaped: closes what no
    /// item owns. The item in flight keeps its own worktree, and what its tasks opened since
    /// the start is spared too.
    async fn sweep_worktrees(&self) {
        let config = self.config();
        let Some((repo, identity)) = lookup(&config, &self.shared.options.repo) else {
            return;
        };
        let Some(workspace) = super::workspace::Workspace::of(repo) else {
            return;
        };
        let owned: Vec<PathBuf> = self
            .current
            .as_ref()
            .and_then(|c| c.claimed.as_ref())
            .and_then(|c| c.worktree.as_ref())
            .map(|wt| wt.worktree.path.clone())
            .into_iter()
            .collect();
        workspace.sweep(&self.shared, identity, &owned, true).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn recover_items(&mut self) -> bool {
        let config = self.config();
        let repo_name = self.shared.options.repo.clone();
        let Some((repo, _)) = lookup(&config, &repo_name) else {
            return false;
        };
        let repo_key = repo.key.clone();
        let reader = self.shared.deps.reader.clone();
        let stored: Vec<StoredItem> = match reader.items(false).await {
            Ok(items) => items.into_iter().filter(|s| s.repo == repo_key).collect(),
            Err(e) => {
                self.shared
                    .trouble(format!("the store cannot be read: {e}"));
                return false;
            }
        };
        let children: Vec<RecordedChild> = match reader.children().await {
            Ok(children) => {
                let ours: BTreeSet<&ItemId> = stored.iter().map(|s| &s.item.id).collect();
                children
                    .into_iter()
                    .filter(|c| ours.contains(&c.item))
                    .collect()
            }
            Err(e) => {
                self.shared
                    .trouble(format!("the store cannot be read: {e}"));
                return false;
            }
        };
        // The session shims recorded for the repository's items (#8), not known to be gone.
        let processes: Vec<ProcessRow> = match reader.processes().await {
            Ok(rows) => {
                let ours: BTreeSet<&ItemId> = stored.iter().map(|s| &s.item.id).collect();
                rows.into_iter()
                    .filter(|p| ours.contains(&p.item))
                    .collect()
            }
            Err(e) => {
                self.shared
                    .trouble(format!("the store cannot be read: {e}"));
                return false;
            }
        };
        let active: Vec<StoredItem> = stored
            .into_iter()
            .filter(|s| s.item.state.is_active())
            .collect();
        if active.is_empty() && children.is_empty() && processes.is_empty() {
            return true;
        }
        let boot = match self.boot().await {
            Ok(boot) => boot,
            Err(e) => {
                self.shared
                    .trouble(format!("the boot cannot be recorded: {e}"));
                return false;
            }
        };

        // 1. The board first: the user's commands made while the daemon was down.
        let ids: Vec<ItemId> = active.iter().map(|s| s.item.id.clone()).collect();
        let board = if active.is_empty() {
            None
        } else {
            let Some(board) = self.recovery_board(&ids).await else {
                return false;
            };
            Some(board)
        };
        let pending: BTreeSet<ItemId> = match outbox::pending(&reader, &repo_name).await {
            Ok(rows) => rows
                .into_iter()
                .filter(|r| matches!(r.op, OutboxOp::Project(_)))
                .map(|r| r.item)
                .collect(),
            Err(e) => {
                self.shared.trouble(e);
                return false;
            }
        };

        // The children of items that are not in flight (a question's session in its grace)
        // have nothing to recover: they are reaped all the same.
        let mut by_item: BTreeMap<ItemId, Vec<RecordedChild>> = BTreeMap::new();
        for child in children {
            by_item.entry(child.item.clone()).or_default().push(child);
        }
        for (item, children) in &by_item {
            if !ids.contains(item) {
                self.reap(item, children).await;
            }
        }
        let mut shims: BTreeMap<ItemId, Vec<ProcessRow>> = BTreeMap::new();
        for process in processes {
            shims.entry(process.item.clone()).or_default().push(process);
        }
        for (item, rows) in &shims {
            if !ids.contains(item) {
                self.processes_gone(item, rows).await;
            }
        }

        for stored in active {
            let children = by_item.remove(&stored.item.id).unwrap_or_default();
            let rows = shims.remove(&stored.item.id).unwrap_or_default();
            if self.current.is_some() {
                // Stage 1 runs one item at a time per repository: a second one in flight is
                // not the loop's to guess about.
                self.reap(&stored.item.id, &children).await;
                self.processes_gone(&stored.item.id, &rows).await;
                self.waiting = true;
                self.shared.trouble(format!(
                    "{} was in flight too; the repository waits for the user",
                    stored.item.id
                ));
                continue;
            }
            let card = board
                .as_ref()
                .and_then(|b| outbox::card_of(&stored.item.id).and_then(|n| b.card(n)))
                .cloned();
            let pending = pending.contains(&stored.item.id);
            self.recover_item(stored, boot, card, pending, &children, &rows)
                .await;
            if self.halted {
                return true;
            }
        }
        true
    }

    /// This start's boot number: the daemon's, or (a loop on its own, in tests) a new one.
    async fn boot(&mut self) -> Result<u32, String> {
        if let Some(boot) = self.shared.options.boot {
            return Ok(boot);
        }
        let deps = &self.shared.deps;
        next_boot(&deps.reader, &deps.writer).await
    }

    /// The board as recovery reads it. A card of `items` missing from the read is read again
    /// one poll later: only a second read without it says the card is gone.
    async fn recovery_board(&mut self, items: &[ItemId]) -> Option<Changes> {
        let mut changes = self.read_board().await?;
        let missing = items
            .iter()
            .any(|id| outbox::card_of(id).is_none_or(|n| changes.card(n).is_none()));
        if missing {
            tokio::time::sleep(self.shared.options.poll).await;
            changes = self.read_board().await?;
        }
        self.cursor = Some(changes.cursor.clone());
        let repo = self.shared.options.repo.clone();
        self.board.note_titles(
            changes
                .cards
                .iter()
                .map(|c| (item_id(&repo, c.id), c.title.clone())),
        );
        Some(changes)
    }

    async fn read_board(&mut self) -> Option<Changes> {
        let queue = self.shared.deps.queue.clone();
        match blocking(move || queue.poll(None)).await {
            Ok(changes) => Some(changes),
            Err(e) => {
                self.board_error("crash recovery cannot read the board", &e);
                None
            }
        }
    }

    /// Kills the recorded children of `item` that still run and clears them all.
    async fn reap(&self, item: &ItemId, children: &[RecordedChild]) {
        for child in children {
            let key = key_of(child);
            if cfg!(mutant_recover_skip_reap) {
                // Mutant: the children a previous daemon left running are not killed.
            } else {
                match blocking(move || bosun_proc::reap(key)).await {
                    Ok(true) => crate::log::log!(
                        "bosund: repository {}: {item}: reaped pid {} of the previous daemon",
                        self.shared.options.repo,
                        child.pid
                    ),
                    Ok(false) => {}
                    Err(e) => self
                        .shared
                        .trouble(format!("{item}: pid {} cannot be reaped: {e}", child.pid)),
                }
            }
            let gone = Write::ChildGone {
                pid: child.pid,
                started_ms: child.started_ms,
            };
            if let Err(e) = self.shared.deps.writer.write(vec![gone]).await {
                self.shared
                    .trouble(format!("{item}: the child's row cannot be cleared: {e}"));
            }
        }
    }

    /// Marks the session shims of `rows` gone (#8): their sessions are not followed. The agent
    /// of a row whose record reads is reaped first, by the record's pid and start time: the
    /// row is written before the spawn, the attempt's `SessionChild` after it, so a daemon that
    /// died between the two left an agent no other row names.
    async fn processes_gone(&self, item: &ItemId, rows: &[ProcessRow]) {
        for row in rows {
            let path = PathBuf::from(&row.record_path);
            let reaped = blocking(move || {
                let record = bosun_proc::shim::Record::read(&path).ok()?;
                Some((record.child, bosun_proc::reap(record.child)))
            })
            .await;
            match reaped {
                Some((child, Ok(true))) => crate::log::log!(
                    "bosund: repository {}: {item}: reaped pid {} of a session not followed",
                    self.shared.options.repo,
                    child.pid
                ),
                Some((child, Err(e))) => self
                    .shared
                    .trouble(format!("{item}: pid {} cannot be reaped: {e}", child.pid)),
                Some((_, Ok(false))) | None => {}
            }
            let gone = Write::ProcessGone {
                record_path: row.record_path.clone(),
                at_ms: now_ms(),
            };
            if let Err(e) = self.shared.deps.writer.write(vec![gone]).await {
                self.shared
                    .trouble(format!("{item}: the shim's row cannot be cleared: {e}"));
            }
        }
    }

    /// The current session of the running `item`, re-attached (#8, E3) from its `process` row
    /// in `rows` at the row's offsets: `None` when it cannot be. It can be while the shim's
    /// record is readable, its run's end is not recorded yet, and the shim still runs or
    /// recorded the session's exit (a session that ended while no daemon ran).
    async fn attachable(
        &self,
        item: &bosun_core::Item,
        rows: &[ProcessRow],
    ) -> Option<session::Attached> {
        let attempt = item.attempt.as_ref()?;
        let process = rows
            .iter()
            .rev()
            .find(|r| r.attempt == attempt.number && r.spawn == attempt.spawn)?
            .clone();
        let runs = self.shared.deps.reader.runs(&item.id).await.ok()?;
        let run = runs
            .into_iter()
            .rev()
            .find(|r| r.attempt == attempt.number && r.spawn == attempt.spawn)?;
        if run.exit.is_some() {
            // Its end is recorded: recovered from it, as in stage 1.
            return None;
        }
        let offsets = if cfg!(mutant_loop_reattach_from_zero) {
            // Mutant (`mutant_loop_reattach_from_zero`): the session's output is read again
            // from its start.
            bosun_proc::shim::Offsets {
                stdout: 0,
                stderr: 0,
            }
        } else {
            bosun_proc::shim::Offsets {
                stdout: process.stdout_offset,
                stderr: process.stderr_offset,
            }
        };
        let path = PathBuf::from(&process.record_path);
        let found = blocking(move || {
            let record = bosun_proc::shim::Record::read(&path).map_err(|e| e.to_string())?;
            if record.exit.is_none() && !bosun_proc::is_running(record.shim) {
                return Err("its shim is gone without recording the session's exit".to_owned());
            }
            let spawned = bosun_proc::shim::attach(&path, offsets).map_err(|e| e.to_string())?;
            Ok::<_, String>((record, spawned))
        })
        .await;
        match found {
            Ok((record, spawned)) => Some(session::Attached {
                process,
                run,
                record,
                spawned,
            }),
            Err(e) => {
                crate::log::log!(
                    "bosund: repository {}: {}: the session cannot be re-attached: {e}",
                    self.shared.options.repo,
                    item.id
                );
                None
            }
        }
    }

    /// Recovers one item in flight: rebuilds it as the loop's current item, then performs the
    /// plan of `bosun_core::recover`.
    #[allow(clippy::too_many_lines)]
    async fn recover_item(
        &mut self,
        stored: StoredItem,
        boot: u32,
        card: Option<Card>,
        pending: bool,
        children: &[RecordedChild],
        processes: &[ProcessRow],
    ) {
        let item = stored.item;
        let id = item.id.clone();
        crate::log::log!(
            "bosund: repository {}: recovering {id} from state {}",
            self.shared.options.repo,
            item.state.as_str()
        );
        let claimed = if item.state == State::Claimed {
            None
        } else {
            match self.claimed_from_store(&item).await {
                Ok(claimed) => Some(claimed),
                Err(e) => {
                    self.shared.trouble(format!("{id} cannot be resumed: {e}"));
                    None
                }
            }
        };
        let ignored = match (&item.attempt, &claimed) {
            (Some(attempt), Some(_)) => self
                .ignored_from_store(&id, attempt.number)
                .await
                .map(|ignored| (attempt.number, Arc::new(ignored))),
            _ => None,
        };

        // The user's command on the card, read as the loop reads it at every poll.
        let restarted = self.restarts.contains(&id);
        let command = if item.state == State::Claimed {
            // A claim is redone from its card, which may still be in Ready (the claim's write
            // was lost) or already in In progress. A card that is gone or in Backlog cancels
            // it, one in Done is the user's.
            match card.as_ref().map(|c| c.column) {
                None | Some(Column::Backlog) => Some(bosun_core::UserCommand::Cancel),
                Some(Column::Done) => Some(bosun_core::UserCommand::ManualDone),
                Some(_) => None,
            }
        } else {
            match commands::read(item.state, card.as_ref(), pending, restarted) {
                commands::Read::Command(command) => Some(command),
                commands::Read::Nothing | commands::Read::Revert => None,
            }
        };
        if command == Some(bosun_core::UserCommand::Restart) {
            self.restarts.insert(id.clone());
        }

        // Worktree mode (#8, E19): a running item's session may have gone on under its shim.
        let mut attached = None;
        if item.state == State::Running
            && claimed.as_ref().is_some_and(|c| c.worktree.is_some())
            && !cfg!(mutant_loop_reap_instead_of_attach)
        {
            // Mutant (`mutant_loop_reap_instead_of_attach`): no session is attachable; each is
            // reaped and resumed, as in stage 1.
            attached = self.attachable(&item, processes).await;
        }
        let session_child = attached.as_ref().map(|a| a.record.child);
        let mut facts = Facts {
            boot,
            command,
            children: Vec::new(),
            tail: None,
            attachable: attached.is_some(),
        };
        for child in children {
            let key = key_of(child);
            let alive = self.shared.deps.alive.clone();
            facts.children.push(Child {
                pid: child.pid,
                started_ms: child.started_ms,
                alive: blocking(move || alive(key)).await,
                session: session_child == Some(key),
            });
        }
        if item.state == State::Running && attached.is_none() {
            facts.tail = self.lost_exit(&item).await;
        }
        if cfg!(mutant_recover_resume_before_board) {
            // Mutant: the lost work is resumed before the board's command is looked at; the
            // command waits for the next poll.
            facts.command = None;
        }

        let number = outbox::card_of(&id).unwrap_or(bosun_queue::ItemId(0));
        let card = card.unwrap_or_else(|| bare_card(number));
        let usable = claimed.is_some() || item.state == State::Claimed;
        self.current = Some(Current {
            item: item.clone(),
            card,
            claimed,
            task: None,
            power: None,
            failing: Vec::new(),
            child: park::ChildSlot::default(),
            ignored,
            rebased: Arc::default(),
        });
        if item.state != State::Claimed && item.state.is_active() {
            self.hold_power();
        }

        let ctx = self.ctx();
        let mut plan = bosun_core::recover(&item, &facts, &ctx);
        if !usable && facts.command.is_none() {
            // Nothing to run from: the item needs the user.
            plan.retain(|a| matches!(a, Action::Reap { .. }));
            plan.push(Action::Apply(Event::ConfigChanged));
        }
        if cfg!(mutant_recover_counts_crash) {
            // Mutant: the work lost with the daemon is reported to the machine as a crash of
            // the session or a verdict run that did not pass, which counts.
            for action in &mut plan {
                if let Action::Apply(Event::Restarted { .. }) = action
                    && let Some(attempt) = &item.attempt
                {
                    match item.state {
                        State::Running => {
                            *action = Action::Apply(Event::Exited {
                                attempt: attempt.number,
                                spawn: attempt.spawn,
                                exit: Exit::Crash,
                                text: String::new(),
                            });
                        }
                        State::Verifying if attempt.state == AttemptState::VerdictRunning => {
                            *action = Action::Apply(Event::Verdict {
                                attempt: attempt.number,
                                run: attempt.run,
                                result: RunResult::Fail,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // The children the plan may reap: all but an attached session's agent.
        let attaching = plan.contains(&Action::Attach);
        let reapable: Vec<RecordedChild> = children
            .iter()
            .filter(|c| !(attaching && session_child == Some(key_of(c))))
            .cloned()
            .collect();
        let mut reaped = false;
        let mut followed: Option<String> = None;
        for action in plan {
            match action {
                Action::Apply(event) => {
                    self.apply(event).await;
                    if self.halted {
                        return;
                    }
                }
                Action::Reap { .. } => {
                    if !reaped {
                        reaped = true;
                        self.reap(&id, &reapable).await;
                    }
                }
                Action::Attach => {
                    if let Some(attached) = attached.take() {
                        followed = Some(attached.process.record_path.clone());
                        self.start_attached(attached).await;
                    }
                }
            }
        }
        if !reaped {
            // Only dead children: their rows are cleared all the same.
            self.reap(&id, &reapable).await;
        }
        // The shims whose sessions are not followed are done with; the followed one's row goes
        // when its session ends.
        let others: Vec<ProcessRow> = processes
            .iter()
            .filter(|p| followed.as_ref() != Some(&p.record_path))
            .cloned()
            .collect();
        self.processes_gone(&id, &others).await;
    }

    /// The power request of an item that goes on after recovery.
    fn hold_power(&mut self) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        match self
            .shared
            .deps
            .power
            .acquire("bosun is working on an item")
        {
            Ok(guard) => cur.power = Some(std::sync::Mutex::new(guard)),
            Err(e) => self
                .shared
                .trouble(format!("the power request failed: {e}")),
        }
    }

    /// How the session lost with the daemon ended, if anything says: the exit its run row
    /// recorded (the daemon died before the transition), else its output kept on disk.
    async fn lost_exit(&self, item: &bosun_core::Item) -> Option<Exit> {
        let attempt = item.attempt.as_ref()?;
        let runs = self.shared.deps.reader.runs(&item.id).await.ok()?;
        let run = runs
            .iter()
            .rev()
            .find(|r| r.attempt == attempt.number && r.spawn == attempt.spawn)?;
        if let Some(exit) = run.exit {
            return Some(exit);
        }
        let path: PathBuf = session::tail_path(&self.shared.options.work_dir, &run.session_key);
        // Read once: the session it kept is over.
        let text = blocking(move || {
            let text = std::fs::read_to_string(&path).ok();
            let _ = std::fs::remove_file(&path);
            text
        })
        .await?;
        Some(classify_tail(&text))
    }

    /// What the claim of `item` found, from the store (#68): the snapshot, the base with its
    /// refs and stash entries, git's configuration at claim and the hashes.
    async fn claimed_from_store(&self, item: &bosun_core::Item) -> Result<Arc<Claimed>, String> {
        let reader = &self.shared.deps.reader;
        let row = reader
            .snapshot(&item.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no snapshot is recorded")?;
        let contract = bosun_queue::Contract::parse(&row.body)
            .map_err(|_| "the snapshot's contract no longer parses".to_owned())?;
        let comments: Vec<serde_json::Value> =
            serde_json::from_str(&row.comments_json).map_err(|e| e.to_string())?;
        let comments = comments
            .into_iter()
            .map(|c| bosun_queue::Comment {
                author: c["author"].as_str().unwrap_or_default().to_owned(),
                key: c["key"].as_str().map(str::to_owned),
                body: c["body"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        let snapshot = Snapshot {
            body: row.body,
            contract,
            comments,
            hash: row.hash,
        };
        let (Some(head), Some(refs), Some(stash)) =
            (row.base_sha, row.base_refs_json, row.base_stash_json)
        else {
            return Err("its base was not recorded".to_owned());
        };
        let base = bosun_git::Base {
            head,
            branch: row.branch,
            refs: serde_json::from_str(&refs).map_err(|e| format!("the base's refs: {e}"))?,
            stash: serde_json::from_str(&stash).map_err(|e| format!("the base's stash: {e}"))?,
        };
        let (Some(repo_hash), Some(policy_hash)) = (row.config_hash, row.policy_hash) else {
            return Err("its hashes were not recorded".to_owned());
        };
        let (workdir, worktree) = self.worktree_from_store(&item.id).await?;
        let git_config = self.git_config_from_store(item, &workdir).await?;
        Ok(Arc::new(Claimed {
            snapshot,
            workdir,
            worktree,
            base,
            git_config,
            repo_hash,
            policy_hash,
        }))
    }

    /// The working directory of `item` and, in worktree mode, its worktree: from its `created`
    /// row of kind `item`, with the user's checkout at claim. Without `repo.<name>.worktree`,
    /// the repository's checkout.
    async fn worktree_from_store(
        &self,
        item: &ItemId,
    ) -> Result<(PathBuf, Option<super::ItemWorktree>), String> {
        let config = self.config();
        let (repo, _) =
            lookup(&config, &self.shared.options.repo).ok_or("the repository is gone")?;
        let Some(workspace) = super::workspace::Workspace::of(repo) else {
            return Ok((repo.path.clone(), None));
        };
        let rows = self
            .shared
            .deps
            .reader
            .worktrees(item)
            .await
            .map_err(|e| e.to_string())?;
        let row = rows
            .into_iter()
            .rev()
            .find(|r| {
                r.kind == bosun_store::WorktreeKind::Item
                    && r.state == bosun_store::WorktreeState::Created
            })
            .ok_or("its worktree was not recorded")?;
        let path = PathBuf::from(&row.path);
        if !path.is_dir() {
            return Err(format!("its worktree {} is gone", path.display()));
        }
        let user = row
            .user_base_json
            .as_deref()
            .ok_or("the user's checkout at claim was not recorded")
            .map_err(str::to_owned)
            .and_then(super::checkout::UserCheckout::from_json)?;
        let target = super::landing_target(repo, &user.base)
            .ok_or("the user's checkout was on a detached HEAD at claim and `landing.target` is not configured")?;
        let worktree = bosun_git::worktree::Worktree {
            path: path.clone(),
            branch: row.branch,
            base: row.base_sha,
        };
        Ok((
            path,
            Some(super::ItemWorktree {
                workspace,
                worktree,
                user,
                target,
            }),
        ))
    }

    /// Git's configuration at claim: the copy on the latest attempt that has one. With none,
    /// no session of the item has run since the claim, so the configuration now (in
    /// `workdir`) is the claim's.
    async fn git_config_from_store(
        &self,
        item: &bosun_core::Item,
        workdir: &std::path::Path,
    ) -> Result<bosun_git::GitConfig, String> {
        let newest = item.attempt.as_ref().map_or(0, |a| a.number);
        for number in (1..=newest).rev() {
            let guards = self
                .shared
                .deps
                .reader
                .attempt_guards(&item.id, number)
                .await
                .map_err(|e| e.to_string())?;
            if let Some((_, git_config)) = guards {
                let files: BTreeMap<String, Option<String>> = serde_json::from_str(&git_config)
                    .map_err(|e| format!("git's configuration at claim: {e}"))?;
                return Ok(bosun_git::GitConfig {
                    files: files
                        .into_iter()
                        .map(|(path, digest)| (PathBuf::from(path), digest))
                        .collect(),
                });
            }
        }
        let path = workdir.to_owned();
        blocking(move || bosun_git::GitConfig::capture(&bosun_git::Git::new(path)))
            .await
            .map_err(|e| format!("git's configuration: {e}"))
    }

    /// The ignored files recorded before the first session of `attempt`, if they were.
    async fn ignored_from_store(&self, item: &ItemId, attempt: u32) -> Option<bosun_git::Ignored> {
        let (ignored, _) = self
            .shared
            .deps
            .reader
            .attempt_guards(item, attempt)
            .await
            .ok()??;
        let paths: BTreeSet<String> = serde_json::from_str(&ignored).ok()?;
        Some(bosun_git::Ignored { paths })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_tail_with_a_successful_result_is_an_ended_session() {
        assert_eq!(classify_tail(&fixture("normal-end.jsonl")), Exit::Ok);
    }

    #[test]
    fn a_tail_cut_off_mid_session_is_a_crash() {
        let text = fixture("normal-end.jsonl");
        let cut: Vec<&str> = text
            .lines()
            .filter(|l| !l.contains("\"type\":\"result\""))
            .collect();
        assert_eq!(classify_tail(&cut.join("\n")), Exit::Crash);
        assert_eq!(classify_tail(""), Exit::Crash);
    }
}
