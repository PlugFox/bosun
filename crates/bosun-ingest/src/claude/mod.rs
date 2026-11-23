//! Claude Code transcripts (#3, "Claude Code transcript format").
//!
//! Record types are an allowlist, [`KNOWN_TYPES`]. Of them:
//!
//! - `assistant` yields a [`Record::Message`] (usage, model, cwd, timestamp, and a tool call
//!   per `tool_use` or `server_tool_use` block), a [`Record::FileTouch`] per file tool call,
//!   and one per distinct file and kind a `Bash` or `PowerShell` command names (#76);
//! - `user` yields a [`Record::ToolResult`] per `tool_result` block;
//! - `cost-state` yields a [`Record::SessionCheck`] in a main transcript, and nothing in a
//!   sub-agent's: the root session's check is the main transcript's (#46);
//! - the others yield nothing.
//!
//! Every line that is a JSON object with an RFC 3339 `timestamp` gives its timestamp, whatever
//! its type or outcome: the end of a session check's window is the last one before it.
//!
//! A type not in the list is skipped and counted. A known type in an unexpected shape is a gap
//! with a fixed reason. The shapes checked are those the records above are built from: the
//! identity fields, the timestamp, `message`, its usage counts and its content blocks. Tool
//! inputs are not checked: a tool call whose input lacks its path keeps no target.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bosun_core::{
    ContextWindow, Gap, Message, Origin, Tokens, ToolCall, Usage, WindowKey, WindowKind,
};
use serde_json::{Map, Value};

pub use crate::layout::{FileKind, Located};

use crate::layout::Provider;
use crate::line::{LineParser, Outcome, gap, len64};
use crate::normalise::{absolute, command_head, shell_absolute};
use crate::record::{
    FileTouch, ModelTotals, Record, SessionCheck, Source, ToolResult, TouchKind, WindowMeta,
};
use crate::shell::{self, Dialect};
use crate::time;

#[cfg(test)]
mod tests;

/// The provider name.
pub const PROVIDER: &str = "claude";

/// The version of this parser, recorded on every row it writes. Bump it whenever what a line
/// yields changes.
///
/// - 2: a `cost-state` record carries its window, and yields nothing in a sub-agent's file
///   (#46).
/// - 3: a `Bash` or `PowerShell` call yields a file touch per file its command names, with
///   the kinds `exec` and `unknown` (#76).
pub const PARSER_VERSION: u32 = 3;

/// The record types this parser knows. Any other type is skipped and counted.
pub const KNOWN_TYPES: &[&str] = &[
    // Records that yield something.
    "assistant",
    "user",
    "cost-state",
    // Records with nothing to bill.
    "system",
    "summary",
    "derived-from",
    "continued-in",
    "mode",
    "ai-title",
    "custom-title",
    "last-prompt",
    "file-history-snapshot",
    "queue-operation",
    "attachment",
    "progress",
];

/// The parser of Claude Code transcript lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct Parser;

impl LineParser for Parser {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn version(&self) -> u32 {
        PARSER_VERSION
    }

    fn parse_line(&self, source: &Source, offset: u64, line: &[u8]) -> Outcome {
        parse_line(source, offset, line).0
    }

    fn parse_timed(&self, source: &Source, offset: u64, line: &[u8]) -> (Outcome, Option<i64>) {
        parse_line(source, offset, line)
    }
}

impl Provider for Parser {
    fn locate(&self, path: &Path) -> Option<Located> {
        locate(path)
    }

    fn descend(&self, dir: &str) -> bool {
        // Spilled tool outputs: many files, none a transcript.
        dir != "tool-results"
    }

    fn meta_path(&self, transcript: &Path) -> Option<PathBuf> {
        // `agent-<id>.jsonl` is described by `agent-<id>.meta.json` beside it.
        let name = transcript.file_name()?.to_str()?;
        let stem = name.strip_suffix(".jsonl")?;
        stem.starts_with("agent-")
            .then(|| transcript.with_file_name(format!("{stem}.meta.json")))
    }

    fn parse_meta(&self, window: &WindowKey, bytes: &[u8]) -> Result<WindowMeta, Gap> {
        parse_meta(window, bytes)
    }
}

/// The context window of a transcript or meta file, from its path:
///
/// - `<project>/<session>.jsonl` is the main window of `<session>`;
/// - `<project>/<session>/subagents/agent-<agent>.jsonl` (and its `.meta.json`) is the window
///   of sub-agent `<agent>` of `<session>`.
///
/// Anything else (tool-result spills, other files) is `None`.
#[must_use]
pub fn locate(path: &Path) -> Option<Located> {
    let name = path.file_name()?.to_str()?;
    let (stem, kind) = if let Some(stem) = name.strip_suffix(".meta.json") {
        (stem, FileKind::Meta)
    } else {
        (name.strip_suffix(".jsonl")?, FileKind::Transcript)
    };
    let window = if let Some(agent) = stem.strip_prefix("agent-") {
        let subagents = path.parent()?;
        if subagents.file_name()? != "subagents" {
            return None;
        }
        let session = subagents.parent()?.file_name()?.to_str()?;
        if agent.is_empty() || session.is_empty() {
            return None;
        }
        WindowKey {
            session_id: session.to_owned(),
            agent_id: Some(agent.to_owned()),
        }
    } else if kind == FileKind::Transcript && !stem.is_empty() {
        WindowKey {
            session_id: stem.to_owned(),
            agent_id: None,
        }
    } else {
        return None;
    };
    Some(Located { window, kind })
}

/// Reads a sub-agent's `.meta.json`: its parent window (`parentAgentId`, or the session's main
/// window), type, spawn depth and spawning `tool_use` id.
///
/// # Errors
///
/// A [`Gap`] at offset 0 covering the file, with metadata only, when the file is not a JSON
/// object of the expected shape or `window` is not a sub-agent's.
pub fn parse_meta(window: &WindowKey, bytes: &[u8]) -> Result<WindowMeta, Gap> {
    let object = match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => object,
        Ok(_) => {
            return Err(gap(
                0,
                bytes,
                None,
                Vec::new(),
                "the meta file is not an object",
            ));
        }
        Err(_) => return Err(gap(0, bytes, None, Vec::new(), "the meta file is not JSON")),
    };
    meta(window, &object).map_err(|reason| gap(0, bytes, None, keys(&object), reason))
}

type Object = Map<String, Value>;
/// Why a line is a gap: always a fixed text.
type Reason = &'static str;

fn keys(object: &Object) -> Vec<String> {
    object.keys().cloned().collect()
}

fn meta(window: &WindowKey, object: &Object) -> Result<WindowMeta, Reason> {
    if window.agent_id.is_none() {
        return Err("a meta file belongs to a sub-agent, not a main window");
    }
    let parent = WindowKey {
        session_id: window.session_id.clone(),
        agent_id: optional_string(object, "parentAgentId")?.map(str::to_owned),
    };
    let spawn_depth = match optional_count(object, "spawnDepth")? {
        Some(depth) => Some(u32::try_from(depth).map_err(|_| "`spawnDepth` is out of range")?),
        None => None,
    };
    Ok(WindowMeta {
        window: ContextWindow {
            key: window.clone(),
            parent: Some(parent),
            // Stage 0 spawns nothing: every window is interactive (#6).
            kind: WindowKind::Interactive,
        },
        agent_type: optional_string(object, "agentType")?.map(str::to_owned),
        spawn_depth,
        spawned_by: optional_string(object, "toolUseId")?.map(str::to_owned),
    })
}

/// The outcome of a line and its timestamp, when it is an object with one.
fn parse_line(source: &Source, offset: u64, line: &[u8]) -> (Outcome, Option<i64>) {
    let object = match serde_json::from_slice::<Value>(line) {
        Ok(Value::Object(object)) => object,
        Ok(_) => {
            let gap = gap(offset, line, None, Vec::new(), "not a JSON object");
            return (Outcome::Gap(gap), None);
        }
        Err(_) => {
            return (
                Outcome::Gap(gap(offset, line, None, Vec::new(), "not JSON")),
                None,
            );
        }
    };
    let ts_ms = object
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(time::parse_ms);
    let Some(record_type) = object.get("type").and_then(Value::as_str) else {
        let reason = "no string `type`";
        return (
            Outcome::Gap(gap(offset, line, None, keys(&object), reason)),
            ts_ms,
        );
    };
    let parsed = match record_type {
        "assistant" => assistant(source, &object),
        "user" => user(source, &object),
        "cost-state" => cost_state(source, offset, &object),
        known if KNOWN_TYPES.contains(&known) => Ok(Vec::new()),
        unknown => {
            let skipped = Outcome::Skipped {
                record_type: unknown.to_owned(),
            };
            return (skipped, ts_ms);
        }
    };
    let outcome = match parsed {
        Ok(records) => Outcome::Records(records),
        Err(reason) => Outcome::Gap(gap(offset, line, Some(record_type), keys(&object), reason)),
    };
    (outcome, ts_ms)
}

/// The record's `sessionId` and `agentId`, when present, must name the file's window: one
/// session per file (#3).
fn check_window(source: &Source, object: &Object) -> Result<(), Reason> {
    match object.get("sessionId") {
        None => {}
        Some(Value::String(session)) if *session == source.window.session_id => {}
        Some(_) => return Err("`sessionId` is not the session of the file"),
    }
    match object.get("agentId") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(agent)) if source.window.agent_id.as_ref() == Some(agent) => Ok(()),
        Some(_) => Err("`agentId` is not the agent of the file"),
    }
}

fn string<'a>(object: &'a Object, key: &str, reason: Reason) -> Result<&'a str, Reason> {
    object.get(key).and_then(Value::as_str).ok_or(reason)
}

fn optional_string<'a>(object: &'a Object, key: &str) -> Result<Option<&'a str>, Reason> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err("a text field is not a string"),
    }
}

fn optional_count(object: &Object, key: &str) -> Result<Option<u64>, Reason> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or("a count is not a non-negative integer"),
    }
}

fn optional_number(object: &Object, key: &str) -> Result<Option<f64>, Reason> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or("a cost is not a number"),
    }
}

/// The content blocks of a `message`: none for text content.
fn blocks(message: &Object) -> Result<&[Value], Reason> {
    match message.get("content") {
        None | Some(Value::Null | Value::String(_)) => Ok(&[]),
        Some(Value::Array(blocks)) => Ok(blocks),
        Some(_) => Err("`message.content` is neither text nor a list of blocks"),
    }
}

/// A content block and its type.
fn block(value: &Value) -> Result<(&Object, &str), Reason> {
    let block = value
        .as_object()
        .ok_or("a content block is not an object")?;
    let kind = string(block, "type", "a content block has no string `type`")?;
    Ok((block, kind))
}

/// The five categories of #3. Without `cache_creation`, every cache write is a 5 min write.
fn tokens(usage: &Value) -> Result<Tokens, Reason> {
    let usage = usage
        .as_object()
        .ok_or("`message.usage` is not an object")?;
    let input = optional_count(usage, "input_tokens")?.ok_or("`input_tokens` is missing")?;
    let output = optional_count(usage, "output_tokens")?.ok_or("`output_tokens` is missing")?;
    let cache_read = optional_count(usage, "cache_read_input_tokens")?.unwrap_or(0);
    let written = optional_count(usage, "cache_creation_input_tokens")?.unwrap_or(0);
    let (minutes, hour) = match usage.get("cache_creation") {
        None | Some(Value::Null) => (written, 0),
        Some(Value::Object(split)) => (
            optional_count(split, "ephemeral_5m_input_tokens")?.unwrap_or(0),
            optional_count(split, "ephemeral_1h_input_tokens")?.unwrap_or(0),
        ),
        Some(_) => return Err("`cache_creation` is not an object"),
    };
    Ok(Tokens {
        input,
        cache_write_5m: minutes,
        cache_write_1h: hour,
        cache_read,
        output,
    })
}

fn assistant(source: &Source, object: &Object) -> Result<Vec<Record>, Reason> {
    check_window(source, object)?;
    let timestamp = string(object, "timestamp", "`timestamp` is missing")?;
    let ts_ms = time::parse_ms(timestamp).ok_or("`timestamp` is not RFC 3339")?;
    let cwd = string(object, "cwd", "`cwd` is missing")?;
    let message = object
        .get("message")
        .and_then(Value::as_object)
        .ok_or("`message` is not an object")?;
    let id = string(message, "id", "`message.id` is missing")?;
    let model = string(message, "model", "`message.model` is missing")?;
    let usage = message.get("usage").ok_or("`message.usage` is missing")?;
    let tokens = tokens(usage)?;

    let mut touches = Vec::new();
    let mut tool_calls = BTreeMap::new();
    for value in blocks(message)? {
        let (block, kind) = block(value)?;
        if !matches!(kind, "tool_use" | "server_tool_use") {
            continue;
        }
        let call_id = string(block, "id", "a `tool_use` block has no `id`")?;
        let name = string(block, "name", "a `tool_use` block has no `name`")?;
        let input = block.get("input").unwrap_or(&Value::Null);
        let (target, files) = target(name, input, cwd);
        for (path, kind, confined) in files {
            touches.push(Record::FileTouch(FileTouch {
                provider: PROVIDER.to_owned(),
                message_id: id.to_owned(),
                tool_use_id: call_id.to_owned(),
                path,
                kind,
                confined,
            }));
        }
        tool_calls.insert(
            call_id.to_owned(),
            ToolCall {
                name: name.to_owned(),
                target,
                input_bytes: len64(input.to_string().len()),
                result_bytes: 0,
                is_error: false,
            },
        );
    }

    let message = Message {
        provider: PROVIDER.to_owned(),
        id: id.to_owned(),
        origin: Origin {
            ts_ms,
            file: source.file.clone(),
            window: source.window.clone(),
        },
        model: model.to_owned(),
        cwd: cwd.to_owned(),
        usage: Usage {
            tokens,
            raw: usage.clone(),
        },
        tool_calls,
    };
    let mut records = vec![Record::Message(message)];
    records.extend(touches);
    Ok(records)
}

/// A file a tool call touches: its canonical absolute path, how, and whether it came out of a
/// command's text (see [`FileTouch::confined`]).
type Touched = (String, TouchKind, bool);

/// The normalised target of a tool call and the files it touches: one for a file tool, every
/// distinct (path, kind) a shell command names.
fn target(name: &str, input: &Value, cwd: &str) -> (Option<String>, Vec<Touched>) {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let file = |path: Option<&str>, kind: TouchKind| {
        path.map_or_default(|path| {
            let path = absolute(path, cwd);
            (Some(path.clone()), vec![(path, kind, false)])
        })
    };
    let shell = |dialect: Dialect| {
        let Some(command) = field("command") else {
            return (None, Vec::new());
        };
        let mut seen = BTreeSet::new();
        let touched = shell::touches(command, dialect)
            .into_iter()
            .map(|(path, kind)| (shell_absolute(&path, cwd), kind))
            .filter(|touch| seen.insert(touch.clone()))
            .map(|(path, kind)| (path, kind, true))
            .collect();
        (command_head(command), touched)
    };
    match name {
        "Read" => file(field("file_path"), TouchKind::Read),
        "Edit" | "MultiEdit" | "Write" => file(field("file_path"), TouchKind::Write),
        "NotebookEdit" => file(field("notebook_path"), TouchKind::Write),
        // A search without a path searches the working directory.
        "Grep" | "Glob" | "LS" => file(field("path").or(Some(cwd)), TouchKind::Search),
        "Bash" => shell(Dialect::Posix),
        "PowerShell" => shell(Dialect::PowerShell),
        _ => (None, Vec::new()),
    }
}

fn user(source: &Source, object: &Object) -> Result<Vec<Record>, Reason> {
    check_window(source, object)?;
    let message = object
        .get("message")
        .and_then(Value::as_object)
        .ok_or("`message` is not an object")?;
    let mut records = Vec::new();
    for value in blocks(message)? {
        let (block, kind) = block(value)?;
        if kind != "tool_result" {
            continue;
        }
        let tool_use_id = string(block, "tool_use_id", "a `tool_result` has no `tool_use_id`")?;
        let is_error = match block.get("is_error") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(is_error)) => *is_error,
            Some(_) => return Err("`is_error` is not a boolean"),
        };
        records.push(Record::ToolResult(ToolResult {
            provider: PROVIDER.to_owned(),
            tool_use_id: tool_use_id.to_owned(),
            result_bytes: result_bytes(block.get("content")),
            is_error,
        }));
    }
    Ok(records)
}

/// The size of a tool result: the bytes of its text, and of the base64 data of its images.
fn result_bytes(content: Option<&Value>) -> u64 {
    let size = |value: &Value| -> u64 {
        match value {
            Value::String(text) => len64(text.len()),
            Value::Object(block) => match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map_or(0, |text| len64(text.len())),
                Some("image") => block
                    .get("source")
                    .and_then(|source| source.get("data"))
                    .and_then(Value::as_str)
                    .map_or(0, |data| len64(data.len())),
                _ => len64(value.to_string().len()),
            },
            _ => len64(value.to_string().len()),
        }
    };
    match content {
        None | Some(Value::Null) => 0,
        Some(Value::Array(items)) => items.iter().map(size).sum(),
        Some(value) => size(value),
    }
}

/// The provider's totals of the session, from the main transcript only. A sub-agent's file
/// may hold a `cost-state` line too; it is not the root session's check, which it would
/// replace (#46), so it yields nothing.
fn cost_state(source: &Source, offset: u64, object: &Object) -> Result<Vec<Record>, Reason> {
    check_window(source, object)?;
    if source.window.agent_id.is_some() && !cfg!(mutant_ingest_subagent_cost_state) {
        // Mutant `mutant_ingest_subagent_cost_state`: a sub-agent's line is the session's check.
        return Ok(Vec::new());
    }
    let start_ms = match object.get("startTime") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64().ok_or("`startTime` is not an integer")?),
    };
    let usage = object
        .get("modelUsage")
        .and_then(Value::as_object)
        .ok_or("`modelUsage` is not an object")?;
    let mut models = BTreeMap::new();
    for (model, totals) in usage {
        let totals = totals
            .as_object()
            .ok_or("a `modelUsage` entry is not an object")?;
        let count = |key| optional_count(totals, key).map(Option::unwrap_or_default);
        let written = count("cacheCreationInputTokens")?;
        let split = (
            optional_count(totals, "cacheCreationEphemeral5mInputTokens")?,
            optional_count(totals, "cacheCreationEphemeral1hInputTokens")?,
        );
        let (minutes, hour) = match split {
            (None, None) => (written, 0),
            (minutes, hour) => (minutes.unwrap_or(0), hour.unwrap_or(0)),
        };
        let tokens = Tokens {
            input: count("inputTokens")?,
            cache_write_5m: minutes,
            cache_write_1h: hour,
            cache_read: count("cacheReadInputTokens")?,
            output: count("outputTokens")?,
        };
        let cost_usd = optional_number(totals, "costUSD")?;
        models.insert(model.clone(), ModelTotals { tokens, cost_usd });
    }
    Ok(vec![Record::SessionCheck(SessionCheck {
        provider: PROVIDER.to_owned(),
        session_id: source.window.session_id.clone(),
        offset,
        start_ms,
        // Set by the reader of the file, which knows the lines before this one.
        end_ms: None,
        total_cost_usd: optional_number(object, "totalCostUSD")?,
        models,
    })])
}
