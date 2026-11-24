//! The `item.*` and `session.*` methods (#4, #70): one item's detail from the identity's
//! store, restart-item, and the sessions bosun spawned.
//!
//! `item.restart` is the user's own `restart` label (#5), put on the card for them: the loop
//! (#65) acts on it as on a label the user added, so the API decides nothing the board does
//! not. It takes the operator token, like the control switches, and writes nothing while the
//! kill switch stops board writes.

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    AgentCallView, AttemptView, ItemDetail, Method, RestartResult, SessionInfo, SnapshotView,
    TransitionView, VerdictView, method,
};
use bosun_core::{ItemId, SpawnMode};
use bosun_store::{AgentCallKind, RunRow, StoredItem};
use serde_json::Value;

use crate::api::{call, failed};
use crate::state::{Served, State};

/// The label `item.restart` puts on the card.
pub(crate) const RESTART_LABEL: &str = "restart";

/// The `item.*` and `session.*` methods.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::ItemGet::NAME => {
            call::<method::ItemGet, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                detail(served, &stored(served, &p.item).await?).await
            })
            .await
        }
        method::ItemRestart::NAME => {
            call::<method::ItemRestart, _, _>(p, |p| async move {
                state.control.authorize(&p.operator)?;
                let served = state.identity(&p.identity)?;
                restart(state, served, &p.item).await
            })
            .await
        }
        method::SessionGet::NAME => {
            call::<method::SessionGet, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                match (&p.session, &p.item) {
                    (Some(session), None) => by_session(served, session).await,
                    (None, Some(item)) => {
                        let item = stored(served, item).await?;
                        latest(served, &item.item.id).await
                    }
                    _ => Err(ErrorObject::new(
                        code::INVALID_PARAMS,
                        "session.get takes exactly one of `session` and `item`",
                    )),
                }
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

fn not_found(what: String) -> ErrorObject {
    ErrorObject::new(code::NOT_FOUND, what)
}

/// The item `id` of `served`'s identity, as its store holds it.
async fn stored(served: &Served, id: &str) -> Result<StoredItem, ErrorObject> {
    served
        .store
        .reader()
        .item(&ItemId::new(id))
        .await
        .map_err(failed)?
        .ok_or_else(|| {
            not_found(format!(
                "identity {} has no item {id:?} (an item id is `<repo>#<card>`)",
                served.name
            ))
        })
}

fn mode_name(mode: SpawnMode) -> &'static str {
    match mode {
        SpawnMode::Fresh => "fresh",
        SpawnMode::Resume => "resume",
        SpawnMode::Reset => "reset",
    }
}

fn session_info(item: &ItemId, run: &RunRow) -> SessionInfo {
    SessionInfo {
        item: item.to_string(),
        attempt: run.attempt,
        spawn: run.spawn,
        session: run.session_key.clone(),
        provider: run.provider.clone(),
        mode: mode_name(run.mode).to_owned(),
        started_ms: run.started_ms,
        ended_ms: run.ended_ms,
        exit: run
            .exit
            .map(|exit| crate::events::exit_name(Some(exit)).to_owned()),
        running: run.ended_ms.is_none(),
    }
}

async fn detail(served: &Served, item: &StoredItem) -> Result<ItemDetail, ErrorObject> {
    let reader = served.store.reader();
    let id = &item.item.id;
    let transitions = reader.transitions(id).await.map_err(failed)?;
    let attempts = reader.attempts(id).await.map_err(failed)?;
    let runs = reader.runs(id).await.map_err(failed)?;
    let verdicts = reader.verdict_runs(id).await.map_err(failed)?;
    let calls = reader.agent_calls(id).await.map_err(failed)?;
    let snapshot = reader.snapshot(id).await.map_err(failed)?;
    Ok(ItemDetail {
        identity: served.name.clone(),
        item: id.to_string(),
        repo: served.repo(&item.repo),
        state: item.item.state.as_str().to_owned(),
        detail: crate::events::reason_of(item.item.state),
        attempt: item.item.attempt.as_ref().map_or(0, |a| a.number),
        failed: item.item.failed,
        created_ms: item.created_ms,
        updated_ms: item.updated_ms,
        snapshot: snapshot.map(|s| SnapshotView {
            hash: s.hash,
            taken_ms: s.taken_ms,
            body: s.body,
            base_sha: s.base_sha,
            branch: s.branch,
        }),
        transitions: transitions
            .into_iter()
            .map(|t| TransitionView {
                seq: t.seq,
                attempt: t.attempt,
                from: t.from,
                to: t.to,
                detail: t.to_detail,
                attempt_from: t.attempt_from,
                attempt_to: t.attempt_to,
                cause: t.cause,
                ts_ms: t.ts_ms,
            })
            .collect(),
        attempts: attempts
            .into_iter()
            .map(|a| {
                let number = a.attempt.number;
                AttemptView {
                    number,
                    state: a.attempt.state.as_str().to_owned(),
                    started_ms: a.started_ms,
                    ended_ms: a.ended_ms,
                    snapshot_hash: a.snapshot_hash,
                    sessions: runs
                        .iter()
                        .filter(|r| r.attempt == number)
                        .map(|r| session_info(id, r))
                        .collect(),
                    verdicts: verdicts
                        .iter()
                        .filter(|v| v.attempt == number)
                        .map(|v| VerdictView {
                            run: v.run,
                            command: v.command.clone(),
                            started_ms: v.started_ms,
                            ended_ms: v.ended_ms,
                            result: v
                                .result
                                .map(|r| crate::events::run_result_name(r).to_owned()),
                            exit_code: v.exit_code,
                            verdict_line: v.verdict_line.clone(),
                        })
                        .collect(),
                }
            })
            .collect(),
        calls: calls
            .into_iter()
            .map(|c| AgentCallView {
                attempt: c.attempt,
                kind: match c.kind {
                    AgentCallKind::Summary { .. } => "summary",
                    AgentCallKind::Question { blocking: true } => "blocking_question",
                    AgentCallKind::Question { blocking: false } => "question",
                    AgentCallKind::Note => "note",
                }
                .to_owned(),
                body: c.body,
                refused: c.refused,
                ts_ms: c.ts_ms,
            })
            .collect(),
    })
}

/// The latest session of item `id`.
async fn latest(served: &Served, id: &ItemId) -> Result<SessionInfo, ErrorObject> {
    let runs = served.store.reader().runs(id).await.map_err(failed)?;
    runs.iter()
        .max_by_key(|r| (r.attempt, r.spawn))
        .map(|r| session_info(id, r))
        .ok_or_else(|| not_found(format!("item {id} has had no session yet")))
}

/// The latest spawn of provider session `session` in `served`'s identity.
async fn by_session(served: &Served, session: &str) -> Result<SessionInfo, ErrorObject> {
    let reader = served.store.reader();
    let mut found: Option<SessionInfo> = None;
    for item in reader.items(false).await.map_err(failed)? {
        for run in reader.runs(&item.item.id).await.map_err(failed)? {
            if run.session_key == session {
                let info = session_info(&item.item.id, &run);
                if found
                    .as_ref()
                    .is_none_or(|f| (f.attempt, f.spawn) < (info.attempt, info.spawn))
                {
                    found = Some(info);
                }
            }
        }
    }
    found.ok_or_else(|| {
        not_found(format!(
            "identity {} has no session {session:?}",
            served.name
        ))
    })
}

/// Puts the `restart` label on the card of item `id`, which must be in flight.
async fn restart(state: &State, served: &Served, id: &str) -> Result<RestartResult, ErrorObject> {
    let item = stored(served, id).await?;
    if !item.item.state.is_active() {
        return Err(ErrorObject::new(
            code::FAILED,
            format!(
                "item {id} is {}, not in flight: only an item in flight restarts (move a card in \
                 Needs you back to Ready to retry it)",
                item.item.state.as_str()
            ),
        ));
    }
    if state.control.board_writes_stopped() {
        return Err(ErrorObject::new(
            code::FAILED,
            "the kill switch is thrown: nothing is written to any board until it is released",
        ));
    }
    let (repo_name, card) = id
        .rsplit_once('#')
        .and_then(|(repo, n)| Some((repo, n.parse::<u64>().ok()?)))
        .ok_or_else(|| ErrorObject::new(code::INVALID_PARAMS, format!("{id:?} is no item id")))?;
    let (name, _) = crate::board::repo_named(state, served, repo_name)?;
    let board = crate::board::board_of(served, name)
        .map_err(|e| ErrorObject::new(code::FAILED, format!("repository {repo_name}: {e}")))?;
    tokio::task::spawn_blocking(move || board.add_label(bosun_queue::ItemId(card), RESTART_LABEL))
        .await
        .map_err(|e| ErrorObject::new(code::INTERNAL_ERROR, e.to_string()))?
        .map_err(failed)?;
    eprintln!("bosund: {id}: restart asked for; the `{RESTART_LABEL}` label is on its card");
    Ok(RestartResult {
        item: id.to_owned(),
        label: RESTART_LABEL.to_owned(),
    })
}
