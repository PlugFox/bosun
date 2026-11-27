//! The writer task: the only write connection, fed batches through a channel (#3).
//!
//! A batch is the caller's unit of atomicity: the writer applies it in one transaction, so it
//! is in the database whole or not at all. Ingest sends the rows parsed from one read of a file
//! together with that file's new offset, so a crash never leaves rows without their offset or
//! an offset without its rows.

use std::time::Duration;

use bosun_core::{
    Commit, Effect, FileTouch, Gap, ItemId, Message, Rates, SessionCheck, Stepped, ToolResult,
    WindowKey, WindowKind, WindowMeta, canonical_path, default_table, earliest, merge, repo_root,
    strip_root, style_of,
};
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Executor};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::annotation;
use crate::error::{Error, from_db, to_db};
use crate::notify::{self, NewChannel, NewNotice};
use crate::proposal::{self, NewProposal, ProposalStep};
use crate::read;
use crate::work::write as work;
use crate::work::{
    AgentCall, LandingRow, NewItem, NewProcess, NewWorktree, RunCommits, RunDenials, RunEnd,
    RunStart, Snapshot, VerdictEnd, VerdictStart, WorktreeState,
};

/// How many batches may wait for the writer before senders wait too.
const QUEUE: usize = 64;

/// One change to the database. A batch of them is applied in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Registers a transcript file or records how far it has been parsed.
    SourceFile(SourceFile),
    /// A record of a message, merged with what is stored by [`bosun_core::merge`]. Its source
    /// file must have been registered by an earlier write.
    Message {
        message: Message,
        parser_version: u32,
    },
    /// A line of `file_key` that could not be read as expected. Keyed by (file, offset): a
    /// second gap at the same offset replaces the first.
    Gap {
        provider: String,
        file_key: String,
        gap: Gap,
        parser_version: u32,
    },
    /// A new price for a model from a time on. Closes the current row; never touches a message.
    ChangePrice(PriceChange),
    /// Registers a repository root or records its origin and default branch. Keyed by the
    /// configuration name when it has one, else by the canonical path (a path a named
    /// repository was configured at before resolves to it and creates nothing). A named
    /// repository written at a new path moves: its row's path is updated in place, the old
    /// path stays an alias of the row, and an unnamed row the new path had is merged into it;
    /// a row of another name at the path is refused. Messages are attributed to the longest
    /// root or alias that contains their cwd ([`bosun_core::repo_root`]); a root seen for the
    /// first time re-attributes every stored message.
    Repo(Repo),
    /// Every repository an identity's configuration names, registered as one set (#132), as
    /// the daemon does at start. Each is written as a named [`Write::Repo`] is, and the set
    /// settles what one write cannot:
    ///
    /// - A row at a configured path whose name the set does not have (the repository was
    ///   renamed, or migration 16 named the row after an old item prefix) gives up the name:
    ///   the configured name takes the row over, or merges it into its own.
    /// - Moves out of a path come before moves into it, so one repository can take a path
    ///   another leaves in the same edit, a swap included.
    /// - An unnamed row whose items are the name's (`<name>#<card>`, left there by a move
    ///   before migration 16) is merged into the named row, and its path becomes an alias.
    Configured(Vec<Configured>),
    /// A commit of the repository with path key `repo` (or the repository `repo` is an alias
    /// of), which is registered by path if it is not yet.
    /// Keyed by (repository, sha): a stored commit is never changed, so writing it again
    /// changes nothing.
    Commit { repo: String, commit: Commit },
    /// What the provider says about a context window: for a sub-agent, its parent, type, depth
    /// and spawning call. Fields the meta leaves out keep their stored value.
    Session { provider: String, meta: WindowMeta },
    /// The result of a tool call, applied to the stored call with its `tool_use` id. A result
    /// whose call is not stored is dropped.
    ToolResult(ToolResult),
    /// A file a stored tool call touched. A touch whose call is not stored is dropped.
    FileTouch(FileTouch),
    /// The provider's totals for a session and their window; replace what is stored for its
    /// root session.
    SessionCheck {
        check: SessionCheck,
        /// When bosun recorded it.
        recorded_ms: i64,
    },
    /// Registers an item of the repository of its name (registered as [`Write::Repo`] with that
    /// name and path), or updates its card key and links it to that repository. Its state is
    /// changed by transitions only.
    Item(NewItem),
    /// The item's snapshot at claim; it becomes the item's current snapshot. Keyed by its key.
    Snapshot(Snapshot),
    /// One arrow of the item machine, recorded before its effects: the transition under its
    /// idempotence key, the item and attempt as the step left them, and its board writes in the
    /// outbox. A key already stored is a replay and changes nothing.
    Transition { stepped: Stepped, at_ms: i64 },
    /// A session about to be spawned, with its pre-assigned provider session id. A transcript of
    /// that id is a run session whether it was ingested before or after this row.
    Run(RunStart),
    /// The child process of the attempt's current session (#50).
    SessionChild {
        item: ItemId,
        attempt: u32,
        pid: u32,
        pid_started_ms: i64,
    },
    /// The guard snapshots of #59 for an attempt (#99), as JSON: the ignored files before its
    /// first session and git's own configuration at claim. Crash recovery (#68) compares the
    /// repository with them again.
    AttemptGuards {
        item: ItemId,
        attempt: u32,
        ignored_json: String,
        git_config_json: String,
    },
    /// Why the daemon refused an attempt before its verdict (#137): the failed preconditions or
    /// the guard refusals, in the daemon's words. Writing it again replaces them.
    AttemptRefusal {
        item: ItemId,
        attempt: u32,
        reasons: Vec<String>,
    },
    /// A session ended; clears the attempt's child process.
    RunEnded(RunEnd),
    /// The commit range of a session that ended: each commit of it is linked to the session
    /// exactly (#72). Keyed by (attempt, spawn).
    RunCommits(RunCommits),
    /// The tool calls a session's agent CLI refused (#7), stored on its attempt. Keyed by
    /// (attempt, spawn, position in the session): writing it again adds nothing.
    RunDenials(RunDenials),
    /// A verdict run started, with its child process. Keyed by (attempt, run).
    VerdictStarted(VerdictStart),
    /// A verdict run finished; clears its child process.
    VerdictEnded(VerdictEnd),
    /// A call of the agent. Every call is a row; it is not a transition.
    AgentCall(AgentCall),
    /// A board write in the outbox, outside a transition. Keyed by the effect's key; a pending
    /// write with the same coalesce key absorbs it. Effects that are not board writes are
    /// skipped.
    Enqueue { effect: Effect, at_ms: i64 },
    /// The board acknowledged `revision` of outbox row `id`. A row that absorbed a newer write
    /// since stays pending.
    Acknowledge { id: i64, revision: u32, at_ms: i64 },
    /// Sending outbox row `id` failed; it waits until `retry_at_ms`.
    SendFailed {
        id: i64,
        error: String,
        retry_at_ms: i64,
    },
    /// A comment the daemon created on the board, by its key. Writing it again records the new
    /// node id (a deleted comment recreated).
    BoardComment {
        item: ItemId,
        key: String,
        node_id: String,
        at_ms: i64,
    },
    /// What the daemon found about an item for its Needs-you comment (#111): the lint
    /// findings of a bounce (`kind` `lint`), the figure of a budget cap (`budget`). Writing a
    /// kind again replaces its body.
    ItemNote {
        item: ItemId,
        kind: String,
        body: String,
        at_ms: i64,
    },
    /// Tests that failed on a verdict's first run and passed on its re-run (#44 D).
    FlakyTests {
        item: ItemId,
        attempt: u32,
        names: Vec<String>,
        at_ms: i64,
    },
    /// A child process is gone (reaped, or found dead): the session or verdict run that named
    /// it by pid and start time no longer does.
    ChildGone { pid: u32, started_ms: i64 },
    /// A start of the daemon, numbered after the last one ([`crate::Reader::last_boot`]). A
    /// number already recorded fails the batch: two starts never share one.
    Boot {
        number: u32,
        started_ms: i64,
        pid: u32,
    },
    /// A git worktree the daemon made for an item (#8). Keyed by its path: writing it again
    /// stores nothing.
    Worktree(NewWorktree),
    /// The worktree at `path` was removed, or quarantined with the file that blocked its
    /// removal and the error. Only a worktree still `created` ends: the first end is kept.
    /// `bytes`, when given, is its last measured size.
    WorktreeEnded {
        path: String,
        /// [`WorktreeState::Removed`] or [`WorktreeState::Quarantined`].
        state: WorktreeState,
        blocking: Option<String>,
        error: Option<String>,
        bytes: Option<u64>,
        at_ms: i64,
    },
    /// The size on disk of the worktree at `path`, in bytes, as last measured.
    WorktreeBytes { path: String, bytes: u64 },
    /// The `setup` or `teardown` command running in the worktree at `path` (#8, E3): its
    /// child, by pid and start time, while it runs; `None` once it ended. Replaces the record.
    WorktreeChild {
        path: String,
        child: Option<(u32, i64)>,
    },
    /// The quarantined worktree at `path` was removed once it could be: `quarantined` becomes
    /// `removed`, ended at `at_ms`; what blocked it stays as history. A worktree in another
    /// state is left as it is.
    WorktreeFreed { path: String, at_ms: i64 },
    /// A session shim the daemon started (#8). Keyed by its record's path: writing it again
    /// stores nothing.
    Process(NewProcess),
    /// The shim of a `process` row written before its spawn (pid 0) has started: its pid and
    /// start time. Writing it again stores the same.
    ProcessStarted {
        record_path: String,
        pid: u32,
        pid_started_ms: i64,
    },
    /// How far the daemon has read the shim's output files (E18). An offset only grows: the
    /// larger of the stored and the written one is kept.
    ProcessOffsets {
        record_path: String,
        stdout: u64,
        stderr: u64,
    },
    /// The shim is gone; the first time recorded is kept.
    ProcessGone { record_path: String, at_ms: i64 },
    /// A request of `holder` for `resource` (#8, E15), for an item or (`None`) a `bosun with`
    /// command. Keyed by `(holder, resource, filed_ms)`: writing it again stores nothing.
    LeaseFiled {
        item: Option<ItemId>,
        holder: String,
        resource: String,
        filed_ms: i64,
    },
    /// The request filed at `filed_ms` was granted; the first grant recorded is kept.
    LeaseGranted {
        holder: String,
        resource: String,
        filed_ms: i64,
        at_ms: i64,
    },
    /// `holder` released what it held: every granted request of it not yet released.
    LeaseReleased { holder: String, at_ms: i64 },
    /// Where the item's landing stands (#8). Keyed by the item: the attempt, target and time
    /// are replaced, and a column given as `None` keeps its stored value.
    Landing(LandingRow),
    /// The refs of the base of `item`'s current snapshot (a JSON object, as
    /// [`Snapshot::base_refs_json`]), captured again: landing moved the target the item's
    /// preconditions compare with (#8). Fails when the item has no snapshot.
    SnapshotRefs { item: ItemId, refs_json: String },
    /// The user's checkout as captured for the worktree at `path`
    /// ([`NewWorktree::user_base_json`]), captured again: landing moved the user's target (#8).
    WorktreeUserBase {
        path: String,
        user_base_json: String,
    },
    /// A notification channel of the identity (#9). Refused ([`Error::Refused`]) for a name a
    /// live channel has, or one that is not `[a-z0-9-]{1,32}`.
    ChannelAdded(NewChannel),
    /// The live channel `name` is removed: its row is kept, marked, and the name is free.
    /// Fails ([`Error::UnknownRow`]) when no live channel has it.
    ChannelRemoved { name: String, at_ms: i64 },
    /// A notice (#9): one row per live channel whose events have it; a key a channel has
    /// already queues nothing for it.
    NoticeQueued(NewNotice),
    /// The notification drainer sent `ids` in one message at `at_ms`: they are sent, and their
    /// channel last sent then.
    NoticesSent { ids: Vec<i64>, at_ms: i64 },
    /// A send of `ids` failed and may pass: one more try each, retried from `retry_at_ms`.
    NoticesFailed {
        ids: Vec<i64>,
        error: String,
        retry_at_ms: i64,
    },
    /// `ids` are never to be sent: too old, refused for good, or their channel unbuildable.
    NoticesDropped {
        ids: Vec<i64>,
        why: String,
        at_ms: i64,
    },
    /// A heat proposal a detector found (#11 D), stored `open`. Refused by name when it lacks a
    /// metric, a value, a target or a window, when its repository already has an active
    /// proposal of the same (detector, provider, subject), and inside the cooldown of that key.
    ProposalOpened(NewProposal),
    /// The detector found the subject of the open or published proposal `key` again.
    ProposalSeen { key: String, at_ms: i64 },
    /// One arrow of the proposal machine ([`bosun_core::proposal::step`]) on proposal `key`.
    ProposalStep {
        key: String,
        step: ProposalStep,
        at_ms: i64,
    },
    /// An annotation of the repository at path key `repo` at `ts_ms` (#11 D: a landed heat
    /// proposal). Refused when no repository has that path.
    Annotation {
        ts_ms: i64,
        repo: String,
        text: String,
    },
    /// Mutant `mutant_ingest_reingest_truncates` (#21): deletes every ingested row, as a
    /// reingest that truncates before it rebuilds would.
    #[cfg(mutant_ingest_reingest_truncates)]
    Truncate,
}

/// A repository the configuration names ([`Write::Configured`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    /// The configuration name (`repo.<name>`).
    pub name: String,
    /// The canonical path key (`bosun_core::canonical_path`), as the configuration holds it.
    pub path: String,
}

/// A repository root. Keyed by `name` when it has one (a configured repository), else by
/// `path`: writing it again updates the row, and a named one written at another path moves its
/// row there ([`Write::Repo`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The configuration name (`repo.<name>`); `None` for a root registered by path only.
    pub name: Option<String>,
    /// The canonical path key (`bosun_core::canonical_path`), as the configuration holds it.
    pub path: String,
    /// The origin URL, without credentials.
    pub remote: Option<String>,
    pub default_branch: Option<String>,
}

/// A transcript file and how far it has been parsed. Keyed by (provider, file key): writing it
/// again updates the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub provider: String,
    /// The file id, as in [`bosun_core::Origin::file`].
    pub file_key: String,
    pub path: String,
    pub size: u64,
    pub parsed_offset: u64,
    pub parser_version: u32,
    pub last_line_hash: Option<String>,
    /// The complete lines up to `parsed_offset`; `None` when they were not counted (a file
    /// parsed before the count existed keeps `None` until it is re-read from zero).
    pub parsed_lines: Option<u64>,
    /// The lines up to `parsed_offset` of record types the parser does not know; `None` when
    /// not counted, as for `parsed_lines`.
    pub unknown_lines: Option<u64>,
    /// The lines up to `parsed_offset` of the provider's usage-bearing record type, billed or
    /// not; `None` when not counted.
    pub usage_lines: Option<u64>,
    /// Those of `usage_lines` that billed nothing (a gap); `None` when not counted.
    pub unbilled_lines: Option<u64>,
    /// The timestamp of the last timestamped line up to `parsed_offset`, carried to the next
    /// read: a read may begin right at a session check, whose window ends there (#46).
    pub last_ts_ms: Option<i64>,
}

/// New rates for `model` (and `prompt_threshold`) valid from `from_ms` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceChange {
    pub model: String,
    pub prompt_threshold: Option<u64>,
    /// Must be after the start of the current row.
    pub from_ms: i64,
    pub rates: Rates,
}

#[derive(Debug)]
enum Command {
    Batch(Vec<Write>, oneshot::Sender<Result<(), Error>>),
    Shutdown(oneshot::Sender<Result<(), Error>>),
}

/// A handle to the writer task. Cloning it is cheap; every clone feeds the same task.
#[derive(Debug, Clone)]
pub struct Writer {
    commands: mpsc::Sender<Command>,
}

impl Writer {
    pub(crate) fn spawn(conn: SqliteConnection, checkpoint: Duration) -> (Self, JoinHandle<()>) {
        let (commands, inbox) = mpsc::channel(QUEUE);
        let task = tokio::spawn(run(conn, inbox, checkpoint));
        (Self { commands }, task)
    }

    /// Applies `batch` in one transaction and waits for the commit.
    ///
    /// # Errors
    ///
    /// The batch failed and was rolled back as a whole, or the store is closed.
    pub async fn write(&self, batch: Vec<Write>) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Batch(batch, reply))
            .await
            .map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Shutdown(reply))
            .await
            .map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)?
    }
}

async fn run(mut conn: SqliteConnection, mut inbox: mpsc::Receiver<Command>, every: Duration) {
    let mut timer = tokio::time::interval_at(Instant::now() + every, every);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Batch(batch, reply)) => {
                    let _ = reply.send(apply_batch(&mut conn, batch).await);
                }
                Some(Command::Shutdown(reply)) => {
                    let _ = reply.send(close(conn).await);
                    return;
                }
                // Every handle is gone: stop as if shut down; nobody waits for the result.
                None => {
                    let _ = close(conn).await;
                    return;
                }
            },
            _ = timer.tick() => {
                // A failed periodic checkpoint is retried on the next tick, and SQLite's own
                // automatic checkpoint still bounds the WAL meanwhile.
                let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)").await;
            }
        }
    }
}

async fn close(mut conn: SqliteConnection) -> Result<(), Error> {
    let checkpoint = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").await;
    conn.close().await?;
    checkpoint?;
    Ok(())
}

async fn apply_batch(conn: &mut SqliteConnection, batch: Vec<Write>) -> Result<(), Error> {
    if cfg!(mutant_store_commit_per_row) {
        for (row, write) in batch.into_iter().enumerate() {
            let mut tx = conn.begin().await?;
            apply(&mut tx, write).await?;
            tx.commit().await?;
            failpoint(row + 1);
        }
        return Ok(());
    }
    let mut tx = conn.begin().await?;
    for (row, write) in batch.into_iter().enumerate() {
        apply(&mut tx, write).await?;
        failpoint(row + 1);
    }
    tx.commit().await?;
    Ok(())
}

/// In tests, aborts the process once the given row of a batch has been applied (see the abort
/// test below). Compiled out of every other build.
#[cfg(test)]
pub(crate) static ABORT_AFTER_ROW: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn failpoint(row: usize) {
    if ABORT_AFTER_ROW.load(std::sync::atomic::Ordering::SeqCst) == row {
        eprintln!("failpoint: abort after row {row}");
        std::process::abort();
    }
}

#[cfg(not(test))]
fn failpoint(_row: usize) {}

// Long: one arm per kind of write.
#[allow(clippy::too_many_lines)]
async fn apply(conn: &mut SqliteConnection, write: Write) -> Result<(), Error> {
    match write {
        Write::SourceFile(file) => upsert_source_file(conn, &file).await,
        Write::Message {
            message,
            parser_version,
        } => upsert_message(conn, message, parser_version).await,
        Write::Gap {
            provider,
            file_key,
            gap,
            parser_version,
        } => upsert_gap(conn, &provider, &file_key, &gap, parser_version).await,
        Write::ChangePrice(change) => change_price(conn, &change).await,
        Write::Repo(repo) => upsert_repo(conn, &repo).await.map(drop),
        Write::Configured(repos) => register_configured(conn, &repos).await,
        Write::Commit { repo, commit } => insert_commit(conn, &repo, &commit).await,
        Write::Session { provider, meta } => upsert_session(conn, &provider, &meta).await,
        Write::ToolResult(result) => apply_tool_result(conn, &result).await,
        Write::FileTouch(touch) => insert_file_touch(conn, &touch).await,
        Write::SessionCheck { check, recorded_ms } => {
            replace_session_check(conn, &check, recorded_ms).await
        }
        Write::Item(item) => work::upsert_item(conn, &item).await,
        Write::Snapshot(snapshot) => work::insert_snapshot(conn, &snapshot).await,
        Write::Transition { stepped, at_ms } => {
            work::record_transition(conn, &stepped, at_ms).await
        }
        Write::Run(run) => work::insert_run(conn, &run).await,
        Write::SessionChild {
            item,
            attempt,
            pid,
            pid_started_ms,
        } => work::session_child(conn, &item, attempt, pid, pid_started_ms).await,
        Write::AttemptGuards {
            item,
            attempt,
            ignored_json,
            git_config_json,
        } => work::attempt_guards(conn, &item, attempt, &ignored_json, &git_config_json).await,
        Write::AttemptRefusal {
            item,
            attempt,
            reasons,
        } => work::attempt_refusal(conn, &item, attempt, &reasons).await,
        Write::RunEnded(end) => work::end_run(conn, &end).await,
        Write::RunCommits(range) => work::run_commits(conn, &range).await,
        Write::RunDenials(denials) => work::run_denials(conn, &denials).await,
        Write::VerdictStarted(start) => work::start_verdict(conn, &start).await,
        Write::VerdictEnded(end) => work::end_verdict(conn, &end).await,
        Write::AgentCall(call) => work::insert_agent_call(conn, &call).await,
        Write::Enqueue { effect, at_ms } => work::enqueue_effect(conn, &effect, at_ms).await,
        Write::Acknowledge {
            id,
            revision,
            at_ms,
        } => work::acknowledge(conn, id, revision, at_ms).await,
        Write::SendFailed {
            id,
            error,
            retry_at_ms,
        } => work::send_failed(conn, id, &error, retry_at_ms).await,
        Write::BoardComment {
            item,
            key,
            node_id,
            at_ms,
        } => work::upsert_board_comment(conn, &item, &key, &node_id, at_ms).await,
        Write::ItemNote {
            item,
            kind,
            body,
            at_ms,
        } => work::upsert_item_note(conn, &item, &kind, &body, at_ms).await,
        Write::FlakyTests {
            item,
            attempt,
            names,
            at_ms,
        } => work::insert_flaky_tests(conn, &item, attempt, &names, at_ms).await,
        Write::ChildGone { pid, started_ms } => work::child_gone(conn, pid, started_ms).await,
        Write::Boot {
            number,
            started_ms,
            pid,
        } => work::insert_boot(conn, number, started_ms, pid).await,
        Write::Worktree(new) => work::insert_worktree(conn, &new).await,
        Write::WorktreeEnded {
            path,
            state,
            blocking,
            error,
            bytes,
            at_ms,
        } => {
            let end = work::WorktreeEnd {
                path: &path,
                state,
                blocking: blocking.as_deref(),
                error: error.as_deref(),
                bytes,
                at_ms,
            };
            work::end_worktree(conn, &end).await
        }
        Write::WorktreeBytes { path, bytes } => work::worktree_bytes(conn, &path, bytes).await,
        Write::WorktreeChild { path, child } => work::worktree_child(conn, &path, child).await,
        Write::WorktreeFreed { path, at_ms } => work::worktree_freed(conn, &path, at_ms).await,
        Write::Process(new) => work::insert_process(conn, &new).await,
        Write::ProcessStarted {
            record_path,
            pid,
            pid_started_ms,
        } => work::process_started(conn, &record_path, pid, pid_started_ms).await,
        Write::ProcessOffsets {
            record_path,
            stdout,
            stderr,
        } => work::process_offsets(conn, &record_path, stdout, stderr).await,
        Write::ProcessGone { record_path, at_ms } => {
            work::process_gone(conn, &record_path, at_ms).await
        }
        Write::LeaseFiled {
            item,
            holder,
            resource,
            filed_ms,
        } => work::lease_filed(conn, item.as_ref(), &holder, &resource, filed_ms).await,
        Write::LeaseGranted {
            holder,
            resource,
            filed_ms,
            at_ms,
        } => work::lease_granted(conn, &holder, &resource, filed_ms, at_ms).await,
        Write::LeaseReleased { holder, at_ms } => work::lease_released(conn, &holder, at_ms).await,
        Write::Landing(landing) => work::upsert_landing(conn, &landing).await,
        Write::SnapshotRefs { item, refs_json } => {
            work::snapshot_refs(conn, &item, &refs_json).await
        }
        Write::WorktreeUserBase {
            path,
            user_base_json,
        } => work::worktree_user_base(conn, &path, &user_base_json).await,
        Write::ChannelAdded(new) => notify::add(conn, &new).await,
        Write::ChannelRemoved { name, at_ms } => notify::remove(conn, &name, at_ms).await,
        Write::NoticeQueued(notice) => notify::queue(conn, &notice).await,
        Write::NoticesSent { ids, at_ms } => notify::sent(conn, &ids, at_ms).await,
        Write::NoticesFailed {
            ids,
            error,
            retry_at_ms,
        } => notify::failed(conn, &ids, &error, retry_at_ms).await,
        Write::NoticesDropped { ids, why, at_ms } => notify::dropped(conn, &ids, &why, at_ms).await,
        Write::ProposalOpened(new) => proposal::open(conn, &new).await,
        Write::ProposalSeen { key, at_ms } => proposal::seen(conn, &key, at_ms).await,
        Write::ProposalStep { key, step, at_ms } => proposal::step(conn, &key, &step, at_ms).await,
        Write::Annotation { ts_ms, repo, text } => annotation::add(conn, ts_ms, &repo, &text).await,
        #[cfg(mutant_ingest_reingest_truncates)]
        Write::Truncate => {
            conn.execute(
                "DELETE FROM file_touch; DELETE FROM tool_call; DELETE FROM message;
                 DELETE FROM ingest_gap; DELETE FROM session_check;
                 DELETE FROM session_check_window;",
            )
            .await?;
            Ok(())
        }
    }
}

/// Every repository root a working directory can attribute to: each row's id with its
/// canonical key, and with every alias of it (a path it was configured at before).
pub(crate) async fn repos(conn: &mut SqliteConnection) -> Result<Vec<(i64, String)>, Error> {
    Ok(sqlx::query!(
        r#"SELECT id AS "id!: i64", path AS "path!: String" FROM repo
           UNION ALL
           SELECT repo_id AS "id!: i64", path AS "path!: String" FROM repo_alias"#
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| (r.id, r.path))
    .collect())
}

/// The id of the repository whose root `cwd` belongs to, if any.
fn repo_of(cwd: &str, repos: &[(i64, String)]) -> Option<i64> {
    let root = repo_root(cwd, repos.iter().map(|(_, path)| path.as_str()))?;
    repos
        .iter()
        .find(|(_, path)| path == root)
        .map(|(id, _)| *id)
}

/// The id of the repository at canonical key `key`: the row with that path, or the row that
/// was configured there before (an alias).
pub(crate) async fn repo_at(conn: &mut SqliteConnection, key: &str) -> Result<Option<i64>, Error> {
    Ok(sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM (
               SELECT id, 0 AS rank FROM repo WHERE path = ?1
               UNION ALL
               SELECT repo_id AS id, 1 AS rank FROM repo_alias WHERE path = ?1)
           ORDER BY rank LIMIT 1"#,
        key,
    )
    .fetch_optional(conn)
    .await?)
}

/// Registers `repo` and returns its row's id.
///
/// Without a name, a path that is a row's or an alias of one resolves to it, and a new path is
/// a new (unnamed) row. With a name, the row of that name is the repository: a path that
/// differs from its stored one is a move, so the old path becomes an alias of the row and the
/// row's path is updated in place (every message, commit and item on the row follows it). An
/// unnamed row that already holds the new path (registered by path before the move) is merged
/// into the named one. A row of another name at the path is refused: two configured
/// repositories never share one path.
pub(crate) async fn upsert_repo(conn: &mut SqliteConnection, repo: &Repo) -> Result<i64, Error> {
    let key = canonical_path(&repo.path, style_of(&repo.path));
    let (id, new_root) = match &repo.name {
        Some(name) => named_repo(conn, name, &key, false).await?,
        None => unnamed_repo(conn, &key).await?,
    };
    sqlx::query!(
        "UPDATE repo SET
             remote = coalesce(?, remote),
             default_branch = coalesce(?, default_branch)
         WHERE id = ?",
        repo.remote,
        repo.default_branch,
        id,
    )
    .execute(&mut *conn)
    .await?;
    if new_root {
        reattribute(conn).await?;
    }
    Ok(id)
}

/// The row at `key` (or the row `key` is an alias of), inserted unnamed if there is none, and
/// whether the root is new.
async fn unnamed_repo(conn: &mut SqliteConnection, key: &str) -> Result<(i64, bool), Error> {
    if let Some(id) = repo_at(conn, key).await? {
        return Ok((id, false));
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO repo (path) VALUES (?) RETURNING id AS "id!: i64""#,
        key
    )
    .fetch_one(conn)
    .await?;
    Ok((id, true))
}

/// Registers the configured set `repos` ([`Write::Configured`]).
async fn register_configured(
    conn: &mut SqliteConnection,
    repos: &[Configured],
) -> Result<(), Error> {
    let repos: Vec<(&str, String)> = repos
        .iter()
        .map(|r| (r.name.as_str(), canonical_path(&r.path, style_of(&r.path))))
        .collect();
    let configured = |name: &str| repos.iter().any(|(n, _)| *n == name);
    // A row at a configured path under a name the configuration no longer has gives the name
    // up: the configured name then takes it over (or merges it) as an unnamed row.
    // Mutant (`mutant_store_repo_rename_refused`): a renamed repository's row keeps its old
    // name, so registering the new name at its path is refused and the store does not open.
    if !cfg!(mutant_store_repo_rename_refused) {
        for (_, key) in &repos {
            let at_path = sqlx::query!(
                r#"SELECT id AS "id!: i64", name FROM repo WHERE path = ?"#,
                key
            )
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(row) = at_path
                && row.name.as_deref().is_some_and(|other| !configured(other))
            {
                sqlx::query!("UPDATE repo SET name = NULL WHERE id = ?", row.id)
                    .execute(&mut *conn)
                    .await?;
            }
        }
    }
    // Moves out before moves in: each row that moves leaves its path first (the path becomes
    // an alias of it, and the row is parked at a path no repository has), so a path one
    // repository leaves is free for another, whatever the order of the set.
    let mut parked = Vec::new();
    // Mutant (`mutant_store_repo_moves_unordered`): no row leaves its path first, so a swap
    // finds the other repository still at the path it takes, and is refused.
    for (name, key) in repos
        .iter()
        .filter(|_| !cfg!(mutant_store_repo_moves_unordered))
    {
        let of_name = sqlx::query!(
            r#"SELECT id AS "id!: i64", path FROM repo WHERE name = ?"#,
            name
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = of_name.filter(|row| row.path != *key) {
            alias(conn, &row.path, row.id).await?;
            let park = format!("<parked {}>", row.id);
            sqlx::query!("UPDATE repo SET path = ? WHERE id = ?", park, row.id)
                .execute(&mut *conn)
                .await?;
            parked.push(*name);
        }
    }
    let mut new_root = false;
    for (name, key) in &repos {
        let (id, new) = named_repo(conn, name, key, parked.contains(name)).await?;
        new_root |= new;
        new_root |= merge_unnamed_items(conn, name, id).await?;
    }
    if new_root {
        reattribute(conn).await?;
    }
    Ok(())
}

/// Merges into row `into` (of `name`) each unnamed row that holds an item of `name`, and
/// keeps that row's path as an alias; whether any was merged.
async fn merge_unnamed_items(
    conn: &mut SqliteConnection,
    name: &str,
    into: i64,
) -> Result<bool, Error> {
    // Mutant (`mutant_store_repo_unnamed_items_kept`): an unnamed row keeps the name's older
    // items, which the loop never sees.
    if cfg!(mutant_store_repo_unnamed_items_kept) {
        return Ok(false);
    }
    let prefix = format!("{name}#");
    let rows = sqlx::query!(
        r#"SELECT r.id AS "id!: i64", r.path FROM repo r
           WHERE r.name IS NULL AND r.id <> ?1
             AND EXISTS (SELECT 1 FROM item i
                         WHERE i.repo_id = r.id
                           AND substr(i.item_key, 1, length(?2)) = ?2)"#,
        into,
        prefix,
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in &rows {
        merge_repo(conn, row.id, into).await?;
        alias(conn, &row.path, into).await?;
    }
    Ok(!rows.is_empty())
}

/// Records `path` as an alias of row `id` (a path it was at before).
async fn alias(conn: &mut SqliteConnection, path: &str, id: i64) -> Result<(), Error> {
    sqlx::query!(
        "INSERT INTO repo_alias (path, repo_id) VALUES (?, ?)
         ON CONFLICT (path) DO UPDATE SET repo_id = excluded.repo_id",
        path,
        id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The row of `name`, at `key` once written, and whether a root is new (messages attribute
/// differently). `parked`: the row of `name` left its old path already, and that path is an
/// alias of it.
async fn named_repo(
    conn: &mut SqliteConnection,
    name: &str,
    key: &str,
    parked: bool,
) -> Result<(i64, bool), Error> {
    let at_path = sqlx::query!(
        r#"SELECT id AS "id!: i64", name FROM repo WHERE path = ?"#,
        key
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(other) = at_path.as_ref().and_then(|row| row.name.as_deref())
        && other != name
    {
        return Err(Error::Corrupt(format!(
            "repositories {other} and {name} are both configured at {key}"
        )));
    }
    let of_name = sqlx::query!(
        r#"SELECT id AS "id!: i64", path FROM repo WHERE name = ?"#,
        name
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(of_name) = of_name else {
        // A configured path wins over an alias of another row.
        let dropped = sqlx::query!("DELETE FROM repo_alias WHERE path = ?", key)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0;
        if let Some(row) = at_path {
            sqlx::query!("UPDATE repo SET name = ? WHERE id = ?", name, row.id)
                .execute(&mut *conn)
                .await?;
            return Ok((row.id, dropped));
        }
        let id = sqlx::query_scalar!(
            r#"INSERT INTO repo (path, name) VALUES (?, ?) RETURNING id AS "id!: i64""#,
            key,
            name,
        )
        .fetch_one(&mut *conn)
        .await?;
        return Ok((id, true));
    };
    if of_name.path == key {
        return Ok((of_name.id, false));
    }
    // Mutant (`mutant_store_repo_new_row_on_move`): the name is ignored when the path differs,
    // so the new path keeps or gets its own row and the old row keeps the name.
    if cfg!(mutant_store_repo_new_row_on_move) {
        return unnamed_repo(conn, key).await;
    }
    let id = of_name.id;
    if !parked {
        alias(conn, &of_name.path, id).await?;
    }
    if let Some(row) = at_path {
        merge_repo(conn, row.id, id).await?;
    }
    sqlx::query!("DELETE FROM repo_alias WHERE path = ?", key)
        .execute(&mut *conn)
        .await?;
    sqlx::query!("UPDATE repo SET path = ? WHERE id = ?", key, id)
        .execute(&mut *conn)
        .await?;
    Ok((id, true))
}

/// Moves every row that points at repository `from` over to `into`, then deletes `from`. A
/// commit `into` holds already (by sha) is not duplicated: `from`'s copy is dropped.
async fn merge_repo(conn: &mut SqliteConnection, from: i64, into: i64) -> Result<(), Error> {
    sqlx::query!(
        r#"UPDATE repo SET
               remote = coalesce(remote, (SELECT remote FROM repo WHERE id = ?1)),
               default_branch = coalesce(default_branch,
                                         (SELECT default_branch FROM repo WHERE id = ?1))
           WHERE id = ?2"#,
        from,
        into,
    )
    .execute(&mut *conn)
    .await?;
    // Every table with a `repo_id`. `"commit"` and `run_commit` are unique by (repository,
    // sha): a sha `into` holds already keeps its row, and `from`'s copy is deleted below.
    for statement in [
        "UPDATE message SET repo_id = ?2 WHERE repo_id = ?1",
        "UPDATE annotation SET repo_id = ?2 WHERE repo_id = ?1",
        "UPDATE item SET repo_id = ?2 WHERE repo_id = ?1",
        "UPDATE flaky_test SET repo_id = ?2 WHERE repo_id = ?1",
        r#"UPDATE OR IGNORE "commit" SET repo_id = ?2 WHERE repo_id = ?1"#,
        "UPDATE OR IGNORE run_commit SET repo_id = ?2 WHERE repo_id = ?1",
        "UPDATE repo_alias SET repo_id = ?2 WHERE repo_id = ?1",
        // A subject both rows have an active proposal of keeps `into`'s: `from`'s expires (a
        // merge is not a lifecycle event, and the partial unique index would refuse the move).
        "UPDATE proposal SET state = 'expired'
         WHERE repo_id = ?1 AND state IN ('open', 'published', 'landed')
           AND dedup_key IN (SELECT dedup_key FROM proposal
                             WHERE repo_id = ?2 AND state IN ('open', 'published', 'landed'))",
        "UPDATE proposal SET repo_id = ?2 WHERE repo_id = ?1",
    ] {
        sqlx::query(statement)
            .bind(from)
            .bind(into)
            .execute(&mut *conn)
            .await?;
    }
    for statement in [
        r#"DELETE FROM "commit" WHERE repo_id = ?"#,
        "DELETE FROM run_commit WHERE repo_id = ?",
        "DELETE FROM repo WHERE id = ?",
    ] {
        sqlx::query(statement)
            .bind(from)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Attributes every stored message to its root again: a new root may contain, or lie deeper
/// than, the roots messages were attributed to.
async fn reattribute(conn: &mut SqliteConnection) -> Result<(), Error> {
    let repos = repos(conn).await?;
    let messages = sqlx::query!(r#"SELECT id AS "id!: i64", cwd, repo_id FROM message"#)
        .fetch_all(&mut *conn)
        .await?;
    for message in messages {
        let repo = repo_of(&message.cwd, &repos);
        if repo != message.repo_id {
            sqlx::query!(
                "UPDATE message SET repo_id = ? WHERE id = ?",
                repo,
                message.id
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn insert_commit(
    conn: &mut SqliteConnection,
    repo: &str,
    commit: &Commit,
) -> Result<(), Error> {
    let repo_id = upsert_repo(
        conn,
        &Repo {
            name: None,
            path: repo.to_owned(),
            remote: None,
            default_branch: None,
        },
    )
    .await?;
    let added = to_db(commit.lines_added, "lines added")?;
    let removed = to_db(commit.lines_removed, "lines removed")?;
    let trailers = serde_json::to_string(&commit.trailers)
        .map_err(|e| Error::Corrupt(format!("trailers of commit {}: {e}", commit.sha)))?;
    sqlx::query!(
        r#"INSERT INTO "commit"
               (repo_id, sha, ts_ms, author, lines_added, lines_removed, trailers_json)
           VALUES (?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (repo_id, sha) DO NOTHING"#,
        repo_id,
        commit.sha,
        commit.ts_ms,
        commit.author,
        added,
        removed,
        trailers,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn upsert_session(
    conn: &mut SqliteConnection,
    provider: &str,
    meta: &WindowMeta,
) -> Result<(), Error> {
    let key = &meta.window.key;
    let id = session_id(conn, provider, key).await?;
    let root = WindowKey {
        session_id: key.session_id.clone(),
        agent_id: None,
    };
    let root = session_id(conn, provider, &root).await?;
    let parent = match &meta.window.parent {
        Some(parent) => Some(session_id(conn, provider, parent).await?),
        None => None,
    };
    let kind = match meta.window.kind {
        WindowKind::Run => "run",
        WindowKind::Interactive => "interactive",
    };
    // A session bosun spawned stays a run session whatever the transcript says.
    sqlx::query!(
        "UPDATE session SET
             kind = CASE WHEN ? = 'run' OR kind = 'run' OR EXISTS (
                     SELECT 1 FROM run r
                     WHERE r.provider = session.provider AND r.session_key = session.session_key)
                 THEN 'run' ELSE 'interactive' END,
             root_id = ?,
             parent_id = coalesce(?, parent_id),
             agent_type = coalesce(?, agent_type),
             spawn_depth = coalesce(?, spawn_depth),
             spawn_tool_use_id = coalesce(?, spawn_tool_use_id)
         WHERE id = ?",
        kind,
        root,
        parent,
        meta.agent_type,
        meta.spawn_depth,
        meta.spawned_by,
        id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The media part of a result as the store keeps it: known once a result of a parser that
/// measures it was seen (one with a time, or with media), NULL otherwise: a call with no
/// result yet, or a row of a parser that predates the column (Claude's version 3 or less,
/// until `bosun reingest`).
fn media_known(result_ts_ms: Option<i64>, media_bytes: u64) -> Option<u64> {
    (result_ts_ms.is_some() || media_bytes > 0).then_some(media_bytes)
}

async fn apply_tool_result(conn: &mut SqliteConnection, result: &ToolResult) -> Result<(), Error> {
    let calls = sqlx::query!(
        r#"SELECT tc.id AS "id!: i64", tc.result_bytes, tc.is_error, tc.call_ts_ms,
                  tc.result_ts_ms, tc.result_media_bytes
           FROM tool_call tc JOIN message m ON m.id = tc.message_id
           WHERE m.provider = ? AND tc.tool_use_key = ?"#,
        result.provider,
        result.tool_use_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    for call in calls {
        // The rule of `merge` for a call seen twice: the larger size, the error if either.
        let bytes = from_db(call.result_bytes, "tool result bytes")?.max(result.result_bytes);
        let bytes = to_db(bytes, "tool result bytes")?;
        let is_error = call.is_error != 0 || result.is_error;
        // The earliest known time of the result, and the larger media part (#210).
        let result_ts_ms = if cfg!(mutant_ingest_result_time_from_call) {
            // Mutant `mutant_ingest_result_time_from_call`: a result's time is its call's.
            call.call_ts_ms
        } else {
            earliest(call.result_ts_ms, result.ts_ms)
        };
        let stored_media = match call.result_media_bytes {
            Some(media) => Some(from_db(media, "tool result media bytes")?),
            None => None,
        };
        let media = match (stored_media, media_known(result.ts_ms, result.media_bytes)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let media = media
            .map(|media| to_db(media, "tool result media bytes"))
            .transpose()?;
        sqlx::query!(
            "UPDATE tool_call SET result_bytes = ?, is_error = ?, result_ts_ms = ?,
                 result_media_bytes = ?
             WHERE id = ?",
            bytes,
            is_error,
            result_ts_ms,
            media,
            call.id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_file_touch(conn: &mut SqliteConnection, touch: &FileTouch) -> Result<(), Error> {
    let Some(call) = sqlx::query!(
        r#"SELECT tc.id AS "id!: i64", r.path AS "repo?: String"
           FROM tool_call tc
           JOIN message m ON m.id = tc.message_id
           LEFT JOIN repo r ON r.id = m.repo_id
           WHERE m.provider = ? AND m.message_key = ? AND tc.tool_use_key = ?"#,
        touch.provider,
        touch.message_id,
        touch.tool_use_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(());
    };
    // Relative to the message's repository when the file is inside it (`.` for its root). A
    // Unix key keeps `..`, which may lead out of the repository: a confined path with one is
    // not inside.
    let inside = call.repo.as_deref().and_then(|root| {
        strip_root(&touch.path, root, style_of(root))
            .filter(|rest| !touch.confined || !rest.split(['/', '\\']).any(|part| part == ".."))
    });
    let path = match inside {
        Some("") => ".",
        Some(rest) => rest,
        // A path taken out of a command's text is stored only inside the repository (#2).
        None if touch.confined => return Ok(()),
        // Absolute otherwise.
        None => touch.path.as_str(),
    };
    let kind = touch.kind.as_str();
    sqlx::query!(
        "INSERT INTO file_touch (tool_call_id, path, kind) VALUES (?, ?, ?)
         ON CONFLICT (tool_call_id, path, kind) DO NOTHING",
        call.id,
        path,
        kind,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn replace_session_check(
    conn: &mut SqliteConnection,
    check: &SessionCheck,
    recorded_ms: i64,
) -> Result<(), Error> {
    let root = WindowKey {
        session_id: check.session_id.clone(),
        agent_id: None,
    };
    let root = session_id(conn, &check.provider, &root).await?;
    sqlx::query!("DELETE FROM session_check WHERE root_session_id = ?", root)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "INSERT INTO session_check_window (root_session_id, start_ms, end_ms, total_cost_usd)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (root_session_id) DO UPDATE SET
             start_ms = excluded.start_ms,
             end_ms = excluded.end_ms,
             total_cost_usd = excluded.total_cost_usd",
        root,
        check.start_ms,
        check.end_ms,
        check.total_cost_usd,
    )
    .execute(&mut *conn)
    .await?;
    for (model, totals) in &check.models {
        let t = &totals.tokens;
        let input = to_db(t.input, "input tokens")?;
        let written = to_db(t.cache_write_5m + t.cache_write_1h, "cache write tokens")?;
        let read = to_db(t.cache_read, "cache read tokens")?;
        let output = to_db(t.output, "output tokens")?;
        // A cost the provider did not give stays missing.
        let cost = totals.cost_usd;
        sqlx::query!(
            "INSERT INTO session_check (root_session_id, model, input_tokens, cache_write_tokens,
                                        cache_read_tokens, output_tokens, cost_usd, recorded_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            root,
            model,
            input,
            written,
            read,
            output,
            cost,
            recorded_ms,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn upsert_source_file(conn: &mut SqliteConnection, file: &SourceFile) -> Result<(), Error> {
    let size = to_db(file.size, "file size")?;
    let offset = to_db(file.parsed_offset, "parsed offset")?;
    let lines = file
        .parsed_lines
        .map(|n| to_db(n, "parsed lines"))
        .transpose()?;
    let unknown = file
        .unknown_lines
        .map(|n| to_db(n, "unknown lines"))
        .transpose()?;
    let usage = file
        .usage_lines
        .map(|n| to_db(n, "usage lines"))
        .transpose()?;
    let unbilled = file
        .unbilled_lines
        .map(|n| to_db(n, "unbilled lines"))
        .transpose()?;
    sqlx::query!(
        "INSERT INTO source_file
             (provider, file_key, path, size, parsed_offset, parser_version, last_line_hash,
              parsed_lines, unknown_lines, usage_lines, unbilled_lines, last_ts_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (provider, file_key) DO UPDATE SET
             path = excluded.path,
             size = excluded.size,
             parsed_offset = excluded.parsed_offset,
             parser_version = excluded.parser_version,
             last_line_hash = excluded.last_line_hash,
             parsed_lines = excluded.parsed_lines,
             unknown_lines = excluded.unknown_lines,
             usage_lines = excluded.usage_lines,
             unbilled_lines = excluded.unbilled_lines,
             last_ts_ms = excluded.last_ts_ms",
        file.provider,
        file.file_key,
        file.path,
        size,
        offset,
        file.parser_version,
        file.last_line_hash,
        lines,
        unknown,
        usage,
        unbilled,
        file.last_ts_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn source_file_id(
    conn: &mut SqliteConnection,
    provider: &str,
    file_key: &str,
) -> Result<i64, Error> {
    sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM source_file WHERE provider = ? AND file_key = ?"#,
        provider,
        file_key
    )
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| Error::UnknownSourceFile {
        provider: provider.to_owned(),
        file_key: file_key.to_owned(),
    })
}

async fn session_id(
    conn: &mut SqliteConnection,
    provider: &str,
    window: &WindowKey,
) -> Result<i64, Error> {
    let found = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM session
         WHERE provider = ? AND session_key = ? AND ifnull(agent_key, '') = ifnull(?, '')"#,
        provider,
        window.session_id,
        window.agent_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = found {
        return Ok(id);
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO session (provider, session_key, agent_key) VALUES (?, ?, ?)
           RETURNING id AS "id!: i64""#,
        provider,
        window.session_id,
        window.agent_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    // The run row came first: the transcript of a spawned session is a run session.
    work::mark_run_session(conn, provider, &window.session_id).await?;
    Ok(id)
}

async fn model_id(conn: &mut SqliteConnection, provider: &str, name: &str) -> Result<i64, Error> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO model (provider, name) VALUES (?, ?)
           ON CONFLICT (provider, name) DO UPDATE SET name = excluded.name
           RETURNING id AS "id!: i64""#,
        provider,
        name,
    )
    .fetch_one(conn)
    .await?)
}

/// Read-modify-write: the stored message (if any) and the incoming record are folded by
/// [`bosun_core::merge`], and the result replaces the row and upserts every tool call.
async fn upsert_message(
    conn: &mut SqliteConnection,
    incoming: Message,
    parser_version: u32,
) -> Result<(), Error> {
    let merged = match read::load_message(conn, &incoming.provider, &incoming.id).await? {
        Some(existing) => merge(existing, incoming)?,
        None => incoming,
    };
    let m = &merged;
    let session = session_id(conn, &m.provider, &m.origin.window).await?;
    let file = source_file_id(conn, &m.provider, &m.origin.file).await?;
    let model = model_id(conn, &m.provider, &m.model).await?;
    let repo = repo_of(&m.cwd, &repos(conn).await?);
    let t = &m.usage.tokens;
    let input = to_db(t.input, "input tokens")?;
    let five_minute = to_db(t.cache_write_5m, "cache write tokens")?;
    let one_hour = to_db(t.cache_write_1h, "cache write tokens")?;
    let read = to_db(t.cache_read, "cache read tokens")?;
    let output = to_db(t.output, "output tokens")?;
    let usage = m.usage.raw.to_string();
    // The window that owned the message before this record: the merge may move it to an
    // earlier one, whose span then no longer counts it.
    let previous = sqlx::query_scalar!(
        r#"SELECT session_id AS "session_id!: i64" FROM message
           WHERE provider = ? AND message_key = ?"#,
        m.provider,
        m.id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO message (
               provider, message_key, session_id, source_file_id, ts_ms, model_id, cwd, repo_id,
               input_tokens, cache_write_5m_tokens, cache_write_1h_tokens, cache_read_tokens,
               output_tokens, usage_json, parser_version)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (provider, message_key) DO UPDATE SET
               session_id = excluded.session_id,
               source_file_id = excluded.source_file_id,
               ts_ms = excluded.ts_ms,
               model_id = excluded.model_id,
               cwd = excluded.cwd,
               repo_id = excluded.repo_id,
               input_tokens = excluded.input_tokens,
               cache_write_5m_tokens = excluded.cache_write_5m_tokens,
               cache_write_1h_tokens = excluded.cache_write_1h_tokens,
               cache_read_tokens = excluded.cache_read_tokens,
               output_tokens = excluded.output_tokens,
               usage_json = excluded.usage_json,
               parser_version = excluded.parser_version
           RETURNING id AS "id!: i64""#,
        m.provider,
        m.id,
        session,
        file,
        m.origin.ts_ms,
        model,
        m.cwd,
        repo,
        input,
        five_minute,
        one_hour,
        read,
        output,
        usage,
        parser_version,
    )
    .fetch_one(&mut *conn)
    .await?;

    upsert_tool_calls(conn, id, m, repo.is_some()).await?;
    update_span(conn, session).await?;
    if let Some(previous) = previous.filter(|&p| p != session) {
        update_span(conn, previous).await?;
    }
    Ok(())
}

/// Writes the tool calls of message `m`, stored as row `id`. `attributed` says whether the
/// message is attributed to a repository.
async fn upsert_tool_calls(
    conn: &mut SqliteConnection,
    id: i64,
    m: &Message,
    attributed: bool,
) -> Result<(), Error> {
    // A command's text and its hash reach the database only for a message attributed to a
    // repository: of a message in no repository nothing of the text is kept (#2, #76, #210).
    // Every repository row comes from the configuration, so an attributed message is one of a
    // configured repository.
    // Mutant `mutant_ingest_command_text_outside_repo`: the text is kept whatever the attribution.
    let keep_command = attributed || cfg!(mutant_ingest_command_text_outside_repo);
    for (key, call) in &m.tool_calls {
        let input_bytes = to_db(call.input_bytes, "tool input bytes")?;
        let result_bytes = to_db(call.result_bytes, "tool result bytes")?;
        let (command_hash, command_text) = if keep_command {
            (call.command_hash.as_deref(), call.command_text.as_deref())
        } else {
            (None, None)
        };
        let media = media_known(call.result_ts_ms, call.result_media_bytes)
            .map(|media| to_db(media, "tool result media bytes"))
            .transpose()?;
        sqlx::query!(
            "INSERT INTO tool_call
                 (message_id, tool_use_key, name, target, input_bytes, result_bytes, is_error,
                  command_hash, command_text, call_ts_ms, result_ts_ms, result_media_bytes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (message_id, tool_use_key) DO UPDATE SET
                 name = excluded.name,
                 target = excluded.target,
                 input_bytes = excluded.input_bytes,
                 result_bytes = excluded.result_bytes,
                 is_error = excluded.is_error,
                 command_hash = excluded.command_hash,
                 command_text = excluded.command_text,
                 call_ts_ms = excluded.call_ts_ms,
                 result_ts_ms = excluded.result_ts_ms,
                 result_media_bytes = excluded.result_media_bytes",
            id,
            key,
            call.name,
            call.target,
            input_bytes,
            result_bytes,
            call.is_error,
            command_hash,
            command_text,
            call.call_ts_ms,
            call.result_ts_ms,
            media,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Sets the span of window `session` to the first and last time of the messages it owns now
/// (#95), or clears it when it owns none. Recomputed from the rows rather than widened, so the
/// span is the same whatever order the records were merged in (#17), and a rescan or a
/// reingest of the same records leaves it as it was.
async fn update_span(conn: &mut SqliteConnection, session: i64) -> Result<(), Error> {
    if cfg!(mutant_ingest_no_session_span) {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE session SET
             started_ms = (SELECT min(ts_ms) FROM message WHERE session_id = session.id),
             ended_ms = (SELECT max(ts_ms) FROM message WHERE session_id = session.id)
         WHERE id = ?",
        session,
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn upsert_gap(
    conn: &mut SqliteConnection,
    provider: &str,
    file_key: &str,
    gap: &Gap,
    parser_version: u32,
) -> Result<(), Error> {
    let file = source_file_id(conn, provider, file_key).await?;
    let offset = to_db(gap.offset, "gap offset")?;
    let length = to_db(gap.length, "gap length")?;
    let keys = serde_json::Value::from(gap.top_level_keys.clone()).to_string();
    sqlx::query!(
        "INSERT INTO ingest_gap (source_file_id, byte_offset, length, record_type,
                                 top_level_keys_json, reason, parser_version)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (source_file_id, byte_offset) DO UPDATE SET
             length = excluded.length,
             record_type = excluded.record_type,
             top_level_keys_json = excluded.top_level_keys_json,
             reason = excluded.reason,
             parser_version = excluded.parser_version",
        file,
        offset,
        length,
        gap.record_type,
        keys,
        gap.reason,
        parser_version,
    )
    .execute(conn)
    .await?;
    Ok(())
}

struct RateColumns {
    input: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    cache_read: i64,
    output: i64,
}

fn rate_columns(rates: &Rates) -> Result<RateColumns, Error> {
    Ok(RateColumns {
        input: to_db(rates.input, "price")?,
        cache_write_5m: to_db(rates.cache_write_5m, "price")?,
        cache_write_1h: to_db(rates.cache_write_1h, "price")?,
        cache_read: to_db(rates.cache_read, "price")?,
        output: to_db(rates.output, "price")?,
    })
}

async fn change_price(conn: &mut SqliteConnection, change: &PriceChange) -> Result<(), Error> {
    let threshold = change
        .prompt_threshold
        .map(|t| to_db(t, "prompt threshold"))
        .transpose()?;
    let current = sqlx::query!(
        r#"SELECT id AS "id!: i64", valid_from_ms, is_reference FROM price
           WHERE model = ? AND prompt_threshold IS ? AND valid_to_ms IS NULL"#,
        change.model,
        threshold,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let is_reference = match current {
        Some(row) if row.valid_from_ms >= change.from_ms => {
            return Err(Error::PriceChange(format!(
                "the current row of {} starts at {} ms, not before {} ms",
                change.model, row.valid_from_ms, change.from_ms
            )));
        }
        Some(row) => {
            sqlx::query!(
                "UPDATE price SET valid_to_ms = ? WHERE id = ?",
                change.from_ms,
                row.id
            )
            .execute(&mut *conn)
            .await?;
            row.is_reference
        }
        None => 0,
    };
    let version =
        sqlx::query_scalar!(r#"SELECT ifnull(max(version), 0) + 1 AS "v!: i64" FROM price"#)
            .fetch_one(&mut *conn)
            .await?;
    let r = rate_columns(&change.rates)?;
    sqlx::query!(
        "INSERT INTO price (version, model, valid_from_ms, valid_to_ms, prompt_threshold,
                            input, cache_write_5m, cache_write_1h, cache_read, output,
                            is_reference)
         VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?)",
        version,
        change.model,
        change.from_ms,
        threshold,
        r.input,
        r.cache_write_5m,
        r.cache_write_1h,
        r.cache_read,
        r.output,
        is_reference,
    )
    .execute(&mut *conn)
    .await?;
    // The rows must still form a valid table; otherwise the batch rolls back.
    read::load_price_table(conn).await?;
    Ok(())
}

/// Fills an empty price table with [`bosun_core::default_table`] as version 1. A table that
/// has rows is left alone: a price set by the user is never overwritten.
pub(crate) async fn seed_prices(conn: &mut SqliteConnection) -> Result<(), Error> {
    let rows = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM price"#)
        .fetch_one(&mut *conn)
        .await?;
    if rows > 0 {
        return Ok(());
    }
    let table = default_table();
    let mut tx = conn.begin().await?;
    for row in table.rows() {
        let threshold = row
            .prompt_threshold
            .map(|t| to_db(t, "prompt threshold"))
            .transpose()?;
        let r = rate_columns(&row.rates)?;
        let is_reference = row.model == table.reference_model();
        sqlx::query!(
            "INSERT INTO price (version, model, valid_from_ms, valid_to_ms, prompt_threshold,
                                input, cache_write_5m, cache_write_1h, cache_read, output,
                                is_reference)
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            row.model,
            row.valid_from_ms,
            row.valid_to_ms,
            threshold,
            r.input,
            r.cache_write_5m,
            r.cache_write_1h,
            r.cache_read,
            r.output,
            is_reference,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use bosun_core::{Origin, Tokens, ToolCall, Usage};

    use super::*;
    use crate::{Options, Store};

    pub(crate) const PROVIDER: &str = "claude";

    pub(crate) fn source_file(key: &str) -> Write {
        Write::SourceFile(SourceFile {
            provider: PROVIDER.to_owned(),
            file_key: key.to_owned(),
            path: format!("projects/p/{key}.jsonl"),
            size: 100,
            parsed_offset: 100,
            parser_version: 1,
            last_line_hash: None,
            parsed_lines: Some(4),
            unknown_lines: Some(0),
            usage_lines: Some(0),
            unbilled_lines: Some(0),
            last_ts_ms: None,
        })
    }

    /// A record of message `id` in file `file`, with tool calls named by their ids.
    pub(crate) fn record(id: &str, file: &str, ts_ms: i64, output: u64, tools: &[&str]) -> Message {
        Message {
            provider: PROVIDER.to_owned(),
            id: id.to_owned(),
            origin: Origin {
                ts_ms,
                file: file.to_owned(),
                window: WindowKey {
                    session_id: format!("session-{file}"),
                    agent_id: None,
                },
            },
            model: "claude-opus-5-5".to_owned(),
            cwd: "/work/repo".to_owned(),
            usage: Usage {
                tokens: Tokens {
                    input: 10,
                    cache_write_5m: 1_000,
                    cache_write_1h: 2_000,
                    cache_read: 50_000,
                    output,
                },
                raw: serde_json::json!({ "output_tokens": output, "service_tier": "standard" }),
            },
            tool_calls: tools
                .iter()
                .map(|t| {
                    let call = ToolCall {
                        name: "Read".to_owned(),
                        target: Some(format!("src/{t}.rs")),
                        input_bytes: 40,
                        ..ToolCall::default()
                    };
                    ((*t).to_owned(), call)
                })
                .collect(),
        }
    }

    pub(crate) fn write(message: Message) -> Write {
        Write::Message {
            message,
            parser_version: 1,
        }
    }

    async fn count(store: &Store, sql_like: &str) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!: i64" FROM message WHERE message_key LIKE ?"#,
            sql_like
        )
        .fetch_one(store.reader().pool())
        .await
        .unwrap()
    }

    const ABORT_TEST: &str = "writer::tests::an_abort_mid_batch_leaves_the_whole_batch_or_none";
    const ABORT_CHILD: &str = "BOSUN_STORE_ABORT_CHILD_DB";
    const BATCH: usize = 10;
    const ABORT_AFTER: usize = 4;

    /// The child half: commits one batch, then aborts after row `ABORT_AFTER` of the next.
    async fn abort_child(db: &Path) {
        let store = Store::open(db, Options::default()).await.unwrap();
        let first: Vec<Write> = std::iter::once(source_file("f1"))
            .chain((0..BATCH).map(|i| write(record(&format!("first-{i}"), "f1", 1, 5, &[]))))
            .collect();
        store.writer().write(first).await.unwrap();

        ABORT_AFTER_ROW.store(ABORT_AFTER, Ordering::SeqCst);
        let second: Vec<Write> = (0..BATCH)
            .map(|i| write(record(&format!("second-{i}"), "f1", 2, 5, &["t"])))
            .collect();
        let _ = store.writer().write(second).await;
        // Reaching this line means the failpoint did not fire; the parent sees a clean exit.
        eprintln!("failpoint not reached");
    }

    #[tokio::test]
    async fn an_abort_mid_batch_leaves_the_whole_batch_or_none() {
        if let Some(db) = std::env::var_os(ABORT_CHILD) {
            abort_child(Path::new(&db)).await;
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bosun.db");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([ABORT_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(ABORT_CHILD, &db)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(!child.status.success(), "the child did not abort: {stderr}");
        assert!(
            stderr.contains(&format!("failpoint: abort after row {ABORT_AFTER}")),
            "the child failed before the failpoint: {stderr}"
        );

        let store = Store::open(&db, Options::default()).await.unwrap();
        let batch = i64::try_from(BATCH).unwrap();
        assert_eq!(count(&store, "first-%").await, batch, "the committed batch");
        let second = count(&store, "second-%").await;
        assert!(
            second == 0 || second == batch,
            "the aborted batch is partial: {second} of {BATCH} rows"
        );
        store.close().await.unwrap();
    }

    /// The span of every window, by SQL: (session id, start, end).
    async fn spans(store: &Store) -> Vec<(String, Option<i64>, Option<i64>)> {
        sqlx::query_as("SELECT session_key, started_ms, ended_ms FROM session ORDER BY session_key")
            .fetch_all(store.reader().pool())
            .await
            .unwrap()
    }

    /// #95: a window's span is its first and last message, in any merge order; a message that
    /// moves to an earlier window leaves the span of the one it left.
    #[tokio::test]
    async fn a_windows_span_is_its_first_and_last_message_in_any_order() {
        // `m` is seen in f2 first, then in f1 at an earlier time, which then owns it.
        let records = [
            record("a", "f2", 500, 1, &[]),
            record("m", "f2", 900, 1, &[]),
            record("b", "f1", 300, 1, &[]),
            record("m", "f1", 100, 1, &[]),
        ];
        let want = [
            ("session-f1".to_owned(), Some(100), Some(300)),
            ("session-f2".to_owned(), Some(500), Some(500)),
        ];
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 3, 0, 1]] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("bosun.db"), Options::default())
                .await
                .unwrap();
            let batch: Vec<Write> = [source_file("f1"), source_file("f2")]
                .into_iter()
                .chain(order.iter().map(|&i| write(records[i].clone())))
                .collect();
            store.writer().write(batch.clone()).await.unwrap();
            assert_eq!(spans(&store).await, want, "order {order:?}");
            // A rescan of the same records leaves the spans as they are.
            store.writer().write(batch).await.unwrap();
            assert_eq!(spans(&store).await, want, "order {order:?}, rescanned");
            store.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn records_of_one_message_merge_into_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        // A split message: three records, the last has the final output; the same message
        // also appears later in a second file (a continued session).
        let batch = vec![
            source_file("f1"),
            source_file("f2"),
            write(record("m", "f2", 900, 120, &["c"])),
            write(record("m", "f1", 100, 3, &["a"])),
            write(record("m", "f1", 100, 80, &["b"])),
        ];
        store.writer().write(batch.clone()).await.unwrap();
        // Ingesting the same records again changes nothing.
        store.writer().write(batch).await.unwrap();

        let stored = store
            .reader()
            .message(PROVIDER, "m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.usage.tokens.output, 120);
        assert_eq!(
            stored.origin.file, "f1",
            "the earliest file owns the message"
        );
        assert_eq!(stored.origin.ts_ms, 100);
        let ids: Vec<&str> = stored.tool_calls.keys().map(String::as_str).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(count(&store, "%").await, 1);
        let calls = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM tool_call"#)
            .fetch_one(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(calls, 3);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_rolls_back_its_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let batch = vec![
            source_file("f1"),
            write(record("ok", "f1", 1, 1, &[])),
            // No source file registered for f9.
            write(record("bad", "f9", 1, 1, &[])),
        ];
        let err = store.writer().write(batch).await.unwrap_err();
        assert!(matches!(err, Error::UnknownSourceFile { .. }), "{err}");
        assert_eq!(count(&store, "%").await, 0);
        // The writer keeps serving after a failed batch.
        store
            .writer()
            .write(vec![
                source_file("f1"),
                write(record("ok", "f1", 1, 1, &[])),
            ])
            .await
            .unwrap();
        assert_eq!(count(&store, "%").await, 1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_new_repository_root_re_attributes_stored_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let mut nested = record("m2", "f1", 2, 1, &[]);
        nested.cwd = "/work/repo/sub/x".to_owned();
        store
            .writer()
            .write(vec![
                Write::Repo(Repo {
                    name: None,
                    path: "/work/repo".to_owned(),
                    remote: None,
                    default_branch: None,
                }),
                source_file("f1"),
                write(record("m1", "f1", 1, 1, &[])),
                write(nested),
            ])
            .await
            .unwrap();
        let repos = || async {
            sqlx::query!(
                r#"SELECT m.message_key, r.path AS "repo?: String"
                   FROM message m LEFT JOIN repo r ON r.id = m.repo_id
                   ORDER BY m.message_key"#
            )
            .fetch_all(store.reader().pool())
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.message_key, row.repo))
            .collect::<Vec<_>>()
        };
        let repo = Some("/work/repo".to_owned());
        assert_eq!(
            repos().await,
            [("m1".to_owned(), repo.clone()), ("m2".to_owned(), repo)]
        );

        // A deeper root, registered later, takes the messages under it; registering it again
        // changes nothing.
        let deeper = Write::Repo(Repo {
            name: None,
            path: "/work//repo/sub/".to_owned(),
            remote: None,
            default_branch: None,
        });
        store.writer().write(vec![deeper.clone()]).await.unwrap();
        store.writer().write(vec![deeper]).await.unwrap();
        assert_eq!(
            repos().await,
            [
                ("m1".to_owned(), Some("/work/repo".to_owned())),
                ("m2".to_owned(), Some("/work/repo/sub".to_owned())),
            ]
        );
        store.close().await.unwrap();
    }

    /// #132: a configured repository is its name. Written at a new path, its row moves there:
    /// an unnamed row the new path had (registered by path before the move) is merged into it
    /// without a duplicate commit, and the old path stays an alias, so a working directory
    /// under either path attributes to the one repository, and neither path makes a new row.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn a_moved_repository_keeps_its_row_and_both_paths_attribute_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let named = |path: &str| {
            Write::Repo(Repo {
                name: Some("demo".to_owned()),
                path: path.to_owned(),
                remote: None,
                default_branch: None,
            })
        };
        let unnamed = |path: &str| {
            Write::Repo(Repo {
                name: None,
                path: path.to_owned(),
                remote: None,
                default_branch: None,
            })
        };
        let commit = |repo: &str, sha: &str| Write::Commit {
            repo: repo.to_owned(),
            commit: Commit {
                sha: sha.to_owned(),
                ts_ms: 1_000,
                author: "A <a@example.com>".to_owned(),
                lines_added: 1,
                lines_removed: 0,
                trailers: std::collections::BTreeMap::new(),
            },
        };
        let message = |id: &str, cwd: &str| {
            let mut message = record(id, "f1", 1, 1, &[]);
            message.cwd = cwd.to_owned();
            write(message)
        };
        store
            .writer()
            .write(vec![
                named("/w/old"),
                Write::Item(crate::NewItem {
                    item: ItemId::new("demo#1"),
                    name: "demo".to_owned(),
                    repo: "/w/old".to_owned(),
                    card_key: None,
                    at_ms: 1,
                }),
                source_file("f1"),
                message("m-old", "/w/old/src"),
                commit("/w/old", "aaa"),
                commit("/w/old", "both"),
                unnamed("/w/new"),
                message("m-new", "/w/new"),
                commit("/w/new", "bbb"),
                commit("/w/new", "both"),
            ])
            .await
            .unwrap();
        assert_eq!(store.reader().repos().await.unwrap().len(), 2);

        store.writer().write(vec![named("/w/new")]).await.unwrap();
        let demo = vec![crate::RepoRow {
            name: Some("demo".to_owned()),
            path: "/w/new".to_owned(),
        }];
        assert_eq!(store.reader().repos().await.unwrap(), demo);
        let of_messages = || async {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT m.message_key, r.name FROM message m LEFT JOIN repo r ON r.id = m.repo_id
                 ORDER BY m.message_key",
            )
            .fetch_all(store.reader().pool())
            .await
            .unwrap()
        };
        let on_demo = |keys: &[&str]| {
            keys.iter()
                .map(|k| ((*k).to_owned(), Some("demo".to_owned())))
                .collect::<Vec<_>>()
        };
        assert_eq!(of_messages().await, on_demo(&["m-new", "m-old"]));
        let items = store.reader().items(false).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].repo, "/w/new");
        assert_eq!(items[0].repo_name.as_deref(), Some("demo"));
        assert_eq!(
            store.reader().commit_shas("/w/new").await.unwrap(),
            ["aaa", "bbb", "both"]
        );
        let commits: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "commit""#)
            .fetch_one(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(commits, 3);

        // A working directory under the old path still attributes to the repository, and so
        // does every message once an unrelated root re-attributes them all.
        store
            .writer()
            .write(vec![message("m-later", "/w/old/x")])
            .await
            .unwrap();
        assert_eq!(of_messages().await, on_demo(&["m-later", "m-new", "m-old"]));
        store
            .writer()
            .write(vec![unnamed("/w/other")])
            .await
            .unwrap();
        assert_eq!(of_messages().await, on_demo(&["m-later", "m-new", "m-old"]));

        // The old path, by path, is the repository: it creates no row.
        store
            .writer()
            .write(vec![commit("/w/old", "ccc"), unnamed("/w/old")])
            .await
            .unwrap();
        let repos = store.reader().repos().await.unwrap();
        assert_eq!(
            repos,
            [
                demo[0].clone(),
                crate::RepoRow {
                    name: None,
                    path: "/w/other".to_owned(),
                }
            ]
        );
        assert_eq!(
            store.reader().commit_shas("/w/new").await.unwrap(),
            ["aaa", "bbb", "both", "ccc"]
        );
        store.close().await.unwrap();
    }

    fn configured(repos: &[(&str, &str)]) -> Write {
        Write::Configured(
            repos
                .iter()
                .map(|(name, path)| crate::Configured {
                    name: (*name).to_owned(),
                    path: (*path).to_owned(),
                })
                .collect(),
        )
    }

    fn new_item(key: &str, name: &str, repo: &str) -> Write {
        Write::Item(crate::NewItem {
            item: ItemId::new(key),
            name: name.to_owned(),
            repo: repo.to_owned(),
            card_key: None,
            at_ms: 1,
        })
    }

    /// The (name, path) of every row, and the repository name of each item.
    async fn rows_and_items(store: &Store) -> (Vec<(Option<String>, String)>, Vec<String>) {
        let rows = store
            .reader()
            .repos()
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.name, r.path))
            .collect();
        let items = store
            .reader()
            .items(false)
            .await
            .unwrap()
            .into_iter()
            .map(|i| format!("{}@{}", i.item.id, i.repo_name.unwrap_or_default()))
            .collect();
        (rows, items)
    }

    /// #132: a repository renamed in the configuration at the same path keeps its row: its
    /// items and messages stay on it, under the new name.
    #[tokio::test]
    async fn a_renamed_repository_keeps_its_row_with_its_items_and_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let mut message = record("m", "f1", 1, 1, &[]);
        message.cwd = "/w/repo/src".to_owned();
        store
            .writer()
            .write(vec![
                configured(&[("demo", "/w/repo")]),
                new_item("demo#1", "demo", "/w/repo"),
                source_file("f1"),
                write(message),
            ])
            .await
            .unwrap();

        store
            .writer()
            .write(vec![configured(&[("demo2", "/w/repo")])])
            .await
            .unwrap();
        let (rows, items) = rows_and_items(&store).await;
        assert_eq!(rows, [(Some("demo2".to_owned()), "/w/repo".to_owned())]);
        assert_eq!(items, ["demo#1@demo2"]);
        let of_message: Option<String> =
            sqlx::query_scalar("SELECT r.name FROM message m JOIN repo r ON r.id = m.repo_id")
                .fetch_one(store.reader().pool())
                .await
                .unwrap();
        assert_eq!(of_message.as_deref(), Some("demo2"));
        store.close().await.unwrap();
    }

    /// #132: two repositories that swap paths in one edit both move, whatever the order of
    /// the set: each keeps its row and items.
    #[tokio::test]
    async fn two_repositories_that_swap_paths_keep_their_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        store
            .writer()
            .write(vec![
                configured(&[("a", "/w/q"), ("b", "/w/p")]),
                new_item("a#1", "a", "/w/q"),
                new_item("b#1", "b", "/w/p"),
            ])
            .await
            .unwrap();
        store
            .writer()
            .write(vec![configured(&[("a", "/w/p"), ("b", "/w/q")])])
            .await
            .unwrap();
        let (rows, items) = rows_and_items(&store).await;
        assert_eq!(
            rows,
            [
                (Some("a".to_owned()), "/w/p".to_owned()),
                (Some("b".to_owned()), "/w/q".to_owned()),
            ]
        );
        assert_eq!(items, ["a#1@a", "b#1@b"]);
        // Neither path each left stays an alias of it: the other repository is configured
        // there.
        let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_alias")
            .fetch_one(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(aliases, 0);
        store.close().await.unwrap();
    }

    /// #132: an unnamed row that holds older items of a name (migration 16 named only the row
    /// of the newest item) is merged into the named row, so the loop sees those items; its
    /// path becomes an alias.
    #[tokio::test]
    async fn an_unnamed_row_with_a_names_items_is_merged_into_the_named_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        store
            .writer()
            .write(vec![
                configured(&[("demo", "/w/new")]),
                new_item("demo#2", "demo", "/w/new"),
                Write::Repo(Repo {
                    name: None,
                    path: "/w/old".to_owned(),
                    remote: None,
                    default_branch: None,
                }),
            ])
            .await
            .unwrap();
        // As migration 16 leaves it: the older item on an unnamed row (no `Write` puts it
        // there, so a connection of its own does).
        let mut conn = SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("bosun.db")),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO item (item_key, repo_id, created_ms, updated_ms)
             SELECT 'demo#1', id, 1, 1 FROM repo WHERE path = '/w/old'",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        store
            .writer()
            .write(vec![configured(&[("demo", "/w/new")])])
            .await
            .unwrap();
        let (rows, items) = rows_and_items(&store).await;
        assert_eq!(rows, [(Some("demo".to_owned()), "/w/new".to_owned())]);
        assert_eq!(items, ["demo#1@demo", "demo#2@demo"]);
        let aliased: Option<String> = sqlx::query_scalar(
            "SELECT r.name FROM repo_alias a JOIN repo r ON r.id = a.repo_id
             WHERE a.path = '/w/old'",
        )
        .fetch_optional(store.reader().pool())
        .await
        .unwrap();
        assert_eq!(aliased.as_deref(), Some("demo"));
        store.close().await.unwrap();
    }

    /// #2, #76: a confined touch (out of a command's text) is stored only inside the message's
    /// repository; a `..` that leads out of it is outside. A file tool's touch outside it
    /// stays absolute.
    #[tokio::test]
    async fn a_confined_touch_is_stored_only_inside_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let touch = |path: &str, confined: bool| {
            Write::FileTouch(FileTouch {
                provider: PROVIDER.to_owned(),
                message_id: "m1".to_owned(),
                tool_use_id: "t1".to_owned(),
                path: path.to_owned(),
                kind: bosun_core::TouchKind::Unknown,
                confined,
            })
        };
        store
            .writer()
            .write(vec![
                Write::Repo(Repo {
                    name: None,
                    path: "/work/repo".to_owned(),
                    remote: None,
                    default_branch: None,
                }),
                source_file("f1"),
                write(record("m1", "f1", 1, 1, &["t1"])),
                touch("/work/repo/src/a.rs", true),
                touch("/work/repo/../secret/b.rs", true),
                touch("/elsewhere/c.rs", true),
                touch("/elsewhere/d.rs", false),
            ])
            .await
            .unwrap();
        let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM file_touch ORDER BY path")
            .fetch_all(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(paths, ["/elsewhere/d.rs", "src/a.rs"]);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn gaps_are_keyed_by_file_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let gap = |reason: &str| Write::Gap {
            provider: PROVIDER.to_owned(),
            file_key: "f1".to_owned(),
            gap: Gap {
                offset: 42,
                length: 7,
                record_type: Some("assistant".to_owned()),
                top_level_keys: vec!["type".to_owned(), "message".to_owned()],
                reason: reason.to_owned(),
            },
            parser_version: 1,
        };
        store
            .writer()
            .write(vec![source_file("f1"), gap("first"), gap("second")])
            .await
            .unwrap();
        let rows = sqlx::query!("SELECT reason, top_level_keys_json FROM ingest_gap")
            .fetch_all(store.reader().pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "second");
        assert_eq!(rows[0].top_level_keys_json, r#"["type","message"]"#);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_timer_checkpoints_the_wal_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bosun.db");
        let options = Options {
            checkpoint_interval: Duration::from_millis(200),
            ..Options::default()
        };
        let store = Store::open(&db, options).await.unwrap();
        store
            .writer()
            .write(vec![source_file("f1"), write(record("m", "f1", 1, 1, &[]))])
            .await
            .unwrap();
        // `immutable=1` reads the database file alone, ignoring the WAL.
        let in_file = || async {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db)
                .read_only(true)
                .immutable(true);
            let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
            let n = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM message"#)
                .fetch_one(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            n
        };
        assert_eq!(in_file().await, 0, "the commit is still in the WAL only");
        let deadline = Instant::now() + Duration::from_secs(10);
        while in_file().await == 0 {
            assert!(Instant::now() < deadline, "no checkpoint within 10 s");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        store.close().await.unwrap();
    }
}
