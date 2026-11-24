//! `bosun-mcp`, the stdio MCP server an agent session talks to (#7, #64).
//!
//! ```text
//! bosun-mcp --item <item id> [--session <session id>] [--endpoint <socket or pipe>]
//! ```
//!
//! The daemon writes it into the session's MCP configuration; the agent CLI starts it. The
//! session's credential comes in the variable `BOSUN_AGENT_CREDENTIAL`, never on the command
//! line. On start it connects to the daemon's endpoint (`--endpoint`, else
//! `bosun_client::Endpoint::resolve`) and calls `agent.hello` with the credential and the item:
//! that call is the handshake the daemon waits for after spawning the session, and a session
//! without it is killed. If the daemon refuses or cannot be reached, `bosun-mcp` says why on
//! stderr and exits with 1 before reading anything, so the agent CLI runs without the server
//! and the handshake's timeout catches it.
//!
//! Then it serves MCP over stdin and stdout ([`server`]): `tools/list` names `ask_question`,
//! `submit_summary` and `note`, and `tools/call` maps them to `agent.ask_question`,
//! `agent.submit_summary` and `agent.note`. Stdout carries MCP messages only. It exits when
//! stdin closes.
//!
//! A client of the local API (#4): it depends on `bosun-client` and `bosun-api` only and never
//! links `bosun-store` (#12).

mod server;

use std::process::ExitCode;

use bosun_api::method;
use bosun_api::{AgentAck, AgentHelloParams, AskQuestionParams, NoteParams, SubmitSummaryParams};
use bosun_client::{Client, Endpoint, Error};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::server::{Agent, Call, Server};

/// The variable that carries the session's credential (the daemon's `session::CREDENTIAL_VAR`).
const CREDENTIAL_VAR: &str = "BOSUN_AGENT_CREDENTIAL";

const USAGE: &str =
    "usage: bosun-mcp --item <item id> [--session <session id>] [--endpoint <socket or pipe>]";

/// The command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    item: String,
    session: Option<String>,
    endpoint: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut item = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--item" => item = Some(value()?),
            "--session" => parsed.session = Some(value()?),
            "--endpoint" => parsed.endpoint = Some(value()?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    parsed.item = item.ok_or("--item is required")?;
    Ok(parsed)
}

/// The daemon behind the tools: one connection, opened again after it was lost.
struct Daemon {
    endpoint: Endpoint,
    credential: String,
    item: String,
    client: Option<Client>,
}

impl Daemon {
    async fn client(&mut self) -> Result<&mut Client, Error> {
        if self.client.is_none() {
            self.client = Some(Client::connect(&self.endpoint).await?);
        }
        Ok(self.client.as_mut().expect("connected above"))
    }

    /// `agent.hello`: the handshake.
    async fn hello(&mut self) -> Result<bosun_api::AgentHelloResult, Error> {
        let params = AgentHelloParams {
            credential: self.credential.clone(),
            item: self.item.clone(),
        };
        self.client()
            .await?
            .call::<method::AgentHello>(&params)
            .await
    }
}

impl Agent for Daemon {
    async fn call(&mut self, call: Call) -> Result<AgentAck, String> {
        let (credential, item) = (self.credential.clone(), self.item.clone());
        let client = self.client().await.map_err(|e| e.to_string())?;
        let result = match call {
            Call::Question { text, blocking } => {
                client
                    .call::<method::AgentAskQuestion>(&AskQuestionParams {
                        credential,
                        item,
                        text,
                        blocking,
                    })
                    .await
            }
            Call::Summary {
                body,
                decisions,
                followups,
            } => {
                client
                    .call::<method::AgentSubmitSummary>(&SubmitSummaryParams {
                        credential,
                        item,
                        body,
                        decisions,
                        followups,
                    })
                    .await
            }
            Call::Note { text } => {
                client
                    .call::<method::AgentNote>(&NoteParams {
                        credential,
                        item,
                        text,
                    })
                    .await
            }
        };
        result.map_err(|e| {
            // A refusal leaves the connection usable; anything else ends it.
            if !matches!(e, Error::Rpc(_)) {
                self.client = None;
            }
            e.to_string()
        })
    }
}

async fn run(args: Args, credential: String) -> Result<(), String> {
    let endpoint = match args.endpoint {
        Some(endpoint) => Endpoint::new(endpoint),
        None => Endpoint::resolve()?,
    };
    let mut daemon = Daemon {
        endpoint,
        credential,
        item: args.item,
        client: None,
    };
    let bound = daemon
        .hello()
        .await
        .map_err(|e| format!("the handshake with the daemon failed: {e}"))?;
    eprintln!(
        "bosun-mcp: serving {} attempt {} (session {})",
        bound.item,
        bound.attempt,
        args.session.as_deref().unwrap_or(&bound.session)
    );
    let mut server = Server::new(daemon, env!("CARGO_PKG_VERSION"));
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = stdin.next_line().await.map_err(|e| format!("stdin: {e}"))? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = server.handle(&line).await {
            let mut text = answer.to_string();
            text.push('\n');
            stdout
                .write_all(text.as_bytes())
                .await
                .map_err(|e| format!("stdout: {e}"))?;
            stdout.flush().await.map_err(|e| format!("stdout: {e}"))?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&raw) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bosun-mcp: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(credential) = std::env::var(CREDENTIAL_VAR).ok().filter(|c| !c.is_empty()) else {
        eprintln!("bosun-mcp: {CREDENTIAL_VAR} is not set: run from a session the daemon started");
        return ExitCode::from(2);
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("bosun-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args, credential)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bosun-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_command_line_needs_an_item() {
        let parsed = parse(&args(&[
            "--item",
            "demo#1",
            "--session",
            "s",
            "--endpoint",
            "e",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Args {
                item: "demo#1".to_owned(),
                session: Some("s".to_owned()),
                endpoint: Some("e".to_owned()),
            }
        );
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["--item"])).is_err());
        assert!(parse(&args(&["--item", "x", "--credential", "c"])).is_err());
    }
}
