//! The MCP side: JSON-RPC 2.0 over newline-delimited stdio, as the MCP specification's stdio
//! transport defines it, with the three tools of #7.
//!
//! Handled: `initialize`, `notifications/initialized` (and every other notification, ignored),
//! `ping`, `tools/list` and `tools/call`. Any other request is answered with "method not
//! found"; the Claude CLI probes `server/discover` first and falls back to `initialize` on that
//! answer. Responses the client sends (to requests this server never makes) are ignored.
//!
//! A tool call that the daemon refuses (an unauthorized credential, a rate cap) or that cannot
//! reach it within the retry window (the daemon gone, or restarting and not confirming the
//! call: see the crate's docs) is a tool result with `isError: true`, so the model sees why; a
//! call of an unknown tool, or with arguments that do not match its schema, is a JSON-RPC
//! error.

use bosun_api::AgentAck;
use serde_json::{Map, Value, json};

/// The MCP protocol versions this server speaks, newest first. `initialize` echoes the
/// client's version when it is one of these, otherwise it answers with the first.
pub(crate) const PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The server's name in `initialize`; the agent CLI names the tools `mcp__<config name>__<tool>`.
const SERVER_NAME: &str = "bosun";

/// What `initialize` tells the model about the server.
const INSTRUCTIONS: &str = "The daemon that runs this session. Report through these tools: \
    `submit_summary` when the work is done (it may be called again; the last call wins), \
    `ask_question` when you need the user (blocking: true ends the session), `note` for \
    anything worth keeping. Nothing you say here marks the task done: its verdict decides.";

/// JSON-RPC error codes of the specification.
mod code {
    pub(super) const PARSE_ERROR: i64 = -32700;
    pub(super) const INVALID_REQUEST: i64 = -32600;
    pub(super) const METHOD_NOT_FOUND: i64 = -32601;
    pub(super) const INVALID_PARAMS: i64 = -32602;
}

/// One tool call, with its arguments checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    /// `ask_question` → `agent.ask_question`.
    Question { text: String, blocking: bool },
    /// `submit_summary` → `agent.submit_summary`.
    Summary {
        body: String,
        decisions: Vec<String>,
        followups: Vec<String>,
    },
    /// `note` → `agent.note`.
    Note { text: String },
}

/// Where a tool call goes: the daemon's `agent.*` methods, or a fake in the tests.
pub(crate) trait Agent {
    /// Makes the call; `Err` holds the reason it was refused or did not arrive.
    async fn call(&mut self, call: Call) -> Result<AgentAck, String>;
}

/// The tools, as `tools/list` returns them: exactly these three (#7). There is no `done`.
pub(crate) fn tools() -> Vec<Value> {
    #[allow(unused_mut)]
    let mut tools = vec![
        json!({
            "name": "ask_question",
            "title": "Ask the user",
            "description": "Ask the user a question. With blocking: true you cannot go on \
                without the answer: the task goes to the user and this session ends shortly \
                after, so submit your summary first. A question that does not block goes into \
                the task's report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The question."},
                    "blocking": {
                        "type": "boolean",
                        "description": "True when you cannot continue without the answer."
                    }
                },
                "required": ["text", "blocking"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "submit_summary",
            "title": "Submit the summary",
            "description": "Submit the summary of your work on this task, with the decisions \
                you took and follow-ups for later tasks. Call it again when the work changes: \
                the last call wins. It does not mark the task done; its verdict decides.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "body": {"type": "string", "description": "What was done, and why."},
                    "decisions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Decisions taken on the way."
                    },
                    "followups": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Follow-ups for later tasks."
                    }
                },
                "required": ["body"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "note",
            "title": "Leave a note",
            "description": "Record a note for the task's report: something found on the way \
                that is worth keeping. It changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "The note."}},
                "required": ["text"],
                "additionalProperties": false
            }
        }),
    ];
    #[cfg(mutant_mcp_extra_tool)]
    tools.push(json!({
        "name": "done",
        "description": "Mark the task done.",
        "inputSchema": {"type": "object", "properties": {}}
    }));
    tools
}

/// Reads a tool call's arguments against its schema.
fn parse_call(name: &str, args: &Map<String, Value>) -> Result<Call, String> {
    let text = |key: &str| -> Result<String, String> {
        match args.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("`{key}` must be a string")),
            None => Err(format!("`{key}` is required")),
        }
    };
    let list = |key: &str| -> Result<Vec<String>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("`{key}` must be a list of strings"))
                })
                .collect(),
            Some(_) => Err(format!("`{key}` must be a list of strings")),
        }
    };
    let known: &[&str] = match name {
        "ask_question" => &["text", "blocking"],
        "submit_summary" => &["body", "decisions", "followups"],
        "note" => &["text"],
        other => return Err(format!("no tool named {other:?}")),
    };
    if let Some(extra) = args.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(format!("`{name}` takes no argument `{extra}`"));
    }
    Ok(match name {
        "ask_question" => Call::Question {
            text: text("text")?,
            blocking: match args.get("blocking") {
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("`blocking` must be true or false".to_owned()),
                None => return Err("`blocking` is required".to_owned()),
            },
        },
        "submit_summary" => Call::Summary {
            body: text("body")?,
            decisions: list("decisions")?,
            followups: list("followups")?,
        },
        _ => Call::Note {
            text: text("text")?,
        },
    })
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn acknowledged(ack: &AgentAck) -> String {
    match &ack.head_sha {
        Some(head) => format!("Recorded for attempt {} at commit {head}.", ack.attempt),
        None => format!("Recorded for attempt {}.", ack.attempt),
    }
}

fn response(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// The server's state: where its tool calls go.
#[derive(Debug)]
pub(crate) struct Server<A> {
    agent: A,
    version: String,
}

impl<A: Agent> Server<A> {
    pub(crate) fn new(agent: A, version: impl Into<String>) -> Self {
        Self {
            agent,
            version: version.into(),
        }
    }

    /// Answers one line from the client; `None` for a notification or a response.
    pub(crate) async fn handle(&mut self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => return Some(error(&Value::Null, code::PARSE_ERROR, e.to_string())),
        };
        let Value::Object(message) = message else {
            return Some(error(
                &Value::Null,
                code::INVALID_REQUEST,
                "a message is one JSON object; batches are not supported",
            ));
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        let (Some(method), Some(id)) = (method, id) else {
            // A notification (no id) or a response (no method): nothing to answer.
            return None;
        };
        let params = message
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Some(match method {
            "initialize" => response(&id, &self.initialize(&params)),
            "ping" => response(&id, &json!({})),
            "tools/list" => response(&id, &json!({"tools": tools()})),
            "tools/call" => self.tools_call(id, &params).await,
            other => error(&id, code::METHOD_NOT_FOUND, format!("no method {other:?}")),
        })
    }

    fn initialize(&self, params: &Map<String, Value>) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .filter(|v| PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": SERVER_NAME, "version": self.version},
            "instructions": INSTRUCTIONS,
        })
    }

    async fn tools_call(&mut self, id: Value, params: &Map<String, Value>) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error(&id, code::INVALID_PARAMS, "`name` is required");
        };
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(args)) => args.clone(),
            Some(_) => return error(&id, code::INVALID_PARAMS, "`arguments` must be an object"),
        };
        let call = match parse_call(name, &args) {
            Ok(call) => call,
            Err(e) => return error(&id, code::INVALID_PARAMS, e),
        };
        let result = match self.agent.call(call).await {
            Ok(ack) => text_result(&acknowledged(&ack), false),
            Err(reason) => text_result(&reason, true),
        };
        response(&id, &result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the calls; refuses the second note as the rate cap would.
    #[derive(Debug, Default)]
    struct Fake {
        calls: Vec<Call>,
    }

    impl Agent for Fake {
        #[allow(clippy::unused_async_trait_impl)]
        async fn call(&mut self, call: Call) -> Result<AgentAck, String> {
            self.calls.push(call);
            let notes = self
                .calls
                .iter()
                .filter(|c| matches!(c, Call::Note { .. }))
                .count();
            if notes > 1 {
                return Err("the daemon refused the call: over the cap".to_owned());
            }
            Ok(AgentAck {
                attempt: 2,
                head_sha: Some("abc123".to_owned()),
            })
        }
    }

    async fn ask(server: &mut Server<Fake>, request: Value) -> Value {
        server.handle(&request.to_string()).await.unwrap()
    }

    #[tokio::test]
    async fn tools_list_is_exactly_the_three_tools() {
        let mut server = Server::new(Fake::default(), "0.0.0");
        let listed = ask(
            &mut server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["ask_question", "submit_summary", "note"],
            "{listed}"
        );
        for tool in listed["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[tokio::test]
    async fn initialize_echoes_a_known_version_and_offers_tools() {
        let mut server = Server::new(Fake::default(), "1.2.3");
        // What the Claude CLI sent when this was written (2.1.329).
        let init = ask(
            &mut server,
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {"roots": {"listChanged": true}},
                "clientInfo": {"name": "synthetic-client", "version": "1"}
            }}),
        )
        .await;
        assert_eq!(init["id"], 0);
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(init["result"]["serverInfo"]["name"], "bosun");
        assert_eq!(init["result"]["serverInfo"]["version"], "1.2.3");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        let unknown = ask(
            &mut server,
            json!({"jsonrpc": "2.0", "id": "s", "method": "initialize",
                   "params": {"protocolVersion": "1999-01-01"}}),
        )
        .await;
        assert_eq!(unknown["id"], "s");
        assert_eq!(unknown["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[tokio::test]
    async fn notifications_and_responses_get_no_answer_and_unknown_methods_an_error() {
        let mut server = Server::new(Fake::default(), "0");
        for line in [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
        ] {
            assert_eq!(server.handle(&line.to_string()).await, None, "{line}");
        }
        // The probe the Claude CLI sends before `initialize`.
        let probe = ask(
            &mut server,
            json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover"}),
        )
        .await;
        assert_eq!(probe["id"], "server-discover-probe-1");
        assert_eq!(probe["error"]["code"], code::METHOD_NOT_FOUND);
        let ping = ask(
            &mut server,
            json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}),
        )
        .await;
        assert_eq!(ping["result"], json!({}));
        let garbage = server.handle("{not json").await.unwrap();
        assert_eq!(garbage["error"]["code"], code::PARSE_ERROR);
        assert_eq!(garbage["id"], Value::Null);
        let batch = server.handle("[]").await.unwrap();
        assert_eq!(batch["error"]["code"], code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn each_tool_maps_to_its_call_and_a_refusal_is_a_tool_error() {
        let mut server = Server::new(Fake::default(), "0");
        let call = |id: u32, name: &str, arguments: Value| {
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                   "params": {"name": name, "arguments": arguments}})
        };
        let asked = ask(
            &mut server,
            call(
                1,
                "ask_question",
                json!({"text": "Which?", "blocking": true}),
            ),
        )
        .await;
        assert_eq!(asked["result"]["isError"], false, "{asked}");
        assert!(
            asked["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("attempt 2 at commit abc123"),
            "{asked}"
        );
        ask(
            &mut server,
            call(
                2,
                "submit_summary",
                json!({"body": "Did it.", "decisions": ["a"]}),
            ),
        )
        .await;
        ask(&mut server, call(3, "note", json!({"text": "first"}))).await;
        let capped = ask(&mut server, call(4, "note", json!({"text": "second"}))).await;
        assert_eq!(capped["result"]["isError"], true, "{capped}");
        assert!(
            capped["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("over the cap")
        );
        assert_eq!(
            server.agent.calls,
            [
                Call::Question {
                    text: "Which?".to_owned(),
                    blocking: true
                },
                Call::Summary {
                    body: "Did it.".to_owned(),
                    decisions: vec!["a".to_owned()],
                    followups: Vec::new()
                },
                Call::Note {
                    text: "first".to_owned()
                },
                Call::Note {
                    text: "second".to_owned()
                },
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_or_bad_arguments_are_refused_before_the_daemon() {
        let mut server = Server::new(Fake::default(), "0");
        for (name, arguments) in [
            ("done", json!({})),
            ("report_verdict", json!({"passed": true})),
            ("note", json!({})),
            ("note", json!({"text": 3})),
            ("note", json!({"text": "x", "item": "other#2"})),
            ("ask_question", json!({"text": "x"})),
            ("submit_summary", json!({"body": "x", "decisions": "a"})),
        ] {
            let refused = ask(
                &mut server,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": name, "arguments": arguments}}),
            )
            .await;
            assert_eq!(
                refused["error"]["code"],
                code::INVALID_PARAMS,
                "{name}: {refused}"
            );
        }
        assert_eq!(server.agent.calls, []);
    }
}
