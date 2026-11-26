//! The MCP client side of `fake-agent`: what the agent CLI does with `--mcp-config <file>`.
//!
//! Before the script runs, every server of the file's `mcpServers` is started (its `command`,
//! `args` and `env` on top of the inherited environment) and initialized over newline-delimited
//! JSON-RPC on its stdin and stdout: `initialize`, `notifications/initialized`, `tools/list`.
//! As with the real CLI, a server that cannot be started, or that ends before answering, is
//! reported on stderr and left out; the session goes on without it.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// The protocol version the fake client asks for: the one the Claude CLI sent when this was
/// written.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// One connected server.
pub(crate) struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// The names `tools/list` returned.
    pub(crate) tools: Vec<String>,
}

impl Server {
    fn start(name: &str, config: &Value) -> Result<Self, String> {
        let command = config["command"]
            .as_str()
            .ok_or_else(|| format!("server {name}: no command"))?;
        #[allow(
            clippy::disallowed_methods,
            reason = "a test stand-in for an agent, never the daemon's own child"
        )]
        let mut cmd = Command::new(command);
        for arg in config["args"].as_array().into_iter().flatten() {
            cmd.arg(arg.as_str().unwrap_or_default());
        }
        for (key, value) in config["env"].as_object().into_iter().flatten() {
            cmd.env(key, value.as_str().unwrap_or_default());
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("server {name}: cannot start {command}: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            tools: Vec::new(),
        };
        let init = server.request(
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "fake-agent", "version": "0"}
            }),
        );
        let init = match init {
            Ok(init) => init,
            Err(e) => {
                server.stop();
                return Err(format!("server {name}: {e}"));
            }
        };
        if init.get("result").is_none() {
            server.stop();
            return Err(format!("server {name}: initialize failed: {init}"));
        }
        server.notify("notifications/initialized")?;
        let listed = server.request("tools/list", &json!({}))?;
        server.tools = listed["result"]["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("the server's stdin is closed")?;
        writeln!(stdin, "{message}")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("writing to the server: {e}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
    }

    /// Sends a request and reads lines until its response.
    pub(crate) fn request(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| format!("reading from the server: {e}"))?;
            if read == 0 {
                return Err(format!("the server ended before answering {method}"));
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == json!(id) {
                return Ok(message);
            }
        }
    }

    /// Closes the server's stdin and waits for it to end.
    fn stop(&mut self) {
        self.stdin = None;
        let _ = self.child.wait();
    }
}

/// The servers of a session, by their name in the configuration.
#[derive(Default)]
pub(crate) struct Servers {
    servers: BTreeMap<String, Server>,
}

impl Servers {
    /// Starts every server of the configuration file; those that fail are left out.
    pub(crate) fn start(file: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("cannot read the MCP configuration {}: {e}", file.display()))?;
        let config: Value = serde_json::from_str(&text)
            .map_err(|e| format!("the MCP configuration {}: {e}", file.display()))?;
        let mut servers = BTreeMap::new();
        for (name, server) in config["mcpServers"].as_object().into_iter().flatten() {
            match Server::start(name, server) {
                Ok(server) => {
                    servers.insert(name.clone(), server);
                }
                Err(e) => eprintln!("fake-agent: MCP {e}"),
            }
        }
        Ok(Self { servers })
    }

    /// A connected server.
    pub(crate) fn get(&mut self, name: &str) -> Result<&mut Server, String> {
        self.servers
            .get_mut(name)
            .ok_or_else(|| format!("no connected MCP server named {name:?}"))
    }

    /// Calls `tool` of `server` with `arguments`; the tool's result.
    pub(crate) fn call(
        &mut self,
        server: &str,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let response = self
            .get(server)?
            .request("tools/call", &json!({"name": tool, "arguments": arguments}))?;
        match response.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(format!("{server}/{tool}: {response}")),
        }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        for server in self.servers.values_mut() {
            server.stop();
        }
    }
}
