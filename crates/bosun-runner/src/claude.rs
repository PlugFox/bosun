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
//!        --append-system-prompt-file <file>
//!        --permission-mode dontAsk
//!        --allowedTools <rule>... --disallowedTools <rule>...
//!        [--model <model>]
//! ```
//!
//! - `--strict-mcp-config` makes the CLI load the servers of `--mcp-config` only, so neither
//!   the user's nor the repository's own MCP configuration (`.mcp.json`) reaches the session.
//!   The file, from [`mcp_config`], holds `bosun` (the `bosun-mcp` server of #7) and the
//!   repository's `repo.<name>.mcp_servers` (#4), nothing else.
//! - The tools of `bosun` and of the repository's servers are pre-allowed; the policy's
//!   rules follow. Push is denied unless the policy allows it ([`PUSH_RULES`]).
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
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::{
    Exit, LiveEntry, McpServer, ProcessExit, Runner, SessionContext, SessionId, Spawn, Tail,
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
        Spawn {
            program: self.program.clone(),
            args,
            env: Vec::new(),
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

/// The rules of `--disallowedTools`: [`PUSH_RULES`] unless the policy allows push, then the
/// policy's rules.
#[must_use]
pub fn disallowed_tools(ctx: &SessionContext) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !ctx.policy.allow_push {
        out.extend(PUSH_RULES.iter().map(|rule| (*rule).to_owned()));
    }
    out.extend(ctx.policy.deny.iter().cloned());
    out
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

    fn resume_command(&self, id: &SessionId, ctx: &SessionContext) -> Spawn {
        self.spawn(["--resume", id.as_str()], ctx, RESUME_PROMPT.to_owned())
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
