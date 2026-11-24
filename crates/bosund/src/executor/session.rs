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
//! [`Options::handshake`]: super::Options::handshake

use std::collections::BTreeSet;
use std::ffi::OsString;
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
use bosun_runner::claude::{ClaudeRunner, mcp_config};
use bosun_runner::{
    LiveEntry, McpServer, ProcessExit, Runner, SessionContext, SessionId, Tail, ToolPolicy, prompt,
};
use bosun_store::{RunCommits, RunEnd, RunStart, Write};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use super::park::ChildSlot;
use super::verdict::killed;
use super::{Claimed, Shared, blocking, git, identity, lookup, now_ms, workspace};
use crate::agent::Binding;
use crate::events;

/// The provider name of the Claude runner's sessions in the store.
const PROVIDER: &str = "claude";

/// How long the output is still read after the session's process ended.
const DRAIN: Duration = Duration::from_secs(2);

/// Which session this is.
#[derive(Debug, Clone)]
pub(crate) struct Run {
    pub(crate) item: ItemId,
    pub(crate) attempt: u32,
    pub(crate) spawn: u32,
    pub(crate) mode: SpawnMode,
    /// Where the session's child is recorded while it runs, for the wake check (#66).
    pub(crate) child: ChildSlot,
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
    let credential = shared.deps.credentials.issue(Binding {
        identity: repo.identity.clone(),
        item: run.item.clone(),
        attempt: run.attempt,
        session: session_id.to_string(),
        repo: repo.path.clone(),
        writer: shared.deps.writer.clone(),
        on_blocking,
    });
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
    let spawned = match shared.deps.spawner.spawn(&spec) {
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
    let bosun_proc::Spawned {
        mut tree,
        stdout,
        stderr,
    } = spawned;
    let key = tree.key();
    set_child(&run.child, Some(key));
    let child = Write::SessionChild {
        item: run.item.clone(),
        attempt: run.attempt,
        pid: key.pid,
        pid_started_ms: key.start.raw().cast_signed(),
    };
    if let Err(e) = shared.deps.writer.write(vec![child]).await {
        shared.trouble(format!("{}: the session's child: {e}", run.item));
    }
    shared.publish(EventBody::SessionStarted {
        item: run.item.to_string(),
        attempt: run.attempt,
        spawn: run.spawn,
        mode: mode_name(run.mode).to_owned(),
        session: Some(session_id.to_string()),
    });

    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let (mut out_line, mut err_line) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    let mut tail = Tail::default();
    let mismatch = std::sync::atomic::AtomicBool::new(false);
    // The session came up (the CLI's `init`): a crash after it is not a failed resume.
    let up = std::sync::atomic::AtomicBool::new(false);
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
    let mut awaiting_hello = !cfg!(mutant_mcp_skip_handshake);
    let mut missed = false;
    let status = {
        let mut wait = tree.wait();
        loop {
            tokio::select! {
                () = &mut handshake, if awaiting_hello => {
                    awaiting_hello = false;
                    if !credential.said_hello() {
                        missed = true;
                        break None;
                    }
                }
                read = out.read_until(b'\n', &mut out_line), if out_open => {
                    if matches!(read, Ok(0) | Err(_)) {
                        out_open = false;
                    }
                    if !out_line.is_empty() {
                        on_line(OutputStream::Stdout, &out_line, &mut tail);
                        out_line.clear();
                    }
                }
                read = err.read_until(b'\n', &mut err_line), if err_open => {
                    if matches!(read, Ok(0) | Err(_)) {
                        err_open = false;
                    }
                    if !err_line.is_empty() {
                        on_line(OutputStream::Stderr, &err_line, &mut tail);
                        err_line.clear();
                    }
                }
                status = &mut wait => break Some(status),
                () = killed(&mut kill) => break None,
            }
        }
    };
    // The child has exited or is being killed: no wake check may take it for dead now.
    set_child(&run.child, None);
    // Whatever the session left running in its container goes with it, and so does its
    // credential.
    // A session that ended before the timeout must have said hello too.
    let missed = missed || (awaiting_hello && status.is_some() && !credential.said_hello());
    // Whatever the session left running in its container goes with it, and so do its
    // credential and its files.
    let _ = tree.kill_tree();
    drop(credential);
    drop(session_dir);
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
                    on_line(OutputStream::Stdout, &out_line, &mut tail);
                    out_line.clear();
                }
            }
            read = err.read_until(b'\n', &mut err_line), if err_open => {
                if matches!(read, Ok(0) | Err(_)) {
                    err_open = false;
                }
                if !err_line.is_empty() {
                    on_line(OutputStream::Stderr, &err_line, &mut tail);
                    err_line.clear();
                }
            }
            () = tokio::time::sleep(left) => break,
        }
    }
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
