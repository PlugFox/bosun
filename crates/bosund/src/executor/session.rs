//! One agent session (#7, #62): the fail-closed checks, the prompt, the agent environment, the
//! run row before the spawn, the stream, the exit and the session's commit range.
//!
//! The session's files (the system prompt and the MCP configuration, which holds the session's
//! credential) are written to the daemon's work directory, never into the working tree, and the
//! MCP configuration is readable by the user only: on Windows its DACL admits the user alone
//! (`bosun_proc::pipe::OwnerOnly`), on Unix it is mode 0600 in a 0700 directory. Both are removed
//! when the session ends, however it ends.
//!
//! The session's standard output is also kept on disk while it runs ([`tail_path`], outside
//! the session's directory) and removed when the session task ends: a daemon that dies leaves
//! it, and crash recovery (#68) classifies the lost session from its tail.
//!
//! The handshake (#64): `bosun-mcp` must call `agent.hello` within [`Options::handshake`] after
//! the spawn. A session that has not by then, or that ends without it, is killed and reported as
//! [`Event::NoHandshake`]: the agent CLI would otherwise run on without its tools.
//!
//! # Under the session shim (#8)
//!
//! In worktree mode (E19), and only there, the agent runs under the session shim: a copy of
//! `bosun-shim` at a path named by the binary's digest ([`install_shim`], E3), in a directory
//! of its own under the session's (`shim-s<spawn>`), with the prompt in the shim's stdin file
//! (E17) and the item's remaining wall-clock budget as the shim's cap ([`shim_cap`], E9). The
//! shim is recorded in `process` (its pid and start time, its record, the digest of the
//! session's credential), and how far its output has been read, in whole lines, is written to
//! that row at most once a second and at the end (E18). While the item holds leases, the shim
//! is on its journal entry (E16). A session the shim capped is reported as
//! [`Event::BudgetExceeded`]. Dropping the session task leaves the session running: only the
//! daemon's kill ends it. Without `repo.<name>.worktree` the agent is a plain child of
//! [`Deps::spawner`] and dies with the daemon, as in stage 1.
//!
//! A session is run in two parts: [`session`] prepares and spawns it, and a follower reads it
//! to its end (both streams, the live entries, the tail, the exit's classification, the run's
//! end and its commit range, the offsets). A daemon restarted while a session ran under its
//! shim calls the follower through [`attach`] (#8, E3): what it needs is rebuilt from the store
//! and the disk, never a new run row or session id, and there is no handshake to wait for.
//!
//! [`Options::handshake`]: super::Options::handshake
//! [`Deps::spawner`]: super::Deps::spawner

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun_api::{EventBody, OutputStream};
use bosun_config::{Config, Repo};
use bosun_core::{Event, Exit, ItemId, Rules, SpawnMode};
use bosun_git::{Git, Hooks, Trailers};
use bosun_proc::ChildSpec;
use bosun_proc::shim::{Record, ShimSpawner, session_record};
use bosun_runner::claude::{ClaudeRunner, mcp_config};
use bosun_runner::{
    LiveEntry, McpServer, ProcessExit, Runner, SessionContext, SessionId, Tail, ToolPolicy, prompt,
};
use bosun_store::{NewProcess, ProcessRow, RunCommits, RunEnd, RunRow, RunStart, Write};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use super::park::ChildSlot;
use super::verdict::killed;
use super::{Claimed, Shared, blocking, git, identity, lookup, now_ms, workspace};
use crate::agent::Binding;
use crate::events;
use crate::leases::Holder;

/// The provider name of the Claude runner's sessions in the store.
const PROVIDER: &str = "claude";

/// How long the output is still read after the session's process ended.
const DRAIN: Duration = Duration::from_secs(2);

/// The shim's cap when no wall-clock budget is configured (E9: the shim is a deadman).
pub(crate) const SHIM_CAP_UNBUDGETED: Duration = Duration::from_hours(24);

/// How often, at most, the offsets of a shim's output are written to its `process` row (E18).
const OFFSETS_EVERY: Duration = Duration::from_secs(1);

/// How long the session task waits, at its end, for the shim to exit before its directory is
/// removed.
const SHIM_EXIT: Duration = Duration::from_secs(5);

/// The session shim's wall-clock cap (E9): what is left of the item's wall-clock budget
/// (`budget`: the card's `budget.wall`, else the configuration's `budget.item_wall_clock`)
/// after the active time it has `used`; zero once it is spent. With no budget,
/// [`SHIM_CAP_UNBUDGETED`].
pub(crate) fn shim_cap(budget: Option<Duration>, used: Duration) -> Duration {
    if cfg!(mutant_loop_shim_cap_unbounded) {
        // Mutant (`mutant_loop_shim_cap_unbounded`): the shim is capped at a day, whatever
        // the budget.
        return SHIM_CAP_UNBUDGETED;
    }
    budget.map_or(SHIM_CAP_UNBUDGETED, |budget| budget.saturating_sub(used))
}

/// The file name of the shim binary.
fn shim_name() -> String {
    format!("bosun-shim{}", std::env::consts::EXE_SUFFIX)
}

/// The copy of the shim at `found` that sessions run (E3): `<daemon_dir>/shim/<the first 16
/// hex digits of its SHA-256>/bosun-shim[.exe]`, made unless it exists already (copied to a
/// temporary name in the same directory, then renamed). The daemon's own `found` stays
/// replaceable while a session runs, and a session started by an older daemon keeps its copy.
///
/// # Errors
///
/// When `found` cannot be read or the copy cannot be made.
pub(crate) fn install_shim(found: &Path, daemon_dir: &Path) -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if cfg!(mutant_loop_shim_in_place) {
        // Mutant (`mutant_loop_shim_in_place`): the shim runs from where it was found.
        return Ok(found.to_owned());
    }
    let bytes = std::fs::read(found).map_err(|e| format!("{}: {e}", found.display()))?;
    let digest = Sha256::digest(&bytes);
    let hex = digest[..8].iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    let dir = daemon_dir.join("shim").join(hex);
    let name = shim_name();
    let path = dir.join(&name);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let temp = dir.join(format!(
        "{name}.{}-{}.copying",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // `copy` keeps the permissions, so the copy is executable where the original is.
    std::fs::copy(found, &temp).map_err(|e| format!("{}: {e}", temp.display()))?;
    match std::fs::rename(&temp, &path) {
        Ok(()) => Ok(path),
        // Another loop made the same copy meanwhile (it may be running already).
        Err(_) if path.is_file() => {
            std::fs::remove_file(&temp).ok();
            Ok(path)
        }
        Err(e) => {
            std::fs::remove_file(&temp).ok();
            Err(format!("{}: {e}", path.display()))
        }
    }
}

/// The bytes of `line` that count toward a stream's offset (E18): a whole line, its end
/// included; nothing of a line cut short.
fn whole_line(line: &[u8]) -> u64 {
    if line.last() == Some(&b'\n') {
        u64::try_from(line.len()).unwrap_or(u64::MAX)
    } else {
        0
    }
}

/// The session's shim, once it runs: its row and how far its output has been read.
struct ShimRun {
    record: PathBuf,
    shim: bosun_proc::ProcessKey,
    /// The offsets last written to the row.
    written: (u64, u64),
    written_at: Instant,
}

impl ShimRun {
    /// Writes the offsets `read` to the row, unless they are what it holds; `force`: whatever
    /// the time since the last write, else only [`OFFSETS_EVERY`] after it.
    async fn offsets(&mut self, shared: &Shared, read: (u64, u64), force: bool) {
        if cfg!(mutant_loop_drop_shim_offsets) {
            // Mutant (`mutant_loop_drop_shim_offsets`): the offsets are never written.
            return;
        }
        if read == self.written || (!force && self.written_at.elapsed() < OFFSETS_EVERY) {
            return;
        }
        let write = Write::ProcessOffsets {
            record_path: self.record.to_string_lossy().into_owned(),
            stdout: read.0,
            stderr: read.1,
        };
        if let Err(e) = shared.deps.writer.write(vec![write]).await {
            shared.trouble(format!("the offsets of {}: {e}", self.record.display()));
        }
        self.written = read;
        self.written_at = Instant::now();
    }
}

/// Which session this is.
#[derive(Debug, Clone)]
pub(crate) struct Run {
    pub(crate) item: ItemId,
    pub(crate) attempt: u32,
    pub(crate) spawn: u32,
    pub(crate) mode: SpawnMode,
    /// Where the session's child is recorded while it runs, for the wake check (#66).
    pub(crate) child: ChildSlot,
    /// The session shim's wall-clock cap, in worktree mode ([`shim_cap`]).
    pub(crate) shim_cap: Duration,
}

impl Run {
    fn exited(&self, exit: Exit) -> Event {
        Event::Exited {
            attempt: self.attempt,
            spawn: self.spawn,
            exit,
            text: String::new(),
        }
    }
}

/// A new session id: 16 bytes from a hash of the time, the process, a counter and the run.
fn new_session_id(run: &Run) -> SessionId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(run.item.0.as_bytes());
    hasher.update(run.attempt.to_le_bytes());
    hasher.update(run.spawn.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    SessionId::from_random_bytes(bytes)
}

pub(crate) fn exit_of(exit: &bosun_runner::Exit) -> Exit {
    match *exit {
        bosun_runner::Exit::Ok => Exit::Ok,
        // Mutant: the loop reports a limit as a crash, which counts once retried (#7).
        bosun_runner::Exit::Limit { .. } if cfg!(mutant_loop_limit_counts) => Exit::Crash,
        bosun_runner::Exit::Limit { reset } => Exit::Limit {
            reset_ms: reset.and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
            }),
        },
        // Mutant: an expired authentication is retried like a crash.
        bosun_runner::Exit::Auth if cfg!(mutant_loop_auth_retries) => Exit::Crash,
        bosun_runner::Exit::Auth => Exit::Auth,
        // A runner never reads a question from output; it is an MCP call (#63).
        bosun_runner::Exit::Question | bosun_runner::Exit::Crash { .. } => Exit::Crash,
    }
}

fn entry_text(entry: &LiveEntry) -> String {
    match entry {
        LiveEntry::Started { model } => {
            format!("started ({})", model.as_deref().unwrap_or("default model"))
        }
        LiveEntry::Text(text) => text.clone(),
        LiveEntry::Tool { name } => format!("tool: {name}"),
        LiveEntry::Denied { tool } => format!("denied: {tool}"),
        LiveEntry::Error(error) => format!("error: {error}"),
        LiveEntry::Ended { is_error } => {
            if *is_error {
                "ended with an error".to_owned()
            } else {
                "ended".to_owned()
            }
        }
    }
}

/// The prompt of the attempt: the snapshot's prose and verdict, its inlined comments, and the
/// previous attempt's verdict output and refusal from the store (#2: never from the board).
async fn prompt_of(shared: &Shared, claimed: &Claimed, run: &Run) -> prompt::Prompt {
    let reader = &shared.deps.reader;
    let previous = reader
        .verdict_runs(&run.item)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.attempt < run.attempt)
        .filter_map(|v| v.tail)
        .next_back();
    // Why the daemon refused the previous attempt before its verdict, if it did (#137).
    let refusal = reader
        .attempts(&run.item)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|a| a.attempt.number + 1 == run.attempt)
        .map_or_default(|a| a.refusal);
    let daemon: BTreeSet<String> = reader
        .board_comments(&run.item)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    prompt::build(&prompt::PromptInput {
        prose: claimed.snapshot.body.clone(),
        verdict: claimed.snapshot.contract.verdict.clone(),
        attempt: run.attempt,
        max_attempts: Rules::default().max_attempts,
        previous_verdict: previous,
        verdict_tail_bytes: prompt::VERDICT_TAIL_BYTES,
        previous_refusal: refusal,
        standing_instructions: String::new(),
        comments: claimed
            .snapshot
            .comments
            .iter()
            .map(|c| prompt::Comment {
                author: c.author.clone(),
                node_id: String::new(),
                hidden_key: c.key.clone(),
                body: c.body.clone(),
            })
            .collect(),
        daemon_comment_ids: daemon,
    })
}

/// The variable of the `bosun` MCP server that carries the session's credential (#63).
pub(crate) const CREDENTIAL_VAR: &str = "BOSUN_AGENT_CREDENTIAL";

/// The per-session MCP configuration: `bosun` and the repository's servers (#7). The `bosun`
/// server gets the session's credential in its environment, and the endpoint to call
/// `agent.hello` on (#64).
fn mcp_file(
    shared: &Shared,
    repo: &Repo,
    item: &ItemId,
    id: &SessionId,
    credential: &str,
) -> Result<String, String> {
    let bosun = serde_json::json!({
        "command": shared.options.mcp,
        "args": [
            "--item", item.0,
            "--session", id.as_str(),
            "--endpoint", shared.options.endpoint.to_string(),
        ],
        "env": { CREDENTIAL_VAR: credential },
    })
    .to_string();
    let servers: Vec<McpServer> = repo
        .mcp_servers
        .iter()
        .map(|(name, server)| McpServer {
            name: name.clone(),
            config: serde_json::json!({
                "command": server.command,
                "args": server.args,
                "env": server.env,
            })
            .to_string(),
        })
        .collect();
    mcp_config(&bosun, &servers).map_err(|e| e.to_string())
}

/// The session's directory under the daemon's work directory, removed with what it holds when
/// dropped: at the end of the session and on every early return.
struct SessionDir(PathBuf);

impl SessionDir {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            crate::log::log!("bosund: removing {}: {e}", self.0.display());
        }
    }
}

/// Where the standard output of session `session` is kept while it runs: what crash recovery
/// classifies a session lost with the daemon from (#68). Outside the session's directory, which
/// holds its credential.
pub(crate) fn tail_path(work_dir: &Path, session: &str) -> PathBuf {
    work_dir.join("tails").join(format!("{session}.jsonl"))
}

/// The session's output as it arrives, on disk, removed when the session task ends; a daemon
/// that dies leaves it for the next start to read.
struct TailFile {
    path: PathBuf,
    file: std::sync::Mutex<Option<std::fs::File>>,
}

impl TailFile {
    /// Opens the file; one that cannot be opened is reported and nothing is kept.
    fn open(path: PathBuf) -> Self {
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::File::create(&path));
        let file = match file {
            Ok(file) => Some(file),
            Err(e) => {
                crate::log::log!("bosund: {}: {e}", path.display());
                None
            }
        };
        Self {
            path,
            file: std::sync::Mutex::new(file),
        }
    }

    /// Opens the file a previous daemon left for a session this one re-attached (#8): what it
    /// holds is returned, and the session's output from here on is appended to it.
    fn reopen(path: PathBuf) -> (Self, String) {
        let kept = std::fs::read_to_string(&path).unwrap_or_default();
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
            });
        let file = match file {
            Ok(file) => Some(file),
            Err(e) => {
                crate::log::log!("bosund: {}: {e}", path.display());
                None
            }
        };
        let tail = Self {
            path,
            file: std::sync::Mutex::new(file),
        };
        (tail, kept)
    }

    fn push(&self, line: &str) {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(f) = file.as_mut()
            && writeln!(f, "{line}").is_err()
        {
            *file = None;
        }
    }
}

impl Drop for TailFile {
    fn drop(&mut self) {
        drop(
            self.file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            crate::log::log!("bosund: removing {}: {e}", self.path.display());
        }
    }
}

/// Writes a new file that only the current user can read: an owner-only DACL on Windows, mode
/// 0600 on Unix. A file left at `path` is replaced.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    #[cfg(windows)]
    let mut file = bosun_proc::pipe::OwnerOnly::new()?.create_file(path)?;
    #[cfg(not(windows))]
    let mut file = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?
    };
    file.write_all(text.as_bytes())?;
    file.flush()
}

/// The model a session of `repo` asks for (`--model`, #5, #133): the card's `model`, else the
/// repository's default. The configuration refuses a repository with a queue and no model, so a
/// spawned session always names one rather than running on the agent CLI's default of the day.
pub(crate) fn session_model(contract: &bosun_queue::Contract, repo: &Repo) -> Option<String> {
    if cfg!(mutant_runner_no_default_model) {
        // Mutant: the repository's default is not passed.
        return contract.model.clone();
    }
    contract.model.clone().or_else(|| repo.model.clone())
}

/// Where the session's MCP configuration goes: its directory, outside the working tree.
fn mcp_path(dir: &Path, repo: &Repo) -> PathBuf {
    if cfg!(mutant_mcp_config_in_tree) {
        // Mutant: the configuration, credential and all, lands in the working tree.
        return repo.path.join(".bosun-mcp.json");
    }
    dir.join("mcp.json")
}

/// The id a resumed session continues: the attempt's last run's.
async fn resumed_id(shared: &Shared, run: &Run) -> Option<SessionId> {
    let runs = shared.deps.reader.runs(&run.item).await.ok()?;
    let last = runs.iter().rev().find(|r| r.attempt == run.attempt)?;
    SessionId::parse(&last.session_key).ok()
}

/// Runs one session to its end and records it. Returns the machine's event: its exit as the
/// runner classified it, `identity_refused` when the identity checks refused (#2, #99),
/// `crash` when it could not be started, and `resume_failed` when a resume crashed before the
/// session came up (the CLI found no conversation to resume, #66): the machine then resets the
/// tree to the last commit and starts a fresh session, uncounted.
///
/// The session's git carries the trailer hooks of #59: every commit it makes gets `Item:`,
/// `Run:` and `Session:` with the attempt's values.
#[allow(clippy::too_many_lines)]
pub(crate) async fn session(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<bool>,
    on_blocking: Box<dyn Fn() + Send + Sync>,
) -> Event {
    let crash = run.exited(Exit::Crash);
    let Some((repo, identity)) = lookup(config, &shared.options.repo) else {
        return crash;
    };
    let Some(program) = shared.options.agent.clone() else {
        shared.trouble("the agent CLI `claude` was not found on PATH");
        return crash;
    };
    // Its leases (#8), before its first session: the loop stays responsive meanwhile, and a
    // kill withdraws the request. A later session holds them already and goes on at once.
    // Mutant (`mutant_loop_skip_leases`): the session starts without waiting for its lease.
    if !cfg!(mutant_loop_skip_leases) {
        match super::hold_leases(shared, repo, claimed, &run.item, &mut kill).await {
            Ok(()) => {}
            Err(None) => return crash,
            Err(Some(e)) => {
                shared.trouble(format!("{}: its leases: {e}", run.item));
                return crash;
            }
        }
    }
    // Fail closed before spawn (#2, rule 2): a refusal here is Needs you as `identity`, with
    // the refusal logged (#99).
    let bundle = {
        let (repo, name, identity) = (repo.path.clone(), repo.identity.clone(), identity.clone());
        let shared = shared.clone();
        blocking(move || {
            let bundle = identity::bundle(&name, &identity, &repo)?;
            identity::check(&bundle, &repo, shared.deps.logins.as_ref())?;
            Ok::<_, String>(bundle)
        })
        .await
    };
    let bundle = match bundle {
        Ok(bundle) => bundle,
        Err(e) => {
            shared.trouble(format!("{}: {e}", run.item));
            return Event::IdentityRefused {
                attempt: run.attempt,
                spawn: run.spawn,
            };
        }
    };

    let session_id = match run.mode {
        SpawnMode::Resume => match resumed_id(shared, run).await {
            Some(id) => id,
            None => new_session_id(run),
        },
        SpawnMode::Fresh | SpawnMode::Reset => new_session_id(run),
    };
    // Where the item's files are: its worktree in worktree mode (#8).
    let git_repo = Git::new(&claimed.workdir);
    if run.mode == SpawnMode::Reset {
        let git_repo = git_repo.clone();
        let reset = blocking(move || {
            let head = git::head(&git_repo)?;
            git::reset_to(&git_repo, &head)
        })
        .await;
        if let Err(e) = reset {
            shared.trouble(format!(
                "{}: the reset before a fresh session: {e}",
                run.item
            ));
            return crash;
        }
    }

    // The session's credential (#63), revoked when `credential` is dropped: when the process
    // is gone, or on any early return.
    let credential =
        shared
            .deps
            .credentials
            .issue(binding(shared, repo, run, &session_id, on_blocking));
    let credential = match credential {
        Ok(credential) => credential,
        Err(e) => {
            shared.trouble(format!("{}: the session's credential: {e}", run.item));
            return crash;
        }
    };

    let prompt = prompt_of(shared, claimed, run).await;
    let dir = shared
        .options
        .work_dir
        .join("sessions")
        .join(session_id.as_str());
    let files = (|| -> Result<(SessionDir, PathBuf, PathBuf, Hooks), String> {
        let dir = SessionDir::create(dir).map_err(|e| e.to_string())?;
        let system = dir.0.join("system.md");
        std::fs::write(&system, &prompt.system).map_err(|e| e.to_string())?;
        let mcp = mcp_path(&dir.0, repo);
        write_private(
            &mcp,
            &mcp_file(shared, repo, &run.item, &session_id, credential.token())?,
        )
        .map_err(|e| format!("{}: {e}", mcp.display()))?;
        let hooks = Hooks::install(dir.0.join("git-hooks"))
            .map_err(|e| format!("the trailer hooks: {e}"))?;
        Ok((dir, system, mcp, hooks))
    })();
    let (session_dir, system_file, mcp_config_file, hooks) = match files {
        Ok(files) => files,
        Err(e) => {
            shared.trouble(format!("{}: the session files: {e}", run.item));
            return crash;
        }
    };
    let runner = ClaudeRunner::new(&program);
    // Mutant (`mutant_loop_session_in_user_checkout`): in worktree mode the session runs in the
    // user's checkout.
    let cwd = if cfg!(mutant_loop_session_in_user_checkout) {
        repo.path.clone()
    } else {
        claimed.workdir.clone()
    };
    let ctx = SessionContext {
        cwd,
        session_id: session_id.clone(),
        prompt: prompt.user,
        system_prompt_file: system_file,
        mcp_config_file,
        repo_mcp_servers: repo.mcp_servers.keys().cloned().collect(),
        policy: ToolPolicy {
            allow_push: repo.policy.push,
            // The operator token's file (#67).
            deny: shared.deps.control.deny_rules(),
            ..ToolPolicy::default()
        },
        model: session_model(&claimed.snapshot.contract, repo),
    };
    let command = match run.mode {
        SpawnMode::Resume => runner.resume_command(&session_id, &ctx),
        SpawnMode::Fresh | SpawnMode::Reset => runner.command(&ctx),
    };
    let mut env = match identity::agent_env(&bundle, &shared.options.work_dir) {
        Ok(env) => env,
        Err(e) => {
            shared.trouble(format!("{}: the agent environment: {e}", run.item));
            return crash;
        }
    };
    // The trailer hooks of #59 (D6 of #7), in the session's directory: only this session's git
    // reaches them, through command-scope configuration after the identity's own entries.
    env.add_git_config(hooks.git_config());
    let trailers = Trailers {
        item: run.item.0.clone(),
        run: git::run_label(run.attempt, run.spawn),
        session: session_id.to_string(),
    };
    // The checkout as git spells its top level, which the hook compares with its own: a
    // commit the agent makes in another repository gets no trailer (#134).
    let checkout = {
        let git_repo = git_repo.clone();
        match blocking(move || Hooks::checkout(&git_repo)).await {
            Ok(checkout) => checkout,
            Err(e) => {
                shared.trouble(format!("{}: the checkout's top level: {e}", run.item));
                return crash;
            }
        }
    };
    for (name, value) in Hooks::env(&trailers, &checkout) {
        env.add_session_var(&name, value);
    }
    let mut vars: Vec<(OsString, OsString)> =
        env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect();
    vars.extend(command.env.iter().cloned());
    // The shared build directory (E1), and the worktree's files made newer than its last build.
    if let Err(e) = workspace::shared_build(repo, &command.cwd, &mut vars, true).await {
        shared.trouble(format!("{}: {e}", run.item));
        return crash;
    }

    let start_head = {
        let git_repo = git_repo.clone();
        blocking(move || git::head(&git_repo)).await.ok()
    };
    // The run row before the spawn: a transcript of this id is a run session (#57).
    let start = Write::Run(RunStart {
        item: run.item.clone(),
        attempt: run.attempt,
        spawn: run.spawn,
        mode: run.mode,
        provider: PROVIDER.to_owned(),
        session_key: session_id.to_string(),
        started_ms: now_ms(),
    });
    if let Err(e) = shared.deps.writer.write(vec![start]).await {
        shared.trouble(format!("{}: the run cannot be recorded: {e}", run.item));
        return crash;
    }

    let spec = ChildSpec {
        program: command.program.clone(),
        args: command.args.clone(),
        env: vars,
        cwd: command.cwd.clone(),
        stdin: command.stdin.clone().map(String::into_bytes),
    };
    let tail_file = TailFile::open(tail_path(&shared.options.work_dir, session_id.as_str()));
    // Worktree mode (E19): the agent runs under the session shim.
    // Mutant (`mutant_loop_sessions_without_shim`): it is a plain child of the daemon.
    let shim_dir = (claimed.worktree.is_some() && !cfg!(mutant_loop_sessions_without_shim))
        .then(|| session_dir.0.join(format!("shim-s{}", run.spawn)));
    let spawned = match &shim_dir {
        Some(dir) => spawn_under_shim(shared, run, &spec, dir).await,
        None => shared.deps.spawner.spawn(&spec).map_err(|e| e.to_string()),
    };
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(e) => {
            shared.trouble(format!(
                "{}: {} cannot be started: {e}",
                run.item,
                command.program.display()
            ));
            end(shared, run, Exit::Crash, start_head.as_deref(), &git_repo).await;
            return crash;
        }
    };
    let key = spawned.tree.key();
    set_child(&run.child, Some(key));
    let shim_run = match &shim_dir {
        Some(dir) => record_shim(shared, run, &session_id, &credential, dir).await,
        None => None,
    };
    record_child(shared, run, key).await;
    shared.publish(EventBody::SessionStarted {
        item: run.item.to_string(),
        attempt: run.attempt,
        spawn: run.spawn,
        mode: mode_name(run.mode).to_owned(),
        session: Some(session_id.to_string()),
    });
    let following = Following {
        runner,
        session_id,
        credential: Some(credential),
        session_dir,
        under_shim: shim_dir.is_some(),
        shim_run,
        tail_file,
        tail: Tail::default(),
        up: false,
        start_head,
        git_repo,
        await_hello: true,
        consumed: (0, 0),
    };
    follow(shared, run, following, spawned, kill).await
}

/// Records `key` as the session's child on its attempt, for crash recovery's reaping (#68).
async fn record_child(shared: &Shared, run: &Run, key: bosun_proc::ProcessKey) {
    let child = Write::SessionChild {
        item: run.item.clone(),
        attempt: run.attempt,
        pid: key.pid,
        pid_started_ms: key.start.raw().cast_signed(),
    };
    if let Err(e) = shared.deps.writer.write(vec![child]).await {
        shared.trouble(format!("{}: the session's child: {e}", run.item));
    }
}

/// What following a session to its end needs: of one just spawned, or of one a previous daemon
/// left running under its shim ([`attach`]).
struct Following {
    runner: ClaudeRunner,
    session_id: SessionId,
    /// The session's credential, revoked when the session's process is gone; `None` when it
    /// could not be re-admitted after a restart.
    credential: Option<crate::agent::Issued>,
    /// Removed once the session (and under the shim, its shim) is gone.
    session_dir: SessionDir,
    under_shim: bool,
    shim_run: Option<ShimRun>,
    tail_file: TailFile,
    /// The session's output read so far, for the exit's classification.
    tail: Tail,
    /// The session came up (the CLI's `init` was read).
    up: bool,
    /// HEAD when the session started: the start of its commit range.
    start_head: Option<String>,
    git_repo: Git,
    /// The handshake of #64 is still to come.
    await_hello: bool,
    /// The bytes of each stream consumed so far, in whole lines (E18).
    consumed: (u64, u64),
}

/// Follows a running session to its end: reads both streams (the live entries, the tail, the
/// offsets), classifies its exit and records the run's end and its commit range.
#[allow(clippy::too_many_lines)]
async fn follow(
    shared: &Arc<Shared>,
    run: &Run,
    following: Following,
    spawned: bosun_proc::Spawned,
    mut kill: watch::Receiver<bool>,
) -> Event {
    let Following {
        runner,
        session_id,
        credential,
        session_dir,
        under_shim,
        mut shim_run,
        tail_file,
        mut tail,
        up,
        start_head,
        git_repo,
        await_hello,
        mut consumed,
    } = following;
    let said_hello = |credential: &Option<crate::agent::Issued>| {
        credential
            .as_ref()
            .is_some_and(crate::agent::Issued::said_hello)
    };
    let bosun_proc::Spawned {
        mut tree,
        stdout,
        stderr,
    } = spawned;

    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let (mut out_line, mut err_line) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    let mismatch = std::sync::atomic::AtomicBool::new(false);
    // The session came up (the CLI's `init`): a crash after it is not a failed resume.
    let up = std::sync::atomic::AtomicBool::new(up);
    let on_line = |stream: OutputStream, bytes: &[u8], tail: &mut Tail| {
        let line = String::from_utf8_lossy(bytes).trim_end().to_owned();
        let texts: Vec<(String, Option<String>)> = match stream {
            OutputStream::Stdout => {
                let entries = runner.live_entries(&line);
                if entries
                    .iter()
                    .any(|e| matches!(e, LiveEntry::Started { .. }))
                {
                    up.store(true, Ordering::Relaxed);
                }
                let texts = entries
                    .iter()
                    .map(|e| {
                        let tool = match e {
                            LiveEntry::Tool { name } => Some(name.clone()),
                            _ => None,
                        };
                        (entry_text(e), tool)
                    })
                    .collect();
                if let Some(reported) = runner.session_id(&line)
                    && reported != session_id
                    && !mismatch.swap(true, Ordering::Relaxed)
                {
                    crate::log::log!(
                        "bosund: {}: the session reports id {reported}, not {session_id}",
                        run.item
                    );
                }
                tail_file.push(&line);
                tail.push(line);
                texts
            }
            OutputStream::Stderr => vec![(line, None)],
        };
        for (text, tool) in texts {
            shared.publish(EventBody::SessionOutput {
                item: run.item.to_string(),
                attempt: run.attempt,
                spawn: run.spawn,
                stream,
                line: text,
                tool,
            });
        }
    };
    // The handshake (#64): `bosun-mcp` says hello within the timeout, or the session is killed.
    let handshake = tokio::time::sleep(shared.options.handshake);
    tokio::pin!(handshake);
    let mut awaiting_hello = await_hello && !cfg!(mutant_mcp_skip_handshake);
    let mut missed = false;
    // Offsets read just before the output goes quiet are written a second later all the same:
    // a daemon that dies while the agent is silent leaves them in the row (E18).
    let mut flush = tokio::time::interval(OFFSETS_EVERY);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let status = {
        let mut wait = tree.wait();
        loop {
            tokio::select! {
                _ = flush.tick(), if shim_run.is_some() => {}
                () = &mut handshake, if awaiting_hello => {
                    awaiting_hello = false;
                    if !said_hello(&credential) {
                        missed = true;
                        break None;
                    }
                }
                read = out.read_until(b'\n', &mut out_line), if out_open => {
                    if matches!(read, Ok(0) | Err(_)) {
                        out_open = false;
                    }
                    if !out_line.is_empty() {
                        consumed.0 += whole_line(&out_line);
                        on_line(OutputStream::Stdout, &out_line, &mut tail);
                        out_line.clear();
                    }
                }
                read = err.read_until(b'\n', &mut err_line), if err_open => {
                    if matches!(read, Ok(0) | Err(_)) {
                        err_open = false;
                    }
                    if !err_line.is_empty() {
                        consumed.1 += whole_line(&err_line);
                        on_line(OutputStream::Stderr, &err_line, &mut tail);
                        err_line.clear();
                    }
                }
                status = &mut wait => break Some(status),
                () = killed(&mut kill) => break None,
            }
            if let Some(shim_run) = shim_run.as_mut() {
                shim_run.offsets(shared, consumed, false).await;
            }
        }
    };
    // The child has exited or is being killed: no wake check may take it for dead now.
    set_child(&run.child, None);
    // Whatever the session left running in its container goes with it, and so does its
    // credential.
    // A session that ended before the timeout must have said hello too.
    let missed = missed || (awaiting_hello && status.is_some() && !said_hello(&credential));
    // Whatever the session left running in its container goes with it, and so do its
    // credential and its files.
    let _ = tree.kill_tree();
    drop(credential);
    // Under the shim its directory, which holds the shim's files, goes once the shim is gone.
    let session_dir = if under_shim {
        Some(session_dir)
    } else {
        drop(session_dir);
        None
    };
    let deadline = Instant::now() + DRAIN;
    while out_open || err_open {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        tokio::select! {
            read = out.read_until(b'\n', &mut out_line), if out_open => {
                if matches!(read, Ok(0) | Err(_)) {
                    out_open = false;
                }
                if !out_line.is_empty() {
                    consumed.0 += whole_line(&out_line);
                    on_line(OutputStream::Stdout, &out_line, &mut tail);
                    out_line.clear();
                }
            }
            read = err.read_until(b'\n', &mut err_line), if err_open => {
                if matches!(read, Ok(0) | Err(_)) {
                    err_open = false;
                }
                if !err_line.is_empty() {
                    consumed.1 += whole_line(&err_line);
                    on_line(OutputStream::Stderr, &err_line, &mut tail);
                    err_line.clear();
                }
            }
            () = tokio::time::sleep(left) => break,
        }
    }
    // The output files are closed before the shim's directory goes.
    drop((out, err));
    let capped = match shim_run.as_mut() {
        Some(shim_run) => {
            shim_run.offsets(shared, consumed, true).await;
            shim_ended(shared, run, shim_run).await
        }
        None => false,
    };
    drop(session_dir);
    let ended = status.is_some();
    if missed {
        shared.trouble(format!(
            "{}: bosun-mcp ({}) did not say hello within {} s of the spawn; the session was \
             killed",
            run.item,
            shared.options.mcp.display(),
            shared.options.handshake.as_secs()
        ));
        end(shared, run, Exit::Crash, start_head.as_deref(), &git_repo).await;
        shared.publish(EventBody::SessionEnded {
            item: run.item.to_string(),
            attempt: run.attempt,
            spawn: run.spawn,
            exit: events::exit_name(Some(Exit::Crash)).to_owned(),
        });
        return Event::NoHandshake {
            attempt: run.attempt,
            spawn: run.spawn,
        };
    }
    let (exit, published) = match status {
        Some(status) => {
            let code = status.ok().and_then(|s| s.code());
            let exit = exit_of(&runner.classify_exit(ProcessExit { code }, &tail));
            (exit, Some(exit))
        }
        // Killed by the daemon: recorded as a crash; the machine has moved on already.
        None => (Exit::Crash, None),
    };
    end(shared, run, exit, start_head.as_deref(), &git_repo).await;
    shared.publish(EventBody::SessionEnded {
        item: run.item.to_string(),
        attempt: run.attempt,
        spawn: run.spawn,
        exit: events::exit_name(published).to_owned(),
    });
    if capped && ended {
        shared.trouble(format!(
            "{}: the session shim killed the session at the item's wall-clock cap ({} s)",
            run.item,
            run.shim_cap.as_secs()
        ));
        return Event::BudgetExceeded;
    }
    if run.mode == SpawnMode::Resume && exit == Exit::Crash && ended && !up.load(Ordering::Relaxed)
    {
        crate::log::log!(
            "bosund: {}: the resume of session {session_id} failed; a fresh session follows",
            run.item
        );
        return Event::ResumeFailed {
            attempt: run.attempt,
            spawn: run.spawn,
        };
    }
    run.exited(exit)
}

/// A session a previous daemon left under its shim (#8, E3), as crash recovery found it: its
/// `process` row, its run row, the shim's record and the session re-attached from the row's
/// offsets.
#[derive(Debug)]
pub(crate) struct Attached {
    pub(crate) process: ProcessRow,
    pub(crate) run: RunRow,
    pub(crate) record: Record,
    pub(crate) spawned: bosun_proc::Spawned,
}

/// Follows `attached`, the session of `run` that a previous daemon started under the shim, to
/// its end, as [`session`] follows one it spawned. Everything comes from the store and the
/// disk: the session id from the run row (no new run row, no new id); the credential re-admitted
/// from the row's digest with the session's binding, as having said hello (its `bosun-mcp`
/// said it to the previous daemon), and revoked at the end; the output read so far from the
/// tail file, appended to from here on; the start of the commit range from the attempt's
/// previous session's end, else the item's base.
#[allow(clippy::too_many_lines)]
pub(crate) async fn attach(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    attached: Attached,
    mut kill: watch::Receiver<bool>,
    on_blocking: Box<dyn Fn() + Send + Sync>,
) -> Event {
    let crash = run.exited(Exit::Crash);
    let Some((repo, _)) = lookup(config, &shared.options.repo) else {
        return crash;
    };
    let Attached {
        process,
        run: run_row,
        record,
        spawned,
    } = attached;
    let Ok(session_id) = SessionId::parse(&run_row.session_key) else {
        shared.trouble(format!(
            "{}: the session id {:?} does not parse",
            run.item, run_row.session_key
        ));
        return crash;
    };
    // The leases it holds already: granted again at once (E16).
    if !cfg!(mutant_loop_skip_leases) {
        match super::hold_leases(shared, repo, claimed, &run.item, &mut kill).await {
            Ok(()) => {}
            Err(None) => return crash,
            Err(Some(e)) => {
                shared.trouble(format!("{}: its leases: {e}", run.item));
                return crash;
            }
        }
    }
    let digest = process
        .credential_digest
        .as_deref()
        .and_then(|d| <[u8; 32]>::try_from(d).ok());
    let credential = if cfg!(mutant_loop_reattach_without_credential) {
        // Mutant (`mutant_loop_reattach_without_credential`): the stored digest is not
        // re-admitted; the session's agent calls are refused.
        None
    } else {
        digest.map(|digest| {
            shared
                .deps
                .credentials
                .readmit(digest, binding(shared, repo, run, &session_id, on_blocking))
        })
    };
    if credential.is_none() {
        shared.trouble(format!(
            "{}: the credential of session {session_id} cannot be re-admitted",
            run.item
        ));
    }
    let program = shared
        .options
        .agent
        .clone()
        .unwrap_or_else(|| PathBuf::from("claude"));
    let runner = ClaudeRunner::new(&program);
    let session_dir = SessionDir(
        shared
            .options
            .work_dir
            .join("sessions")
            .join(session_id.as_str()),
    );
    let (tail_file, kept) =
        TailFile::reopen(tail_path(&shared.options.work_dir, session_id.as_str()));
    let up = kept.lines().any(|line| {
        runner
            .live_entries(line)
            .iter()
            .any(|e| matches!(e, LiveEntry::Started { .. }))
    });
    let git_repo = Git::new(&claimed.workdir);
    let runs = shared.deps.reader.runs(&run.item).await.unwrap_or_default();
    let start_head = runs
        .iter()
        .rev()
        .filter(|r| r.attempt == run.attempt && r.spawn < run.spawn)
        .find_map(|r| r.head_sha.clone())
        .or_else(|| {
            Some(
                claimed
                    .worktree
                    .as_ref()
                    .map_or_else(|| claimed.base.head.clone(), |w| w.worktree.base.clone()),
            )
        });
    let key = spawned.tree.key();
    set_child(&run.child, Some(key));
    record_child(shared, run, key).await;
    shared
        .deps
        .leases
        .set_shim(&Holder::Item(run.item.clone()), Some(record.shim));
    let shim_run = ShimRun {
        record: PathBuf::from(&process.record_path),
        shim: record.shim,
        written: (process.stdout_offset, process.stderr_offset),
        written_at: Instant::now(),
    };
    crate::log::log!(
        "bosund: {}: re-attached session {session_id} under shim pid {} at offsets {}/{}",
        run.item,
        record.shim.pid,
        process.stdout_offset,
        process.stderr_offset
    );
    let following = Following {
        runner,
        session_id,
        credential,
        session_dir,
        under_shim: true,
        shim_run: Some(shim_run),
        tail_file,
        tail: Tail::from_text(&kept),
        up,
        start_head,
        git_repo,
        await_hello: false,
        consumed: (process.stdout_offset, process.stderr_offset),
    };
    follow(shared, run, following, spawned, kill).await
}

/// The binding of the credential of session `id` of `run` (#63).
fn binding(
    shared: &Shared,
    repo: &Repo,
    run: &Run,
    id: &SessionId,
    on_blocking: Box<dyn Fn() + Send + Sync>,
) -> Binding {
    Binding {
        identity: repo.identity.clone(),
        item: run.item.clone(),
        attempt: run.attempt,
        session: id.to_string(),
        repo: repo.path.clone(),
        writer: shared.deps.writer.clone(),
        on_blocking,
    }
}

/// Starts `spec` under a copy of the session shim, in `dir` (which must not exist yet: a
/// resumed session has a new spawn number), capped at [`Run::shim_cap`].
async fn spawn_under_shim(
    shared: &Arc<Shared>,
    run: &Run,
    spec: &ChildSpec,
    dir: &Path,
) -> Result<bosun_proc::Spawned, String> {
    let (found, daemon_dir) = (
        shared.options.shim.clone(),
        shared.options.daemon_dir.clone(),
    );
    let copy = blocking(move || install_shim(&found, &daemon_dir))
        .await
        .map_err(|e| format!("the session shim cannot be installed: {e}"))?;
    let shim = ShimSpawner::new(copy, dir, run.shim_cap);
    let (sessions, spec, dir) = (
        shared.deps.sessions.clone(),
        spec.clone(),
        dir.to_path_buf(),
    );
    blocking(move || {
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        sessions
            .spawn_in(&shim, &spec, &dir)
            .map_err(|e| format!("through the session shim: {e}"))
    })
    .await
}

/// After the spawn under the shim in `dir`: the `process` row and the lease journal's shim.
/// `None` when the shim's record cannot be read (reported; the session goes on unrecorded).
async fn record_shim(
    shared: &Shared,
    run: &Run,
    session_id: &SessionId,
    credential: &crate::agent::Issued,
    dir: &Path,
) -> Option<ShimRun> {
    let path = session_record(dir);
    let record = match Record::read(&path) {
        Ok(record) => record,
        Err(e) => {
            shared.trouble(format!(
                "{}: the shim's record {}: {e}",
                run.item,
                path.display()
            ));
            return None;
        }
    };
    let write = Write::Process(NewProcess {
        item: run.item.clone(),
        attempt: run.attempt,
        spawn: run.spawn,
        session_key: session_id.to_string(),
        pid: record.shim.pid,
        pid_started_ms: record.shim.start.raw().cast_signed(),
        record_path: path.to_string_lossy().into_owned(),
        credential_digest: Some(credential.digest().to_vec()),
        started_ms: now_ms(),
    });
    if let Err(e) = shared.deps.writer.write(vec![write]).await {
        shared.trouble(format!("{}: the session's shim: {e}", run.item));
        return None;
    }
    shared
        .deps
        .leases
        .set_shim(&Holder::Item(run.item.clone()), Some(record.shim));
    Some(ShimRun {
        record: path,
        shim: record.shim,
        written: (0, 0),
        written_at: Instant::now(),
    })
}

/// The session under the shim has ended: waits a little for the shim to exit, marks its row
/// gone and takes it off the lease journal. Whether the shim capped the session.
async fn shim_ended(shared: &Shared, run: &Run, shim_run: &ShimRun) -> bool {
    let deadline = Instant::now() + SHIM_EXIT;
    while bosun_proc::is_running(shim_run.shim) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let capped = Record::read(&shim_run.record).is_ok_and(|r| r.capped);
    shared
        .deps
        .leases
        .set_shim(&Holder::Item(run.item.clone()), None);
    let gone = Write::ProcessGone {
        record_path: shim_run.record.to_string_lossy().into_owned(),
        at_ms: now_ms(),
    };
    if let Err(e) = shared.deps.writer.write(vec![gone]).await {
        shared.trouble(format!("{}: the end of the session's shim: {e}", run.item));
    }
    capped
}

fn set_child(slot: &ChildSlot, key: Option<bosun_proc::ProcessKey>) {
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = key;
}

fn mode_name(mode: SpawnMode) -> &'static str {
    match mode {
        SpawnMode::Fresh => "fresh",
        SpawnMode::Resume => "resume",
        SpawnMode::Reset => "reset",
    }
}

/// Records the end of a session: the run row's exit and HEAD, and its commit range (#72).
async fn end(shared: &Shared, run: &Run, exit: Exit, start_head: Option<&str>, git_repo: &Git) {
    let (head, commits) = {
        let git_repo = git_repo.clone();
        let start = start_head.map(str::to_owned);
        blocking(move || {
            let head = git::head(&git_repo).ok();
            let commits = match (&start, &head) {
                (Some(start), Some(head)) => git::range(&git_repo, start, head).ok(),
                _ => None,
            };
            (head, commits)
        })
        .await
    };
    let mut writes = vec![Write::RunEnded(RunEnd {
        item: run.item.clone(),
        attempt: run.attempt,
        spawn: run.spawn,
        exit,
        head_sha: head.clone(),
        ended_ms: now_ms(),
    })];
    if let (Some(base), Some(head), Some(commits)) = (start_head, head, commits) {
        writes.push(Write::RunCommits(RunCommits {
            item: run.item.clone(),
            attempt: run.attempt,
            spawn: run.spawn,
            base_sha: base.to_owned(),
            head_sha: head,
            commits,
        }));
    }
    if let Err(e) = shared.deps.writer.write(writes).await {
        shared.trouble(format!("{}: the end of the session: {e}", run.item));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_cap_is_the_remaining_wall_budget_or_a_day() {
        assert_eq!(
            shim_cap(Some(Duration::from_hours(2)), Duration::from_mins(30)),
            Duration::from_mins(90)
        );
        assert_eq!(
            shim_cap(Some(Duration::from_hours(2)), Duration::from_hours(3)),
            Duration::ZERO
        );
        assert_eq!(
            shim_cap(None, Duration::from_mins(30)),
            Duration::from_hours(24)
        );
    }

    #[test]
    fn only_whole_lines_count_toward_an_offset() {
        assert_eq!(whole_line(b"{}\r\n"), 4);
        assert_eq!(whole_line(b"cut sho"), 0);
    }
}
