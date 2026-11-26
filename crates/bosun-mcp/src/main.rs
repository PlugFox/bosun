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
//! A session in worktree mode goes on under its shim while the daemon restarts (#8, E3, E22),
//! so a tool call may meet no daemon, a connection that drops before the answer, or a daemon
//! that has not re-admitted the session's credential yet (`RECOVERING`). Such a call is made
//! again, reconnecting first when the connection is gone, with waits doubling from 100 ms up to
//! 2 s, for up to 60 s from its first try ([`Retry`]); then it is a tool error that tells the
//! model to treat the call as not recorded and to call the tool again. Every other refusal
//! (`UNAUTHORIZED`: a revoked credential, another item's; a rate cap; bad params) and every
//! other error is returned at once. The startup handshake is not retried.
//!
//! A client of the local API (#4): it depends on `bosun-client` and `bosun-api` only and never
//! links `bosun-store` (#12).

mod server;

use std::process::ExitCode;
use std::time::Duration;

use bosun_api::method;
use bosun_api::rpc::code;
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

/// How a connection to the daemon is opened: its endpoint, or a fake in the tests.
trait Connect {
    async fn connect(&self) -> Result<Client, Error>;
}

impl Connect for Endpoint {
    async fn connect(&self) -> Result<Client, Error> {
        Client::connect(self).await
    }
}

/// How long a tool call is made again while the daemon cannot confirm it (#8, E10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Retry {
    /// From the first try; no try starts after it.
    window: Duration,
    /// The first wait between tries; each next one doubles, up to `max`.
    first: Duration,
    max: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            first: Duration::from_millis(100),
            max: Duration::from_secs(2),
        }
    }
}

/// A duration as the tool error says it: whole seconds, else milliseconds.
fn spoken(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Whether `e` may go away when the call is made again: the daemon is gone or restarting
/// (a transport error), or has not re-admitted the credential yet (`RECOVERING`).
fn retryable(e: &Error) -> bool {
    match e {
        Error::NotRunning { .. } | Error::Closed | Error::Timeout | Error::Io(_) => true,
        // Mutant (`mutant_mcp_retry_unauthorized`): a refused credential is retried too.
        Error::Rpc(e) => {
            e.code == code::RECOVERING
                || (cfg!(mutant_mcp_retry_unauthorized) && e.code == code::UNAUTHORIZED)
        }
        _ => false,
    }
}

/// The daemon behind the tools: one connection, opened again after it was lost.
struct Daemon<C> {
    connect: C,
    credential: String,
    item: String,
    client: Option<Client>,
    retry: Retry,
}

impl<C: Connect> Daemon<C> {
    fn new(connect: C, credential: String, item: String, retry: Retry) -> Self {
        Self {
            connect,
            credential,
            item,
            client: None,
            retry,
        }
    }

    async fn client(&mut self) -> Result<&mut Client, Error> {
        if self.client.is_none() {
            self.client = Some(self.connect.connect().await?);
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

impl<C: Connect> Agent for Daemon<C> {
    async fn call(&mut self, call: Call) -> Result<AgentAck, String> {
        let started = tokio::time::Instant::now();
        let mut wait = self.retry.first;
        loop {
            let e = match self.try_call(call.clone()).await {
                Ok(ack) => return Ok(ack),
                Err(e) => e,
            };
            // Mutant (`mutant_mcp_no_retry`): the first error is the call's.
            if cfg!(mutant_mcp_no_retry) || !retryable(&e) {
                return Err(e.to_string());
            }
            let left = self.retry.window.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(format!(
                    "the daemon did not confirm the call within {} (it may be restarting): \
                     treat it as not recorded and call the tool again (last: {e})",
                    spoken(self.retry.window)
                ));
            }
            tokio::time::sleep(wait.min(left)).await;
            wait = wait.saturating_mul(2).min(self.retry.max);
        }
    }
}

impl<C: Connect> Daemon<C> {
    /// One try of `call` on the connection, opened first when there is none.
    async fn try_call(&mut self, call: Call) -> Result<AgentAck, Error> {
        let (credential, item) = (self.credential.clone(), self.item.clone());
        let client = self.client().await?;
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
            e
        })
    }
}

async fn run(args: Args, credential: String) -> Result<(), String> {
    let endpoint = match args.endpoint {
        Some(endpoint) => Endpoint::new(endpoint),
        None => Endpoint::resolve()?,
    };
    let mut daemon = Daemon::new(endpoint, credential, args.item, Retry::default());
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use bosun_api::rpc::{ErrorObject, Request, Response};
    use bosun_api::{API_VERSION, HelloResult};
    use bosun_client::framing;
    use serde_json::{Value, json};

    use super::*;

    /// What the fake daemon does with one agent call.
    #[derive(Debug, Clone, Copy)]
    enum Answer {
        /// Closes the connection without an answer: the daemon stopped.
        Drop,
        /// Refuses it with this code.
        Refuse(i32),
        /// Acknowledges it for attempt 1.
        Ack,
    }

    /// What one `connect` meets.
    #[derive(Debug, Clone)]
    enum Conn {
        /// Nothing listens.
        Down,
        /// A daemon that answers `hello`, then the agent calls in turn (acknowledging those
        /// past the list).
        Up(Vec<Answer>),
    }

    /// A fake daemon: each `connect` takes the next step of the script (`Down` once it has
    /// run out) and counts the agent calls it was sent.
    #[derive(Debug, Clone, Default)]
    struct Fake {
        script: Arc<Mutex<VecDeque<Conn>>>,
        calls: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(script: impl IntoIterator<Item = Conn>) -> Self {
            Self {
                script: Arc::new(Mutex::new(script.into_iter().collect())),
                calls: Arc::default(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    async fn serve(stream: tokio::io::DuplexStream, answers: Vec<Answer>, calls: Arc<AtomicUsize>) {
        let (read, mut write) = tokio::io::split(stream);
        let mut read = tokio::io::BufReader::new(read);
        let mut answers = answers.into_iter();
        while let Ok(Some(line)) = framing::read_line(&mut read).await {
            let request: Request = serde_json::from_str(&line).unwrap();
            let id = request.id.unwrap_or(Value::Null);
            let response = if request.method == "hello" {
                let hello = HelloResult {
                    api_version: API_VERSION,
                    daemon_version: "0.0.0".to_owned(),
                    pid: 1,
                };
                Response::result(id, serde_json::to_value(hello).unwrap())
            } else {
                calls.fetch_add(1, Ordering::SeqCst);
                match answers.next().unwrap_or(Answer::Ack) {
                    Answer::Drop => return,
                    Answer::Refuse(code) => {
                        Response::error(id, ErrorObject::new(code, "refused by the fake"))
                    }
                    Answer::Ack => Response::result(id, json!({"attempt": 1, "head_sha": null})),
                }
            };
            if framing::write_message(&mut write, &response).await.is_err() {
                return;
            }
            let _ = write.flush().await;
        }
    }

    impl Connect for Fake {
        async fn connect(&self) -> Result<Client, Error> {
            let step = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Conn::Down);
            let Conn::Up(answers) = step else {
                return Err(Error::NotRunning {
                    endpoint: "fake".to_owned(),
                });
            };
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            tokio::spawn(serve(theirs, answers, self.calls.clone()));
            Client::over(Box::new(ours)).await
        }
    }

    fn retry(window_ms: u64, first_ms: u64) -> Retry {
        Retry {
            window: Duration::from_millis(window_ms),
            first: Duration::from_millis(first_ms),
            max: Duration::from_secs(2),
        }
    }

    /// A `note` tool call through the MCP server, and its result.
    async fn note(fake: &Fake, retry: Retry) -> Value {
        let daemon = Daemon::new(
            fake.clone(),
            "bosun-agent-00".to_owned(),
            "demo#1".to_owned(),
            retry,
        );
        let mut server = Server::new(daemon, "0");
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "note", "arguments": {"text": "x"}}});
        let answer = server.handle(&line.to_string()).await.unwrap();
        answer["result"].clone()
    }

    fn text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap()
    }

    #[tokio::test]
    async fn a_call_during_a_daemon_restart_is_retried_until_the_daemon_answers() {
        let fake = Fake::new([
            Conn::Up(vec![Answer::Drop]),
            Conn::Down,
            Conn::Down,
            Conn::Up(vec![Answer::Refuse(code::RECOVERING), Answer::Ack]),
        ]);
        let result = note(&fake, retry(5_000, 10)).await;
        assert_eq!(result["isError"], false, "{result}");
        assert!(text(&result).contains("attempt 1"), "{result}");
        assert_eq!(fake.calls(), 3);
    }

    #[tokio::test]
    async fn an_unauthorized_call_is_returned_at_once_and_not_retried() {
        let fake = Fake::new([Conn::Up(vec![Answer::Refuse(code::UNAUTHORIZED)])]);
        let started = std::time::Instant::now();
        let result = note(&fake, retry(5_000, 200)).await;
        let took = started.elapsed();
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(fake.calls(), 1);
        assert!(took < Duration::from_millis(300), "took {took:?}");
    }

    #[tokio::test]
    async fn a_daemon_gone_past_the_retry_window_is_a_tool_error_that_says_to_call_again() {
        let fake = Fake::new([]);
        let started = std::time::Instant::now();
        let result = note(&fake, retry(300, 10)).await;
        let took = started.elapsed();
        assert_eq!(result["isError"], true, "{result}");
        let said = text(&result);
        assert!(said.contains("treat it as not recorded"), "{said}");
        assert!(said.contains("call the tool again"), "{said}");
        assert!(said.contains("within 300 ms"), "{said}");
        assert!(took >= Duration::from_millis(300), "took {took:?}");
        assert!(took < Duration::from_millis(1_300), "took {took:?}");
        assert_eq!(fake.calls(), 0);
    }

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
