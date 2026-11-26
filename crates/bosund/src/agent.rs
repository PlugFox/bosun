//! The agent's side of the API (#7, #63): the per-session credential and the `agent.*` methods.
//!
//! # The credential
//!
//! The executor issues one credential per session, at spawn, bound to the session's identity,
//! item, attempt and session id ([`Credentials::issue`]); it reaches the session through the
//! per-session MCP configuration (#64 writes the server's side). It is 32 bytes from the OS's
//! random source. Only its SHA-256 digest is kept, in the daemon's memory; the text never. A
//! daemon restart kills the session in stage 1 (#7), so nothing needs to survive one there. In
//! worktree mode (#8, E10) a session goes on under its shim across a restart: the digest is
//! also on its `process` row, and the restarted daemon admits it again with the session's
//! binding ([`Credentials::readmit`]), with the per-tool counts its attempt made before the
//! restart read back from the store ([`Credentials::restore_calls`]). The [`Issued`] handle
//! revokes it when dropped, which the session does as soon as its process is gone, on every
//! path.
//!
//! Between the restarted daemon's start and that re-admission the session's `bosun-mcp` may
//! call: every loop holds a [`Recovering`] guard from its creation until its recovery has
//! ended (and re-admitted the session it re-attached), and while any guard is held an unknown
//! credential is refused with `RECOVERING`, which `bosun-mcp` retries, instead of
//! `UNAUTHORIZED`, which it does not. A live credential naming another item is `UNAUTHORIZED`
//! whatever the guards.
//!
//! Limitation: the guard is not bounded in time. A loop whose recovery never ends (its board
//! stays unreachable, so `recover` keeps failing) holds its guard until the daemon stops, and
//! meanwhile every unknown credential daemon-wide, of any repository, is `RECOVERING`: a
//! `bosun-mcp` holding a credential that is gone for good (its session's item was settled
//! before the restart) retries each call for its full 60 s before it fails, instead of failing
//! at once. Nothing is admitted that should not be; only that failure is slow.
//!
//! A request carrying a credential may call `agent.*` only, and `agent.*` admits nothing but a
//! live credential naming its own item ([`crate::api`] refuses the rest).
//!
//! # The calls
//!
//! Every call of a tool is a row of `agent_call`, bound to the credential's attempt and to the
//! commit HEAD resolves to at the time of the call (read through `bosun-git`'s hooks-disabled
//! invoker). Each tool has a per-attempt cap ([`cap`]), counted across the attempt's sessions:
//! a call over it is refused, and still recorded, as refused. A blocking question, once
//! recorded, is handed to the repository's loop, which records the transition to Needs you and
//! ends the session after its grace period. A summary is recorded and nothing else: whatever it
//! says, it moves nothing.
//!
//! `bosun-mcp` retries a call whose answer it did not get (#8), so a call may arrive twice. A
//! blocking question the store already holds for the attempt, with the same text, is
//! acknowledged without a second row; and the loop is handed a blocking question at most once
//! per item and attempt in this daemon (a deduplicated one included, when this daemon has not
//! handed it yet: after a restart the previous daemon's transition was not recorded). Notes and
//! summaries are not deduplicated: a retried one may be a second row (of a summary, the last
//! call wins anyway).

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    API_VERSION, AgentAck, AgentHelloResult, AskQuestionParams, Method, NoteParams,
    SubmitSummaryParams, method,
};
use bosun_core::ItemId;
use bosun_store::{AgentCall, AgentCallKind, Reader, Write, Writer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::call;

/// The prefix of every credential, so one is recognisable in a configuration file.
const PREFIX: &str = "bosun-agent-";

/// A tool of the agent, for its rate cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Tool {
    Question,
    Summary,
    Note,
}

/// The most calls of `tool` one attempt may make. A blocking question ends the session, so the
/// question cap mostly bounds non-blocking ones; the summary may be revised as work goes on.
pub(crate) const fn cap(tool: Tool) -> u32 {
    match tool {
        Tool::Question => 5,
        Tool::Summary => 20,
        Tool::Note => 30,
    }
}

/// What a credential is bound to.
pub(crate) struct Binding {
    pub(crate) identity: String,
    pub(crate) item: ItemId,
    pub(crate) attempt: u32,
    pub(crate) session: String,
    /// The repository's checkout, for HEAD at the time of a call.
    pub(crate) repo: PathBuf,
    /// The identity's store.
    pub(crate) writer: Writer,
    /// The identity's store, read: a retried blocking question is found there.
    pub(crate) reader: Reader,
    /// Hands a recorded blocking question to the repository's loop.
    pub(crate) on_blocking: Box<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding")
            .field("identity", &self.identity)
            .field("item", &self.item)
            .field("attempt", &self.attempt)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A live credential: its binding, and whether its server said hello.
#[derive(Debug)]
struct Live {
    binding: Arc<Binding>,
    hello: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Live credentials by the digest of their text.
    live: HashMap<[u8; 32], Live>,
    /// Calls per item, attempt and tool, across the attempt's sessions.
    calls: HashMap<(ItemId, u32, Tool), u32>,
    /// Loops whose recovery has not ended yet ([`Recovering`]).
    recovering: usize,
    /// The items and attempts whose blocking question was handed to the loop.
    fired: HashSet<(ItemId, u32)>,
}

/// The daemon's live agent credentials; cloning shares them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Credentials {
    inner: Arc<Mutex<Inner>>,
}

/// An issued credential. Dropping it revokes the credential.
#[derive(Debug)]
pub(crate) struct Issued {
    token: String,
    digest: [u8; 32],
    hello: Arc<AtomicBool>,
    credentials: Credentials,
}

impl Issued {
    /// The credential's text, for the session's MCP configuration.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// The SHA-256 digest of the credential's text: what the `process` row keeps of it (#8);
    /// the text itself is never stored.
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Whether the session's `bosun-mcp` has made its `agent.hello` (the handshake of #64).
    #[allow(dead_code)] // The handshake timeout is #64's.
    pub(crate) fn said_hello(&self) -> bool {
        self.hello.load(Ordering::SeqCst)
    }
}

impl Drop for Issued {
    fn drop(&mut self) {
        self.credentials.lock().live.remove(&self.digest);
    }
}

/// A loop that has not ended its recovery yet: while one is held, an unknown credential is
/// refused as retryable (`RECOVERING`). Dropping it lowers the count.
#[derive(Debug)]
pub(crate) struct Recovering {
    credentials: Credentials,
}

impl Drop for Recovering {
    fn drop(&mut self) {
        let mut inner = self.credentials.lock();
        inner.recovering = inner.recovering.saturating_sub(1);
    }
}

/// The tool a stored call is a call of.
pub(crate) const fn tool_of(kind: &AgentCallKind) -> Tool {
    match kind {
        AgentCallKind::Question { .. } => Tool::Question,
        AgentCallKind::Summary { .. } => Tool::Summary,
        AgentCallKind::Note => Tool::Note,
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn unauthorized(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code::UNAUTHORIZED, message)
}

impl Credentials {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves nothing half-done in these maps.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Issues a new credential bound to `binding`.
    ///
    /// # Errors
    ///
    /// The OS's random source failed.
    pub(crate) fn issue(&self, binding: Binding) -> Result<Issued, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| format!("the random source: {e}"))?;
        let mut token = String::from(PREFIX);
        for b in bytes {
            let _ = write!(token, "{b:02x}");
        }
        let digest = digest(&token);
        let hello = Arc::new(AtomicBool::new(false));
        self.lock().live.insert(
            digest,
            Live {
                binding: Arc::new(binding),
                hello: hello.clone(),
            },
        );
        Ok(Issued {
            token,
            digest,
            hello,
            credentials: self.clone(),
        })
    }

    /// Admits again the credential whose digest is `digest`, bound to `binding`: the
    /// credential of a session a previous daemon issued and that went on under its shim (#8,
    /// E10). Its `bosun-mcp` said hello to that daemon, so it counts as having said it. The text
    /// is not known here (only its digest was stored): [`Issued::token`] is empty.
    pub(crate) fn readmit(&self, digest: [u8; 32], binding: Binding) -> Issued {
        let hello = Arc::new(AtomicBool::new(true));
        self.lock().live.insert(
            digest,
            Live {
                binding: Arc::new(binding),
                hello: hello.clone(),
            },
        );
        Issued {
            token: String::new(),
            digest,
            hello,
            credentials: self.clone(),
        }
    }

    /// Raises the count of loops still recovering until the guard is dropped.
    pub(crate) fn recovering(&self) -> Recovering {
        self.lock().recovering += 1;
        Recovering {
            credentials: self.clone(),
        }
    }

    /// Sets the call counts of `item`'s `attempt` to the larger of `counts` (what the store
    /// holds, refused calls included) and what this daemon counted: the caps of a re-attached
    /// session go on from where they were before the restart (#8).
    pub(crate) fn restore_calls(
        &self,
        item: &ItemId,
        attempt: u32,
        counts: impl IntoIterator<Item = (Tool, u32)>,
    ) {
        let mut inner = self.lock();
        for (tool, stored) in counts {
            let n = inner
                .calls
                .entry((item.clone(), attempt, tool))
                .or_insert(0);
            *n = (*n).max(stored);
        }
    }

    /// Whether the blocking question of `item`'s `attempt` is to be handed to the loop: `true`
    /// the first time only.
    fn fire_once(&self, item: &ItemId, attempt: u32) -> bool {
        // Mutant (`mutant_agent_blocking_question_twice`): every blocking question fires.
        cfg!(mutant_agent_blocking_question_twice)
            || self.lock().fired.insert((item.clone(), attempt))
    }

    /// The binding of a live `credential` that names `item`; refused otherwise.
    fn admit(
        &self,
        credential: &str,
        item: &str,
    ) -> Result<(Arc<Binding>, Arc<AtomicBool>), ErrorObject> {
        let inner = self.lock();
        let Some(live) = inner.live.get(&digest(credential)) else {
            // Mutant (`mutant_agent_recovering_unauthorized`): the loops still recovering are
            // not looked at.
            if inner.recovering > 0 && !cfg!(mutant_agent_recovering_unauthorized) {
                return Err(ErrorObject::new(
                    code::RECOVERING,
                    "the daemon is recovering its sessions and has not admitted this \
                     credential yet: retry the call",
                ));
            }
            return Err(unauthorized(
                "the agent credential is not live (unknown or revoked)",
            ));
        };
        // Mutant `mutant_agent_any_item`: the item the call names is not checked.
        if !cfg!(mutant_agent_any_item) && live.binding.item.0 != item {
            return Err(unauthorized(format!(
                "the agent credential is bound to another item than {item:?}"
            )));
        }
        Ok((live.binding.clone(), live.hello.clone()))
    }

    /// Counts a call of `tool` by `binding`'s attempt; `false` when it is over the cap.
    fn count(&self, binding: &Binding, tool: Tool) -> bool {
        let mut inner = self.lock();
        let n = inner
            .calls
            .entry((binding.item.clone(), binding.attempt, tool))
            .or_insert(0);
        *n = n.saturating_add(1);
        // Mutant `mutant_agent_no_rate_cap`: no call is over its cap.
        cfg!(mutant_agent_no_rate_cap) || *n <= cap(tool)
    }

    /// Whether a credential bound to `item` is live (tests wait for the spawn with it).
    #[cfg(test)]
    pub(crate) fn live_for(&self, item: &ItemId) -> bool {
        self.lock().live.values().any(|l| l.binding.item == *item)
    }
}

/// The commit HEAD of `repo` resolves to, through the hooks-disabled invoker.
async fn head(repo: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        bosun_git::Git::new(repo)
            .run(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], None)
            .ok()
            .map(|out| out.trim().to_owned())
            .filter(|sha| !sha.is_empty())
    })
    .await
    .ok()
    .flatten()
}

fn now_ms() -> i64 {
    crate::executor::now_ms()
}

/// Records one call of `tool`: a row, refused when over the cap.
async fn record(
    credentials: &Credentials,
    binding: &Binding,
    tool: Tool,
    kind: AgentCallKind,
    body: String,
) -> Result<AgentAck, ErrorObject> {
    let within = credentials.count(binding, tool);
    let head_sha = head(binding.repo.clone()).await;
    let row = Write::AgentCall(AgentCall {
        item: binding.item.clone(),
        attempt: binding.attempt,
        kind,
        body,
        head_sha: head_sha.clone(),
        refused: !within,
        at_ms: now_ms(),
    });
    binding
        .writer
        .write(vec![row])
        .await
        .map_err(|e| ErrorObject::new(code::FAILED, format!("the call is not recorded: {e}")))?;
    if !within {
        return Err(ErrorObject::new(
            code::RATE_CAPPED,
            format!(
                "over the cap of {} calls per attempt; the call is recorded as refused",
                cap(tool)
            ),
        ));
    }
    Ok(AgentAck {
        attempt: binding.attempt,
        head_sha,
    })
}

/// The acknowledgement of a blocking question with `text` that the store already holds for
/// `binding`'s attempt, not refused: a retry of a call whose answer was lost (#8). `None` when
/// there is none, or when the store cannot be read (the question is recorded again).
async fn asked_already(binding: &Binding, text: &str) -> Option<AgentAck> {
    // Mutant (`mutant_agent_blocking_question_twice`): no repeated question is looked for.
    if cfg!(mutant_agent_blocking_question_twice) {
        return None;
    }
    let rows = binding.reader.agent_calls(&binding.item).await.ok()?;
    rows.into_iter()
        .find(|row| {
            row.attempt == binding.attempt
                && row.kind == (AgentCallKind::Question { blocking: true })
                && !row.refused
                && row.body == text
        })
        .map(|row| AgentAck {
            attempt: binding.attempt,
            head_sha: row.head_sha,
        })
}

/// The credential and item of `agent.*` params, before they are parsed in full.
fn credential_of(p: &Value) -> Result<(&str, &str), ErrorObject> {
    let field = |name| {
        p.get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| unauthorized(format!("an agent.* call carries its `{name}`")))
    };
    Ok((field("credential")?, field("item")?))
}

/// Answers one `agent.*` method.
pub(crate) async fn dispatch(
    credentials: &Credentials,
    name: &str,
    p: Value,
) -> Result<Value, ErrorObject> {
    let (binding, hello) = {
        let (credential, item) = credential_of(&p)?;
        credentials.admit(credential, item)?
    };
    match name {
        method::AgentHello::NAME => {
            call::<method::AgentHello, _, _>(p, |_| async {
                hello.store(true, Ordering::SeqCst);
                Ok(AgentHelloResult {
                    api_version: API_VERSION,
                    identity: binding.identity.clone(),
                    item: binding.item.to_string(),
                    attempt: binding.attempt,
                    session: binding.session.clone(),
                })
            })
            .await
        }
        method::AgentAskQuestion::NAME => {
            call::<method::AgentAskQuestion, _, _>(p, |p: AskQuestionParams| async move {
                let blocking = p.blocking;
                let repeated = if blocking {
                    asked_already(&binding, &p.text).await
                } else {
                    None
                };
                let ack = match repeated {
                    Some(ack) => ack,
                    None => {
                        record(
                            credentials,
                            &binding,
                            Tool::Question,
                            AgentCallKind::Question { blocking },
                            p.text,
                        )
                        .await?
                    }
                };
                if blocking && credentials.fire_once(&binding.item, binding.attempt) {
                    (binding.on_blocking)();
                }
                Ok(ack)
            })
            .await
        }
        method::AgentSubmitSummary::NAME => {
            call::<method::AgentSubmitSummary, _, _>(p, |p: SubmitSummaryParams| async move {
                let kind = AgentCallKind::Summary {
                    decisions: p.decisions,
                    followups: p.followups,
                };
                record(credentials, &binding, Tool::Summary, kind, p.body).await
            })
            .await
        }
        method::AgentNote::NAME => {
            call::<method::AgentNote, _, _>(p, |p: NoteParams| async move {
                record(
                    credentials,
                    &binding,
                    Tool::Note,
                    AgentCallKind::Note,
                    p.text,
                )
                .await
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn note(credentials: &Credentials) -> i32 {
        dispatch(
            credentials,
            method::AgentNote::NAME,
            json!({"credential": "bosun-agent-00", "item": "demo#1", "text": "x"}),
        )
        .await
        .unwrap_err()
        .code
    }

    #[tokio::test]
    async fn an_unknown_credential_is_retryable_only_while_a_loop_recovers() {
        let credentials = Credentials::default();
        assert_eq!(note(&credentials).await, code::UNAUTHORIZED);
        let first = credentials.recovering();
        assert_eq!(note(&credentials).await, code::RECOVERING);
        let second = credentials.recovering();
        drop(first);
        assert_eq!(note(&credentials).await, code::RECOVERING);
        drop(second);
        assert_eq!(note(&credentials).await, code::UNAUTHORIZED);
    }
}
