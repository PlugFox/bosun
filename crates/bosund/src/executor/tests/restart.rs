//! `bosun-mcp` across a daemon restart (#8, E10): what the restarted daemon answers to a call
//! made before its loop has re-admitted the session's credential, the caps an attempt keeps
//! across the restart, and a blocking question a retry sends twice.
//!
//! The re-attach scenes are those of [`super::recovery`]: a child daemon is killed while the
//! session waits under its shim, and the test process is the restarted daemon.

use std::sync::atomic::AtomicBool;

use bosun_api::rpc::code;
use bosun_queue::{Changes, Cursor, Owned, QueueError};
use bosun_store::{AgentCall, AgentCallKind};

use super::recovery::{
    api_of, causes, child_log, end_agent, kill_under_shim, reattached, shimmed_session,
};
use super::*;
use crate::agent::{Tool, cap};

/// The scene's board, whose polls fail while `down` is set.
struct Unreadable {
    inner: board::Shared,
    down: AtomicBool,
    /// The polls made, failed ones included.
    polls: AtomicUsize,
}

impl Queue for Unreadable {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(QueueError::Remote("the board is unreachable".to_owned()));
        }
        self.inner.poll(cursor)
    }

    fn project(
        &self,
        item: bosun_queue::ItemId,
        state: Owned,
        note: Option<&str>,
    ) -> Result<(), QueueError> {
        self.inner.project(item, state, note)
    }

    fn comment(&self, item: bosun_queue::ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        self.inner.comment(item, body, key)
    }

    fn upsert_comment(
        &self,
        item: bosun_queue::ItemId,
        key: &str,
        body: &str,
    ) -> Result<(), QueueError> {
        self.inner.upsert_comment(item, key, body)
    }

    fn add_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.inner.add_label(item, label)
    }

    fn remove_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.inner.remove_label(item, label)
    }

    fn lint_target(&self, item: bosun_queue::ItemId) -> Result<Card, QueueError> {
        self.inner.lint_target(item)
    }
}

/// `agent.note` with `credential` for `demo#1`, on `conn`.
async fn note(
    api: &crate::state::State,
    conn: &mut crate::api::Connection,
    credential: &str,
) -> serde_json::Value {
    rpc(
        api,
        conn,
        "agent.note",
        serde_json::json!({"credential": credential, "item": "demo#1", "text": "after the restart"}),
    )
    .await
}

/// A credential no daemon issued.
const UNKNOWN: &str = "bosun-agent-00";

/// #8 (E10): until the restarted daemon's loop has re-admitted the session it re-attaches, an
/// unknown credential (the session's own, and one never issued) is refused as retryable
/// (`RECOVERING`); once it has, the session's is acknowledged and an unknown one is
/// `UNAUTHORIZED` again (`mutant_agent_recovering_never_ends` keeps it `RECOVERING`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_credential_is_retryable_until_the_loop_has_re_attached_its_session() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    let credential = credential_of_session(&scene.path("work"));

    let queue = Arc::new(Unreadable {
        inner: scene.queue(),
        down: AtomicBool::new(true),
        polls: AtomicUsize::new(0),
    });
    let (task, watcher) = scene.start_with(None, queue.clone());
    wait_for("a few failed polls", Duration::from_secs(30), async || {
        queue.polls.load(Ordering::SeqCst) >= 3
    })
    .await;
    let api = api_of(&scene);
    let mut conn = crate::api::Connection::default();
    let own_early = note(&api, &mut conn, &credential).await;
    let unknown_early = note(&api, &mut conn, UNKNOWN).await;

    queue.down.store(false, Ordering::SeqCst);
    reattached(&scene).await;
    let mut own_late = serde_json::Value::Null;
    wait_for(
        "the session's credential to be admitted",
        Duration::from_secs(30),
        async || {
            own_late = note(&api, &mut conn, &credential).await;
            error_code(&own_late) != Some(code::RECOVERING.into())
        },
    )
    .await;
    let mut unknown_late = serde_json::Value::Null;
    let mut conn_late = crate::api::Connection::default();
    let mut tries = 0;
    while tries < 100 {
        unknown_late = note(&api, &mut conn_late, UNKNOWN).await;
        if error_code(&unknown_late) != Some(code::RECOVERING.into()) {
            break;
        }
        tries += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    let log = child_log(&scene);

    assert_eq!(
        error_code(&own_early),
        Some(code::RECOVERING.into()),
        "{own_early}"
    );
    assert_eq!(
        error_code(&unknown_early),
        Some(code::RECOVERING.into()),
        "{unknown_early}"
    );
    assert!(
        own_late["result"]["attempt"] == 1,
        "the session's credential after the re-attach: {own_late}"
    );
    assert_eq!(
        error_code(&unknown_late),
        Some(code::UNAUTHORIZED.into()),
        "an unknown credential after the re-attach: {unknown_late}"
    );
    assert_eq!(state, State::Done(Outcome::Pass), "{log}");
}

/// #8: the calls an attempt made before the restart count against its caps after it. The
/// store holds a full cap of notes for attempt 1, as the previous daemon recorded them; the
/// re-attached session's next note is over the cap, refused and recorded as refused
/// (`mutant_agent_caps_reset_on_readmit` starts the count from zero and accepts it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattached_session_keeps_the_calls_its_attempt_made_before_the_restart() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    let credential = credential_of_session(&scene.path("work"));
    let item = ItemId::new("demo#1");
    let rows: Vec<Write> = (0..cap(Tool::Note))
        .map(|n| {
            Write::AgentCall(AgentCall {
                item: item.clone(),
                attempt: 1,
                kind: AgentCallKind::Note,
                body: format!("before the restart {n}"),
                head_sha: None,
                refused: false,
                at_ms: now_ms(),
            })
        })
        .collect();
    scene.store.writer().write(rows).await.unwrap();

    let (task, watcher) = scene.start(None);
    reattached(&scene).await;
    let api = api_of(&scene);
    let mut conn = crate::api::Connection::default();
    let mut capped = serde_json::Value::Null;
    wait_for(
        "the session's credential to be admitted",
        Duration::from_secs(30),
        async || {
            capped = note(&api, &mut conn, &credential).await;
            error_code(&capped) != Some(code::RECOVERING.into())
        },
    )
    .await;
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    let log = child_log(&scene);

    assert_eq!(
        error_code(&capped),
        Some(code::RATE_CAPPED.into()),
        "the note over the cap: {capped}"
    );
    let calls = scene.store.reader().agent_calls(&item).await.unwrap();
    let notes: Vec<_> = calls
        .iter()
        .filter(|c| c.attempt == 1 && c.kind == AgentCallKind::Note)
        .collect();
    assert_eq!(notes.len(), cap(Tool::Note) as usize + 1, "{notes:?}");
    let refused: Vec<_> = notes.iter().filter(|c| c.refused).collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].body, "after the restart");
    assert_eq!(state, State::Done(Outcome::Pass), "{log}");
}

/// #8: `bosun-mcp` sends a blocking question again when its answer was lost; the daemon
/// records it once and moves the item once. Both calls are acknowledged for attempt 1; the
/// store holds one blocking question; the item is Needs you with one transition the question
/// caused, and the card one Needs-you comment (`mutant_agent_blocking_question_twice` records
/// two).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_blocking_question_is_recorded_once() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Asks", "test -f never.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
        ],
    );
    let (task, watcher) = scene.start(None);
    let item = ItemId::new("demo#1");
    wait_file(&scene.path("flag1")).await;
    let credential = credential_of_session(&scene.path("work"));
    let api = api_of(&scene);
    let mut conn = crate::api::Connection::default();
    let ask = serde_json::json!({
        "credential": credential, "item": "demo#1", "text": "which way?", "blocking": true
    });
    let first = rpc(&api, &mut conn, "agent.ask_question", ask.clone()).await;
    let retried = rpc(&api, &mut conn, "agent.ask_question", ask).await;
    wait_for("needs you", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Question))
    })
    .await;
    drained(&scene).await;
    // Give a second transition time to happen.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();

    assert_eq!(first["result"]["attempt"], 1, "{first}");
    assert_eq!(retried["result"]["attempt"], 1, "{retried}");
    let calls = scene.store.reader().agent_calls(&item).await.unwrap();
    let questions: Vec<_> = calls
        .iter()
        .filter(|c| c.kind == AgentCallKind::Question { blocking: true })
        .collect();
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert_eq!(
        scene.state(1).await,
        Some(State::NeedsYou(Reason::Question))
    );
    let causes = causes(&scene, 1).await;
    assert_eq!(
        causes.iter().filter(|c| *c == "question").count(),
        1,
        "{causes:?}"
    );
    let card = scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == 1)
        .expect("the card is on the board");
    let asked: Vec<_> = card
        .comments
        .iter()
        .filter(|c| c.key.as_deref().is_some_and(|k| k.starts_with("demo#1/")))
        .collect();
    assert_eq!(asked.len(), 1, "{asked:#?}");
}
