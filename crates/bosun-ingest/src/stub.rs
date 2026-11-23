//! A stub parser for a synthetic second provider (`fixtures/stub/`), #6 and #20.
//!
//! It exists to prove that the event model has room for a provider that is not Claude: its
//! format shares nothing with Claude Code's, and it emits the same [`Record`]s with no new
//! field. It is not a real provider and is never configured.
//!
//! The format, one JSON object per line in `sessions/<session>.jsonl`:
//!
//! - `{"event": "turn", "session", "id", "at", "dir", "model", "tokens": {"fresh", "cached",
//!   "written", "generated"}, "calls": [{"id", "tool", "arg"}]}`: one API message, never split.
//!   `written` tokens are 5 min cache writes; the provider has no 1 h cache. Tools are `open`
//!   (reads the file `arg`), `save` (writes it) and `shell` (runs the command `arg`).
//! - `{"event": "result", "session", "id", "bytes", "failed"}`: the result of call `id`.
//! - `{"event": "start", ...}`: known, with nothing to bill.

use std::collections::BTreeMap;
use std::path::Path;

use bosun_core::{Message, Origin, Tokens, ToolCall, Usage, WindowKey};
use serde_json::{Map, Value};

use crate::line::{LineParser, Outcome, gap, len64};
use crate::normalise::{absolute, command_head};
use crate::record::{FileTouch, Record, Source, ToolResult, TouchKind};
use crate::time;

/// The provider name.
pub const PROVIDER: &str = "stub";

/// The version of this parser.
pub const PARSER_VERSION: u32 = 1;

/// The parser of stub transcript lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct Parser;

/// The window of `sessions/<session>.jsonl`: the stub has no sub-agents.
#[must_use]
pub fn locate(path: &Path) -> Option<WindowKey> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    (!stem.is_empty()).then(|| WindowKey {
        session_id: stem.to_owned(),
        agent_id: None,
    })
}

impl LineParser for Parser {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn version(&self) -> u32 {
        PARSER_VERSION
    }

    fn parse_line(&self, source: &Source, offset: u64, line: &[u8]) -> Outcome {
        let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(line) else {
            return Outcome::Gap(gap(offset, line, None, Vec::new(), "not a JSON object"));
        };
        let keys = object.keys().cloned().collect();
        let Some(event) = object.get("event").and_then(Value::as_str) else {
            return Outcome::Gap(gap(offset, line, None, keys, "no string `event`"));
        };
        if object.get("session").and_then(Value::as_str) != Some(&source.window.session_id) {
            return Outcome::Gap(gap(
                offset,
                line,
                Some(event),
                keys,
                "not the file's session",
            ));
        }
        let parsed = match event {
            "turn" => turn(source, &object),
            "result" => result(&object).map(|r| vec![r]),
            "start" => Some(Vec::new()),
            unknown => {
                return Outcome::Skipped {
                    record_type: unknown.to_owned(),
                };
            }
        };
        match parsed {
            Some(records) => Outcome::Records(records),
            None => Outcome::Gap(gap(offset, line, Some(event), keys, "unexpected shape")),
        }
    }
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn turn(source: &Source, object: &Map<String, Value>) -> Option<Vec<Record>> {
    let id = text(object, "id")?;
    let dir = text(object, "dir")?;
    let counts = object.get("tokens")?.as_object()?;
    let count = |key: &str| counts.get(key).and_then(Value::as_u64);
    let tokens = Tokens {
        input: count("fresh")?,
        cache_write_5m: count("written")?,
        cache_write_1h: 0,
        cache_read: count("cached")?,
        output: count("generated")?,
    };
    let mut records = Vec::new();
    let mut tool_calls = BTreeMap::new();
    for call in object.get("calls")?.as_array()? {
        let call = call.as_object()?;
        let (call_id, tool, arg) = (text(call, "id")?, text(call, "tool")?, text(call, "arg")?);
        let mut touch = |kind| {
            let path = absolute(arg, dir);
            records.push(Record::FileTouch(FileTouch {
                provider: PROVIDER.to_owned(),
                message_id: id.to_owned(),
                tool_use_id: call_id.to_owned(),
                path: path.clone(),
                kind,
            }));
            Some(path)
        };
        let target = match tool {
            "open" => touch(TouchKind::Read),
            "save" => touch(TouchKind::Write),
            "shell" => command_head(arg),
            _ => None,
        };
        tool_calls.insert(
            call_id.to_owned(),
            ToolCall {
                name: tool.to_owned(),
                target,
                input_bytes: len64(arg.len()),
                ..ToolCall::default()
            },
        );
    }
    let message = Message {
        provider: PROVIDER.to_owned(),
        id: id.to_owned(),
        origin: Origin {
            ts_ms: time::parse_ms(text(object, "at")?)?,
            file: source.file.clone(),
            window: source.window.clone(),
        },
        model: text(object, "model")?.to_owned(),
        cwd: dir.to_owned(),
        usage: Usage {
            tokens,
            raw: Value::Object(counts.clone()),
        },
        tool_calls,
    };
    records.insert(0, Record::Message(message));
    Some(records)
}

fn result(object: &Map<String, Value>) -> Option<Record> {
    Some(Record::ToolResult(ToolResult {
        provider: PROVIDER.to_owned(),
        tool_use_id: text(object, "id")?.to_owned(),
        result_bytes: object.get("bytes")?.as_u64()?,
        is_error: object.get("failed")?.as_bool()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::Assembly;
    use crate::line::parse_lines;
    use crate::testing::{cases, expected, files, relative, roots, totals};

    #[test]
    fn the_stub_fills_the_event_model() {
        let cases = cases("stub");
        assert_ne!(cases.len(), 0);
        for case in cases {
            let sessions = case.join("sessions");
            let mut assembly = Assembly::default();
            let mut skipped = 0;
            for path in files(&sessions) {
                let key = relative(&path, &sessions);
                let source = Source {
                    window: locate(Path::new(&key)).unwrap(),
                    file: key,
                };
                let batch = parse_lines(&Parser, &source, 0, &std::fs::read(&path).unwrap());
                assert_eq!(batch.gaps().count(), 0);
                skipped += batch.skipped().values().sum::<u64>();
                for record in batch.records() {
                    assembly.add(record.clone());
                }
            }
            assert_eq!(skipped, 1, "{}", case.display());

            let messages = assembly.messages();
            assert_eq!(
                totals(&messages, &roots(&case)),
                expected(&case),
                "{}",
                case.display()
            );
            let calls: Vec<(&str, Option<&str>, u64, bool)> = messages
                .iter()
                .flat_map(|m| m.tool_calls.values())
                .map(|c| {
                    (
                        c.name.as_str(),
                        c.target.as_deref(),
                        c.result_bytes,
                        c.is_error,
                    )
                })
                .collect();
            assert_eq!(
                calls,
                [
                    ("open", Some("/fixture/repo-a/src/lib.rs"), 22, false),
                    ("shell", Some("cargo"), 30, true),
                ]
            );
            let touches: Vec<(&str, TouchKind)> = assembly
                .touches()
                .map(|t| (t.path.as_str(), t.kind))
                .collect();
            assert_eq!(touches, [("/fixture/repo-a/src/lib.rs", TouchKind::Read)]);
        }
    }
}
