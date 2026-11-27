//! The Claude Code runner (feature `claude`, #52).
//!
//! # The command
//!
//! Headless print mode with streamed JSON, the prompt on standard input:
//!
//! ```text
//! claude -p --output-format stream-json --verbose
//!        --session-id <pre-assigned uuid>            (resume: --resume <uuid>)
//!        --mcp-config <per-session file> --strict-mcp-config
//!        --setting-sources project
//!        --settings <per-session file>
//!        --append-system-prompt-file <file>
//!        --permission-mode dontAsk
//!        --allowedTools <rule>...
//!        --disallowedTools <push rule>... <protected rule>... <rule>...
//!        [--model <model>] [--effort <level>]
//! ```
//!
//! with `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` ([`AUTO_MEMORY_OFF`]) added to the environment.
//!
//! - `--strict-mcp-config` makes the CLI load the servers of `--mcp-config` only, so neither
//!   the user's nor the repository's own MCP configuration (`.mcp.json`) reaches the session.
//!   The file, from [`mcp_config`], holds `bosun` (the `bosun-mcp` server of #7) and the
//!   repository's `repo.<name>.mcp_servers` (#4), nothing else.
//! - `--setting-sources project` ([`SETTING_SOURCES`]): the user's own settings and hooks are
//!   not loaded into an unattended session, only the repository's shared settings.
//! - `--settings <file>`, for fresh, reset and resumed sessions alike: the file, from
//!   [`session_settings`], holds one `PreCompact` hook and nothing else. A long session may
//!   compact its context, and the task lives in the user message; the hook prints the item's
//!   compaction instructions ([`crate::prompt::Prompt::compaction`]), which Claude Code uses
//!   for the summary: keep the task verbatim, the verdict command, the protected paths, the
//!   attempt and the last verdict result, and attribute no approval to the user, who has said
//!   nothing since the run was armed. The hook never blocks (a blocked auto-compaction at the
//!   context limit fails the request). Hook entries merge across settings levels, so the
//!   repository's own hooks still run.
//! - Auto memory is off ([`AUTO_MEMORY_OFF`], #135): the identity's `CLAUDE_CONFIG_DIR` is the
//!   user's own, and its `projects/<project>/memory/` would carry one session's notes into the
//!   prompt of every later session, unreviewed. What an agent learns goes into its summary.
//! - The tools of `bosun` and of the repository's servers are pre-allowed; the policy's
//!   rules follow. Push is denied unless the policy allows it ([`PUSH_RULES`]).
//! - `--disallowedTools` carries, after the push rules and before the policy's own deny rules,
//!   two `Edit(...)` rules for each protected path the session may not edit
//!   ([`protected_rules`]), so the CLI refuses such an edit when it happens; the guard before
//!   the verdict still judges every path the commits touch.
//! - `--effort <level>` is passed only when the card or the repository names a level (one of
//!   `bosun_core::EFFORT_LEVELS`, checked by the configuration and by lint); without one the
//!   CLI uses its own default.
//! - Each rule is one argument: a rule with a space (`Bash(git push:*)`) stays one rule.
//! - `--append-system-prompt-file` is accepted by the CLI although `--help` (2.1.329) lists
//!   only `--append-system-prompt`.
//!
//! # The stream (observed with Claude Code 2.1.329)
//!
//! One JSON object per line. The ones classification and the live tail read:
//!
//! - `{"type":"system","subtype":"init","session_id":...,"model":...,"permissionMode":...}`;
//!   `system` events with other subtypes (`hook_started`, `hook_response`, `thinking_tokens`)
//!   may come before and after it. Every event carries `session_id`; with `--session-id` it is
//!   the pre-assigned one.
//! - `{"type":"assistant","message":{"content":[{"type":"text"|"tool_use"|"thinking",...}]}}`
//!   and `{"type":"user","message":{"content":[{"type":"tool_result",...}]}}`.
//! - `{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":<epoch s>,
//!   "rateLimitType":"seven_day",...}}` after each API response.
//! - An API error is a synthetic assistant message: top-level `"error"` (`model_not_found`,
//!   `authentication_failed`), `"is_api_error_message":true`, `"api_error_status"` (404, 401),
//!   the message's model `<synthetic>`, its text the CLI's error line.
//! - The last line, `{"type":"result","subtype":...,"is_error":...,"api_error_status":...,
//!   "terminal_reason":...,"result":<final text>,"permission_denials":[...]}`. A normal end is
//!   `subtype` `success`, `is_error` false, `terminal_reason` `completed`, exit code 0. An API
//!   error is `is_error` true, `terminal_reason` `api_error`, exit code 1. A failure before the
//!   first turn (resuming an unknown session) is `subtype` `error_during_execution` with
//!   `errors`, exit code 1.
//! - A refused tool call (see the spike below): `{"type":"system","subtype":"permission_denied",
//!   "tool_name":...,"tool_use_id":...,"decision_reason_type":"rule","message":...}` when it
//!   happens, and one entry `{"tool_name":...,"tool_use_id":...,"tool_input":{...}}` of the
//!   result's `permission_denials` for each refusal of the session.
//!
//! # Denials
//!
//! [`Runner::denials`] reads the refused calls from the tail: the `result` line's
//! `permission_denials` when the tail has a result line (of several, each denial once, by its
//! `tool_use_id`), otherwise (a session killed or crashed before its result) the
//! `permission_denied` events still in the tail. A denial's target is
//! the call's subject from `tool_input`: `command` for the shell tools,
//! `file_path` for the file tools, `path` or `pattern` for the search tools, `url` for fetches,
//! else the compact JSON of the input; capped at [`Denial::TARGET_CAP`] characters. Its reason
//! is the matching `permission_denied` event's `decision_reason_type` (by `tool_use_id`) when
//! the tail still has the event. A refusal by a repository's `PreToolUse` hook is not read: it
//! shows only as an error `tool_result`, which the bounded tail does not keep.
//!
//! **Inferred, not observed** (a limit cannot be produced on demand): a usage limit is a
//! `rate_limit_event` whose `status` is `rejected`, with the reset in `resetsAt`, and an API
//! error with `"error":"rate_limit"` and status 429; older CLIs wrote the error text as
//! `Claude AI usage limit reached|<epoch s>`. Expired OAuth credentials are assumed to take the
//! observed shape of a rejected API key: `authentication_failed`, status 401.
//!
//! # Classification
//!
//! In order: exit code 0 with a `result` that is not an error is [`Exit::Ok`]; a rejected
//! `rate_limit_event`, a `rate_limit` API error or a status 429 is [`Exit::Limit`], its reset
//! from the rejected event's `resetsAt`, else from the legacy error text, else none (the
//! daemon's default backoff); an `authentication_failed` API error or a status 401 is
//! [`Exit::Auth`]; anything else is [`Exit::Crash`]. Only fields the CLI writes are read: the
//! agent's own text, and the `result` text that repeats it, never change the class.
//!
//! # Spike: a tool outside the allow list, headless
//!
//! Observed with `--permission-mode dontAsk` and with `manual`: the CLI does not wait for an
//! answer. The call is refused, a `{"type":"system","subtype":"permission_denied",
//! "tool_name":...}` event is written, the model gets an error `tool_result` ("Permission to
//! use Bash ... has been denied") and the session goes on; the denial is listed in the result's
//! `permission_denials`, and the session can still end normally with exit code 0. A call that
//! matches `--disallowedTools` is refused the same way even when the tool itself is allowed.
//! Read-only shell commands (`echo`) ran without being on the list: the CLI's own read-only
//! classification allows them. `--permission-prompt-tool` is not used in stage 1 (#7).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::{
    Denial, Exit, LiveEntry, McpServer, ProcessExit, Runner, SessionContext, SessionId, Spawn, Tail,
};

/// The name of the daemon's MCP server in the per-session configuration.
pub const BOSUN_SERVER: &str = "bosun";

/// The tools of `bosun-mcp` in stage 1 (#7), pre-allowed.
pub const BOSUN_TOOLS: &[&str] = &["ask_question", "submit_summary", "note"];

/// The deny rules that keep `git push` out of a session, for both shell tools of the CLI.
///
/// A rule matches a command prefix only, so `git -C . push` is not caught. The rules are a
/// courtesy to the agent; the enforcement is the agent environment without GitHub credentials
/// (#54).
pub const PUSH_RULES: &[&str] = &["Bash(git push:*)", "PowerShell(git push:*)"];

/// The setting sources an unattended session loads (`--setting-sources`): the repository's
/// shared `.claude/settings.json` only. Neither the user's settings under the identity's
/// `CLAUDE_CONFIG_DIR` (their hooks, allow rules and default mode are for interactive work) nor
/// the repository's git-ignored `settings.local.json` (also the user's) reach a run session;
/// the permission lists come from the policy on the command line (#62).
pub const SETTING_SOURCES: &str = "project";

/// The environment variable that turns Claude Code's auto memory off, and its value (#135):
/// "Set to `1` to disable auto memory ... When disabled, Claude does not create or load auto
/// memory files" (<https://code.claude.com/docs/en/env-vars>; also
/// <https://code.claude.com/docs/en/memory#enable-or-disable-auto-memory>). The variable rather
/// than `autoMemoryEnabled` through `--settings`: `0` forces auto memory on over the setting,
/// so the variable is the switch that wins, and no repository settings file can turn it back on.
pub const AUTO_MEMORY_OFF: (&str, &str) = ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");

/// The prompt a resumed session receives.
pub const RESUME_PROMPT: &str = "The session was interrupted (a usage limit, a sleep or a \
restart). Continue the task where you left off, and end as the exit protocol says.";

/// The Claude Code runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRunner {
    program: PathBuf,
}

impl ClaudeRunner {
    /// A runner that spawns `program`, the resolved `claude` executable.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn spawn(&self, session: [&str; 2], ctx: &SessionContext, stdin: String) -> Spawn {
        let mut args: Vec<OsString> = ["-p", "--output-format", "stream-json", "--verbose"]
            .map(OsString::from)
            .to_vec();
        args.extend(session.map(OsString::from));
        args.push("--mcp-config".into());
        args.push(ctx.mcp_config_file.clone().into());
        #[cfg(not(mutant_runner_loose_mcp))]
        args.push("--strict-mcp-config".into());
        args.extend(["--setting-sources", SETTING_SOURCES].map(OsString::from));
        // Mutant (`mutant_runner_settings_not_passed`): the session's settings are left out.
        if !cfg!(mutant_runner_settings_not_passed) {
            args.push("--settings".into());
            args.push(ctx.settings_file.clone().into());
        }
        args.push("--append-system-prompt-file".into());
        args.push(ctx.system_prompt_file.clone().into());
        args.extend(["--permission-mode", "dontAsk"].map(OsString::from));
        let allowed = allowed_tools(ctx);
        if !allowed.is_empty() {
            args.push("--allowedTools".into());
            args.extend(allowed.into_iter().map(OsString::from));
        }
        let denied = disallowed_tools(ctx);
        if !denied.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(denied.into_iter().map(OsString::from));
        }
        if let Some(model) = &ctx.model {
            args.push("--model".into());
            args.push(model.into());
        }
        if let Some(effort) = &ctx.effort {
            args.push("--effort".into());
            args.push(effort.into());
        }
        // Mutant `mutant_runner_auto_memory_on` (#135): the switch is not passed.
        let env: Vec<(OsString, OsString)> = if cfg!(mutant_runner_auto_memory_on) {
            Vec::new()
        } else {
            vec![(AUTO_MEMORY_OFF.0.into(), AUTO_MEMORY_OFF.1.into())]
        };
        Spawn {
            program: self.program.clone(),
            args,
            env,
            cwd: ctx.cwd.clone(),
            stdin: Some(stdin),
        }
    }
}

/// The rules of `--allowedTools`: the tools of `bosun`, every tool of the repository's MCP
/// servers, then the policy's rules.
#[must_use]
pub fn allowed_tools(ctx: &SessionContext) -> Vec<String> {
    let mut out: Vec<String> = BOSUN_TOOLS
        .iter()
        .map(|tool| format!("mcp__{BOSUN_SERVER}__{tool}"))
        .collect();
    out.extend(
        ctx.repo_mcp_servers
            .iter()
            .map(|name| format!("mcp__{name}")),
    );
    out.extend(ctx.policy.allow.iter().cloned());
    out
}

/// The rules of `--disallowedTools`: [`PUSH_RULES`] unless the policy allows push, the
/// [`protected_rules`] of each of the policy's protected paths, then the policy's rules.
#[must_use]
pub fn disallowed_tools(ctx: &SessionContext) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !ctx.policy.allow_push {
        out.extend(PUSH_RULES.iter().map(|rule| (*rule).to_owned()));
    }
    // Mutant (`mutant_runner_no_protected_rules`): the protected paths get no rules.
    if !cfg!(mutant_runner_no_protected_rules) {
        out.extend(
            ctx.policy
                .protected
                .iter()
                .flat_map(|entry| protected_rules(entry)),
        );
    }
    out.extend(ctx.policy.deny.iter().cloned());
    out
}

/// The two deny rules of a protected path (#7), a repository-relative `/`-separated entry
/// without wildcards (`bosun_config::Policy::protected`).
///
/// - An entry with a `/` names that path: `Edit(/<entry>)` and `Edit(/<entry>/**)`, anchored at
///   the working directory, the top of the repository.
/// - A bare name names a file or directory of that name at any depth: `Edit(<entry>)` and
///   `Edit(**/<entry>/**)`.
/// - An entry starting with `!` or `#` gets a `\` before it, gitignore's escape, so it is never
///   a negation or a comment.
///
/// The syntax, from <https://code.claude.com/docs/en/permissions> ("Read and Edit"): "`Edit`
/// rules apply to all built-in tools that edit files", and `Write(path)` and `NotebookEdit`
/// rules are accepted but never matched, so only `Edit` rules are generated. Paths use gitignore
/// pattern syntax: "`/path` Path relative to the settings source", which for "CLI flags or
/// session rules" is "`<primary working directory>/path`"; "Bare filenames follow gitignore
/// semantics and match at any depth"; "`*` matches within a single path segment [...] while
/// `**` matches across directories"; "A deny or ask pattern that starts with `!` is a gitignore
/// negation"; "On Windows, paths are normalized to POSIX form before matching". Deny is
/// evaluated before allow, so the rules hold against the default `Edit` allowance. They "don't
/// apply to arbitrary subprocesses": the rules are a courtesy, refusing an edit as it happens;
/// the guard before the verdict is the lock. Bash command lines are not pattern-matched.
#[must_use]
pub fn protected_rules(entry: &str) -> [String; 2] {
    let entry = entry.trim_matches('/');
    let escaped = if entry.starts_with(['!', '#']) {
        format!("\\{entry}")
    } else {
        entry.to_owned()
    };
    if entry.contains('/') {
        [format!("Edit(/{escaped})"), format!("Edit(/{escaped}/**)")]
    } else {
        [format!("Edit({escaped})"), format!("Edit(**/{escaped}/**)")]
    }
}

/// Why [`mcp_config`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConfigError {
    /// A server's configuration is not a JSON object.
    NotAnObject {
        /// The server.
        name: String,
    },
    /// Two servers share a name, or a repository server is named [`BOSUN_SERVER`].
    Duplicate {
        /// The name.
        name: String,
    },
}

impl std::fmt::Display for McpConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnObject { name } => {
                write!(
                    f,
                    "the configuration of MCP server {name:?} is not a JSON object"
                )
            }
            Self::Duplicate { name } => write!(f, "MCP server {name:?} is configured twice"),
        }
    }
}

impl std::error::Error for McpConfigError {}

/// The per-session MCP configuration file's text: `{"mcpServers": {...}}` with `bosun`
/// (configured by `bosun_config`, a JSON object) and the repository's servers, nothing else.
///
/// # Errors
///
/// [`McpConfigError`] for a configuration that is not a JSON object or a repeated name.
pub fn mcp_config(bosun_config: &str, repo: &[McpServer]) -> Result<String, McpConfigError> {
    let object = |name: &str, config: &str| match serde_json::from_str::<Value>(config) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(McpConfigError::NotAnObject {
            name: name.to_owned(),
        }),
    };
    let mut servers = Map::new();
    servers.insert(BOSUN_SERVER.to_owned(), object(BOSUN_SERVER, bosun_config)?);
    for server in repo {
        let value = object(&server.name, &server.config)?;
        if servers.insert(server.name.clone(), value).is_some() {
            return Err(McpConfigError::Duplicate {
                name: server.name.clone(),
            });
        }
    }
    let mut root = Map::new();
    root.insert("mcpServers".to_owned(), Value::Object(servers));
    Ok(Value::Object(root).to_string())
}

/// How long the `PreCompact` hook of [`session_settings`] may run, in seconds.
pub const COMPACT_HOOK_TIMEOUT: u64 = 10;

/// Why [`session_settings`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// The instructions file's path holds a `'`, which the hook's single quotes cannot hold.
    Quote,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quote => f.write_str("the compaction instructions' path holds a single quote"),
        }
    }
}

impl std::error::Error for SettingsError {}

/// The per-session settings file's text (`--settings`): one `PreCompact` hook, for every
/// matcher, whose command `cat '<instructions_file>'` prints the compaction instructions
/// ([`crate::prompt::Prompt::compaction`]) for Claude Code to use in the summary. Nothing
/// else: no `disableAllHooks`, so the repository's own hooks keep their setting.
///
/// The path's `\` become `/`; inside single quotes such a path works in `sh`, in Git Bash and
/// in PowerShell (where `cat` is `Get-Content`), the shells Claude Code runs a hook with.
///
/// # Errors
///
/// [`SettingsError::Quote`] for a path holding a `'`: it is refused rather than quoted.
pub fn session_settings(instructions_file: &Path) -> Result<String, SettingsError> {
    let path = instructions_file.to_string_lossy().replace('\\', "/");
    if path.contains('\'') {
        return Err(SettingsError::Quote);
    }
    // Mutant (`mutant_runner_compaction_hook_dropped`): the settings hold no hook.
    if cfg!(mutant_runner_compaction_hook_dropped) {
        return Ok("{}".to_owned());
    }
    let settings = serde_json::json!({
        "hooks": {
            "PreCompact": [{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": format!("cat '{path}'"),
                    "timeout": COMPACT_HOOK_TIMEOUT,
                }],
            }],
        },
    });
    Ok(settings.to_string())
}

impl Runner for ClaudeRunner {
    fn command(&self, ctx: &SessionContext) -> Spawn {
        self.spawn(
            ["--session-id", ctx.session_id.as_str()],
            ctx,
            ctx.prompt.clone(),
        )
    }

    fn session_id(&self, line: &str) -> Option<SessionId> {
        let value: Value = serde_json::from_str(line).ok()?;
        SessionId::parse(value.get("session_id")?.as_str()?).ok()
    }

    fn live_entries(&self, line: &str) -> Vec<LiveEntry> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        match (
            text(&value, "type").as_deref(),
            text(&value, "subtype").as_deref(),
        ) {
            (Some("system"), Some("init")) => vec![LiveEntry::Started {
                model: text(&value, "model"),
            }],
            (Some("system"), Some("permission_denied")) => vec![LiveEntry::Denied {
                tool: text(&value, "tool_name").unwrap_or_default(),
            }],
            (Some("assistant"), _) if is_api_error(&value) => {
                vec![LiveEntry::Error(message_text(&value))]
            }
            (Some("assistant"), _) => content(&value)
                .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("text") => text(block, "text").map(LiveEntry::Text),
                    Some("tool_use") => text(block, "name").map(|name| LiveEntry::Tool { name }),
                    _ => None,
                })
                .collect(),
            (Some("result"), _) => vec![LiveEntry::Ended {
                is_error: value.get("is_error").and_then(Value::as_bool) != Some(false),
            }],
            _ => Vec::new(),
        }
    }

    fn classify_exit(&self, exit: ProcessExit, tail: &Tail) -> Exit {
        classify(exit, &Seen::read(tail))
    }

    fn denials(&self, tail: &Tail) -> Vec<Denial> {
        // Mutant `mutant_runner_drop_denials`: the runner reads no denials.
        if cfg!(mutant_runner_drop_denials) {
            return Vec::new();
        }
        read_denials(tail)
    }

    fn resume_command(&self, id: &SessionId, ctx: &SessionContext) -> Spawn {
        self.spawn(["--resume", id.as_str()], ctx, RESUME_PROMPT.to_owned())
    }
}

/// The denials of a tail (see the module doc's "Denials").
fn read_denials(tail: &Tail) -> Vec<Denial> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut events: Vec<Value> = Vec::new();
    let mut listed: Option<Vec<Value>> = None;
    for line in tail.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match (
            text(&event, "type").as_deref(),
            text(&event, "subtype").as_deref(),
        ) {
            (Some("system"), Some("permission_denied")) => events.push(event),
            (Some("result"), _) => {
                // A session has one result line; should a tail hold more, a denial listed by
                // several is read once.
                let listed = listed.get_or_insert_with(Vec::new);
                for entry in event
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let id = entry.get("tool_use_id").and_then(Value::as_str);
                    let repeated = id.is_some()
                        && listed
                            .iter()
                            .any(|e| e.get("tool_use_id").and_then(Value::as_str) == id);
                    if !repeated {
                        listed.push(entry.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let reason_of = |id: Option<&str>| {
        let id = id?;
        events
            .iter()
            .find(|e| e.get("tool_use_id").and_then(Value::as_str) == Some(id))
            .and_then(|e| text(e, "decision_reason_type"))
    };
    match listed {
        Some(listed) => listed
            .iter()
            .map(|entry| Denial {
                tool: text(entry, "tool_name").unwrap_or_default(),
                target: entry.get("tool_input").and_then(target_of),
                reason: reason_of(entry.get("tool_use_id").and_then(Value::as_str)),
            })
            .collect(),
        None => events
            .iter()
            .map(|event| Denial {
                tool: text(event, "tool_name").unwrap_or_default(),
                target: None,
                reason: text(event, "decision_reason_type"),
            })
            .collect(),
    }
}

/// The subject of a tool call from its `tool_input`: `command` (the shells), `file_path` (the
/// file tools), `path` or `pattern` (the search tools), `url` (fetches), else the compact JSON
/// of the input; capped at [`Denial::TARGET_CAP`] characters. `None` for a missing or empty
/// input.
fn target_of(input: &Value) -> Option<String> {
    let named = ["command", "file_path", "path", "pattern", "url"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str));
    let target = match named {
        Some(text) => text.to_owned(),
        None if input.is_null() || input.as_object().is_some_and(Map::is_empty) => return None,
        None => input.to_string(),
    };
    Some(cap_chars(&target, Denial::TARGET_CAP))
}

/// `text` cut to at most `cap` characters, on a character boundary.
fn cap_chars(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
        Some((at, _)) => text[..at].to_owned(),
        None => text.to_owned(),
    }
}

fn content(event: &Value) -> impl Iterator<Item = &Value> {
    event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The text blocks of an assistant event, joined.
fn message_text(event: &Value) -> String {
    content(event)
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An assistant event the CLI synthesised for an API error.
fn is_api_error(event: &Value) -> bool {
    event.get("is_api_error_message").and_then(Value::as_bool) == Some(true)
        || event.get("error").is_some_and(Value::is_string)
}

/// An API error as the CLI reported it.
#[derive(Debug, Default)]
struct ApiError {
    /// The CLI's error kind (`rate_limit`, `authentication_failed`, ...).
    kind: Option<String>,
    status: Option<u64>,
    text: String,
}

/// The `result` event.
#[derive(Debug)]
struct ResultEvent {
    is_error: bool,
    subtype: Option<String>,
    status: Option<u64>,
}

/// What the tail holds that classification reads.
#[derive(Debug, Default)]
struct Seen {
    result: Option<ResultEvent>,
    api_error: Option<ApiError>,
    /// A `rate_limit_event` was rejected.
    rejected: bool,
    /// The `resetsAt` of the last rejected `rate_limit_event`, if it had one.
    rejected_reset: Option<u64>,
    /// The agent's last text. Read only by a mutant.
    #[cfg_attr(not(mutant_runner_question_from_text), allow(dead_code))]
    last_text: Option<String>,
}

impl Seen {
    fn read(tail: &Tail) -> Self {
        let mut seen = Self::default();
        for line in tail.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue; // a torn line, or something that is not the CLI's
            };
            match event.get("type").and_then(Value::as_str) {
                Some("result") => {
                    seen.result = Some(ResultEvent {
                        is_error: event.get("is_error").and_then(Value::as_bool) != Some(false),
                        subtype: event
                            .get("subtype")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        status: event.get("api_error_status").and_then(Value::as_u64),
                    });
                }
                Some("assistant") if is_api_error(&event) => {
                    seen.api_error = Some(ApiError {
                        kind: event
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        status: event.get("api_error_status").and_then(Value::as_u64),
                        text: message_text(&event),
                    });
                }
                Some("assistant") => {
                    let text = message_text(&event);
                    if !text.is_empty() {
                        seen.last_text = Some(text);
                    }
                }
                Some("rate_limit_event") => {
                    let info = event.get("rate_limit_info");
                    let status = info.and_then(|i| i.get("status")).and_then(Value::as_str);
                    if status == Some("rejected") {
                        seen.rejected = true;
                        seen.rejected_reset =
                            info.and_then(|i| i.get("resetsAt")).and_then(Value::as_u64);
                    }
                }
                _ => {}
            }
        }
        seen
    }

    fn status_is(&self, code: u64) -> bool {
        self.api_error.as_ref().and_then(|e| e.status) == Some(code)
            || self.result.as_ref().and_then(|r| r.status) == Some(code)
    }

    fn kind_is(&self, kind: &str) -> bool {
        self.api_error.as_ref().and_then(|e| e.kind.as_deref()) == Some(kind)
    }
}

fn classify(exit: ProcessExit, seen: &Seen) -> Exit {
    #[cfg(mutant_runner_question_from_text)]
    if seen.last_text.as_deref().is_some_and(|t| t.contains('?')) {
        // Mutant: a question in the agent's text is taken for a question.
        return Exit::Question;
    }

    if exit.code == Some(0) && seen.result.as_ref().is_some_and(|r| !r.is_error) {
        return Exit::Ok;
    }

    let limited = seen.rejected || seen.kind_is("rate_limit") || seen.status_is(429);
    if limited && !cfg!(mutant_runner_limit_is_crash) {
        let reset = seen
            .rejected_reset
            .or_else(|| seen.api_error.as_ref().and_then(|e| legacy_reset(&e.text)))
            .map(epoch);
        return Exit::Limit { reset };
    }

    if seen.kind_is("authentication_failed") || seen.status_is(401) {
        return Exit::Auth;
    }

    let status = exit
        .code
        .map_or_else(|| "killed".to_owned(), |code| format!("exit code {code}"));
    let reason = match &seen.result {
        None => format!("{status}, no result event"),
        Some(result) if result.is_error => format!(
            "{status}, result {} with an error",
            result.subtype.as_deref().unwrap_or("without a subtype")
        ),
        Some(_) => format!("{status} after a successful result"),
    };
    Exit::Crash { reason }
}

/// The reset of the legacy limit text `Claude AI usage limit reached|<epoch s>`.
fn legacy_reset(text: &str) -> Option<u64> {
    let (_, after) = text.split_once("usage limit reached|")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Epoch seconds, or milliseconds when the number is too large to be seconds.
fn epoch(value: u64) -> SystemTime {
    let duration = if value >= 100_000_000_000 {
        Duration::from_millis(value)
    } else {
        Duration::from_secs(value)
    };
    SystemTime::UNIX_EPOCH + duration
}

#[cfg(test)]
mod tests;
