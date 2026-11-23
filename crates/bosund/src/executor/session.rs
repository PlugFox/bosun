//! One agent session (#7, #62): the fail-closed checks, the prompt, the agent environment, the
//! run row before the spawn, the stream, the exit and the session's commit range.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun_api::{EventBody, OutputStream};
use bosun_config::{Config, Repo};
use bosun_core::{Event, Exit, ItemId, Rules, SpawnMode};
use bosun_git::Git;
use bosun_proc::ChildSpec;
use bosun_runner::claude::{ClaudeRunner, mcp_config};
use bosun_runner::{
    LiveEntry, McpServer, ProcessExit, Runner, SessionContext, SessionId, Tail, ToolPolicy, prompt,
};
use bosun_store::{RunCommits, RunEnd, RunStart, Write};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use super::verdict::killed;
use super::{Claimed, Shared, blocking, git, identity, lookup, now_ms};
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

fn exit_of(exit: &bosun_runner::Exit) -> Exit {
    match *exit {
        bosun_runner::Exit::Ok => Exit::Ok,
        bosun_runner::Exit::Limit { reset } => Exit::Limit {
            reset_ms: reset.and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
            }),
        },
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
/// previous attempt's verdict output from the store (#2: never from the board).
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

/// The per-session MCP configuration: `bosun` and the repository's servers (#7). The `bosun`
/// server's own handshake and credential are #63.
fn mcp_file(repo: &Repo, item: &ItemId, id: &SessionId) -> Result<String, String> {
    let bosun = serde_json::json!({
        "command": "bosun-mcp",
        "args": ["--item", item.0, "--session", id.as_str()],
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

/// The id a resumed session continues: the attempt's last run's.
async fn resumed_id(shared: &Shared, run: &Run) -> Option<SessionId> {
    let runs = shared.deps.reader.runs(&run.item).await.ok()?;
    let last = runs.iter().rev().find(|r| r.attempt == run.attempt)?;
    SessionId::parse(&last.session_key).ok()
}

/// Runs one session to its end and records it. Returns the machine's event: its exit as the
/// runner classified it, `auth` when the identity checks refused (#2), `crash` when it could
/// not be started.
#[allow(clippy::too_many_lines)]
pub(crate) async fn session(
    shared: &Arc<Shared>,
    config: &Config,
    claimed: &Claimed,
    run: &Run,
    mut kill: watch::Receiver<bool>,
) -> Event {
    let crash = run.exited(Exit::Crash);
    let Some((repo, identity)) = lookup(config, &shared.options.repo) else {
        return crash;
    };
    let Some(program) = shared.options.agent.clone() else {
        shared.trouble("the agent CLI `claude` was not found on PATH");
        return crash;
    };
    // Fail closed before spawn (#2, rule 2). There is no identity reason in the store yet:
    // a refusal here is Needs you as `auth`, with the refusal logged.
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
            return run.exited(Exit::Auth);
        }
    };

    let session_id = match run.mode {
        SpawnMode::Resume => match resumed_id(shared, run).await {
            Some(id) => id,
            None => new_session_id(run),
        },
        SpawnMode::Fresh | SpawnMode::Reset => new_session_id(run),
    };
    let git_repo = Git::new(&repo.path);
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

    let prompt = prompt_of(shared, claimed, run).await;
    let dir = shared
        .options
        .work_dir
        .join("sessions")
        .join(session_id.as_str());
    let files = (|| -> Result<(PathBuf, PathBuf), String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let system = dir.join("system.md");
        std::fs::write(&system, &prompt.system).map_err(|e| e.to_string())?;
        let mcp = dir.join("mcp.json");
        std::fs::write(&mcp, mcp_file(repo, &run.item, &session_id)?).map_err(|e| e.to_string())?;
        Ok((system, mcp))
    })();
    let (system_file, mcp_config_file) = match files {
        Ok(files) => files,
        Err(e) => {
            shared.trouble(format!("{}: the session files: {e}", run.item));
            return crash;
        }
    };
    let runner = ClaudeRunner::new(&program);
    let ctx = SessionContext {
        cwd: repo.path.clone(),
        session_id: session_id.clone(),
        prompt: prompt.user,
        system_prompt_file: system_file,
        mcp_config_file,
        repo_mcp_servers: repo.mcp_servers.keys().cloned().collect(),
        policy: ToolPolicy {
            allow_push: repo.policy.push,
            ..ToolPolicy::default()
        },
        model: claimed.snapshot.contract.model.clone(),
    };
    let command = match run.mode {
        SpawnMode::Resume => runner.resume_command(&session_id, &ctx),
        SpawnMode::Fresh | SpawnMode::Reset => runner.command(&ctx),
    };
    let env = match identity::agent_env(&bundle, &shared.options.work_dir) {
        Ok(env) => env,
        Err(e) => {
            shared.trouble(format!("{}: the agent environment: {e}", run.item));
            return crash;
        }
    };
    let mut vars: Vec<(OsString, OsString)> =
        env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect();
    vars.extend(command.env.iter().cloned());
    // What the trailer hook of #59 reads; harmless without it.
    for (name, value) in [
        ("BOSUN_ITEM", run.item.0.clone()),
        ("BOSUN_RUN", git::run_label(run.attempt, run.spawn)),
        ("BOSUN_SESSION", session_id.to_string()),
    ] {
        vars.push((name.into(), value.into()));
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
        stdin: command.stdin.is_some(),
    };
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
        stdin,
    } = spawned;
    let key = tree.key();
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
    if let (Some(mut pipe), Some(text)) = (stdin, command.stdin.clone()) {
        tokio::spawn(async move {
            let _ = pipe.write_all(text.as_bytes()).await;
            let _ = pipe.shutdown().await;
        });
    }

    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let (mut out_line, mut err_line) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    let mut tail = Tail::default();
    let mismatch = std::sync::atomic::AtomicBool::new(false);
    let on_line = |stream: OutputStream, bytes: &[u8], tail: &mut Tail| {
        let line = String::from_utf8_lossy(bytes).trim_end().to_owned();
        let texts: Vec<String> = match stream {
            OutputStream::Stdout => {
                let texts = runner.live_entries(&line).iter().map(entry_text).collect();
                if let Some(reported) = runner.session_id(&line)
                    && reported != session_id
                    && !mismatch.swap(true, Ordering::Relaxed)
                {
                    eprintln!(
                        "bosund: {}: the session reports id {reported}, not {session_id}",
                        run.item
                    );
                }
                tail.push(line);
                texts
            }
            OutputStream::Stderr => vec![line],
        };
        for text in texts {
            shared.publish(EventBody::SessionOutput {
                item: run.item.to_string(),
                attempt: run.attempt,
                spawn: run.spawn,
                stream,
                line: text,
            });
        }
    };
    let status = {
        let mut wait = tree.wait();
        loop {
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
                status = &mut wait => break Some(status),
                () = killed(&mut kill) => break None,
            }
        }
    };
    // Whatever the session left running in its container goes with it.
    let _ = tree.kill_tree();
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
    run.exited(exit)
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
