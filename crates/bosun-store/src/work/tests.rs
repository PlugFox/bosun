use bosun_core::{
    Ctx, Effect, EffectKey, EffectKind, Event, Exit, Item, ItemId, RunResult, SpawnMode, State,
    Stepped, step,
};

use super::*;
use crate::writer::tests::{PROVIDER, record, source_file, write};
use crate::{Options, Store, Write};

const T0: i64 = 1_800_000_000_000;
const REPO: &str = "/work/repo";

async fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(dir.path().join("bosun.db"), Options::default())
        .await
        .unwrap()
}

fn id() -> ItemId {
    ItemId::new("local:demo#1")
}

fn register() -> Write {
    Write::Item(NewItem {
        item: id(),
        repo: REPO.to_owned(),
        card_key: Some("card-1".to_owned()),
        at_ms: T0,
    })
}

/// Applies `event`, which must take an arrow.
fn go(item: &Item, event: &Event) -> Stepped {
    step(item, event, &Ctx::at(T0))
        .unwrap_or_else(|| panic!("{event:?} ignored in {:?}", item.state))
}

fn transition(stepped: &Stepped) -> Write {
    Write::Transition {
        stepped: stepped.clone(),
        at_ms: T0 + i64::from(stepped.transition.key.seq),
    }
}

fn current(item: &Item) -> (u32, u32, u32) {
    let a = item.attempt.as_ref().unwrap();
    (a.number, a.spawn, a.run)
}

/// The events of one attempt that passes on its first verdict run, built from the item as it
/// goes: claim, session, exit, preconditions, verdict, landing.
fn happy_path() -> Vec<fn(&Item) -> Event> {
    vec![
        |_| Event::Claim,
        |_| Event::ClaimConfirmed,
        |item| {
            let (attempt, spawn, _) = current(item);
            Event::Exited {
                attempt,
                spawn,
                exit: Exit::Ok,
                text: String::new(),
            }
        },
        |item| Event::Preconditions {
            attempt: current(item).0,
            passed: true,
        },
        |item| {
            let (attempt, _, run) = current(item);
            Event::Verdict {
                attempt,
                run,
                result: RunResult::Pass,
            }
        },
        |_| Event::Landed,
    ]
}

async fn count(store: &Store, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(store.reader().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_transition_written_twice_is_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let confirmed = go(&claim.item, &Event::ClaimConfirmed);
    let (attempt, spawn, _) = current(&confirmed.item);
    let exited = go(
        &confirmed.item,
        &Event::Exited {
            attempt,
            spawn,
            exit: Exit::Ok,
            text: String::new(),
        },
    );
    let all = [&claim, &confirmed, &exited];
    store
        .writer()
        .write(all.iter().map(|s| transition(s)).collect())
        .await
        .unwrap();
    let outbox = store.reader().outbox(T0).await.unwrap();
    assert!(
        !outbox.is_empty(),
        "the confirmed claim writes to the board"
    );

    // A replay after a crash: one arrow alone, then all of them again in one batch.
    store
        .writer()
        .write(vec![transition(&confirmed)])
        .await
        .unwrap();
    store
        .writer()
        .write(all.iter().map(|s| transition(s)).collect())
        .await
        .unwrap();

    let rows = store.reader().transitions(&id()).await.unwrap();
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        ["local:demo#1/t1", "local:demo#1/t2", "local:demo#1/t3"]
    );
    assert_eq!(rows[1].from, "claimed");
    assert_eq!(rows[1].to, "running");
    assert_eq!(rows[1].attempt_to.as_deref(), Some("spawned"));
    assert_eq!(rows[1].cause, "claim_confirmed");
    assert_eq!(count(&store, "transition").await, 3);
    assert_eq!(
        store.reader().outbox(T0).await.unwrap(),
        outbox,
        "a replay adds no board write and folds nothing into the report"
    );
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.item, exited.item);
    store.close().await.unwrap();
}

#[tokio::test]
async fn an_item_reads_back_as_the_machine_left_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    assert_eq!(
        store.reader().item(&id()).await.unwrap().unwrap().item,
        Item::new(id())
    );
    let mut item = Item::new(id());
    for event in happy_path() {
        let stepped = go(&item, &event(&item));
        store
            .writer()
            .write(vec![transition(&stepped)])
            .await
            .unwrap();
        item = stepped.item;
        let stored = store.reader().item(&id()).await.unwrap().unwrap();
        assert_eq!(stored.item, item, "after {:?}", item.state);
        assert_eq!(stored.repo, REPO);
        assert_eq!(stored.card_key.as_deref(), Some("card-1"));
    }
    assert!(matches!(item.state, State::Done(_)));
    assert_eq!(store.reader().items(true).await.unwrap(), []);
    assert_eq!(store.reader().items(false).await.unwrap().len(), 1);
    let attempts = store.reader().attempts(&id()).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].attempt.state.passed());
    assert!(attempts[0].ended_ms.is_some());
    assert_eq!(attempts[0].refusal, Vec::<String>::new());
    // The reasons the daemon refused an attempt with (#137): the last write wins.
    let refusal = |reasons: &[&str]| Write::AttemptRefusal {
        item: id(),
        attempt: 1,
        reasons: reasons.iter().map(|r| (*r).to_owned()).collect(),
    };
    store
        .writer()
        .write(vec![
            refusal(&["first"]),
            refusal(&["no commit was made since the base", "a \"quoted\" one"]),
        ])
        .await
        .unwrap();
    assert_eq!(
        store.reader().attempts(&id()).await.unwrap()[0].refusal,
        ["no commit was made since the base", "a \"quoted\" one"]
    );
    assert_eq!(
        store.reader().item_counts().await.unwrap(),
        [("done".to_owned(), 1)]
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn a_parked_and_a_cancelled_item_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let running = go(&claim.item, &Event::ClaimConfirmed);
    let (attempt, spawn, _) = current(&running.item);
    let parked = go(
        &running.item,
        &Event::Exited {
            attempt,
            spawn,
            exit: Exit::Limit {
                reset_ms: Some(T0 + 60_000),
            },
            text: String::new(),
        },
    );
    assert!(matches!(parked.item.state, State::Parked(_)));
    let cancelled = go(&parked.item, &Event::User(bosun_core::UserCommand::Cancel));
    assert_eq!(cancelled.item.state, State::Cancelled);
    store
        .writer()
        .write(vec![
            transition(&claim),
            transition(&running),
            transition(&parked),
        ])
        .await
        .unwrap();
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.item, parked.item);
    assert_eq!(store.reader().items(true).await.unwrap().len(), 1);
    let rows = store.reader().transitions(&id()).await.unwrap();
    assert_eq!(rows[2].to_detail.as_deref(), Some("limit"));

    store
        .writer()
        .write(vec![transition(&cancelled)])
        .await
        .unwrap();
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.item, cancelled.item);
    store.close().await.unwrap();
}

fn report(seq: u32) -> Effect {
    Effect {
        key: EffectKey {
            item: id(),
            attempt: 1,
            seq,
            index: 0,
        },
        kind: EffectKind::Report,
    }
}

#[tokio::test]
async fn report_writes_of_one_key_coalesce_into_one_pending_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let enqueue = |seq| Write::Enqueue {
        effect: report(seq),
        at_ms: T0 + i64::from(seq),
    };
    store
        .writer()
        .write(vec![enqueue(1), enqueue(2)])
        .await
        .unwrap();
    let pending = store.reader().outbox(T0).await.unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let row = &pending[0];
    assert_eq!(row.op, OutboxOp::UpsertComment);
    assert_eq!(row.coalesce_key.as_deref(), Some("local:demo#1/report"));
    assert_eq!(row.revision, 2);
    assert_eq!(row.effect_key, "local:demo#1/a1/t1/e0");

    // No rendered body is stored: the row holds keys and names, the body is rendered at send.
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('outbox')")
        .fetch_all(store.reader().pool())
        .await
        .unwrap();
    assert!(
        !columns
            .iter()
            .any(|c| c.contains("body") || c.contains("text")),
        "{columns:?}"
    );

    // An acknowledgement of what was sent before the second write keeps the row pending.
    let (id_, sent) = (row.id, 1);
    store
        .writer()
        .write(vec![Write::Acknowledge {
            id: id_,
            revision: sent,
            at_ms: T0 + 10,
        }])
        .await
        .unwrap();
    assert_eq!(store.reader().outbox(T0).await.unwrap().len(), 1);
    store
        .writer()
        .write(vec![Write::Acknowledge {
            id: id_,
            revision: 2,
            at_ms: T0 + 11,
        }])
        .await
        .unwrap();
    assert_eq!(store.reader().outbox(T0).await.unwrap(), []);

    // Once acknowledged, the next report is a new pending row; a replayed key adds nothing.
    store
        .writer()
        .write(vec![enqueue(3), enqueue(3), enqueue(1)])
        .await
        .unwrap();
    let pending = store.reader().outbox(T0).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].revision, 1);
    assert_eq!(count(&store, "outbox").await, 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn the_outbox_keeps_order_and_waits_after_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let running = go(&claim.item, &Event::ClaimConfirmed);
    store
        .writer()
        .write(vec![transition(&claim), transition(&running)])
        .await
        .unwrap();
    let pending = store.reader().outbox(T0).await.unwrap();
    // Every board write of both arrows in order, the sticky report once: the confirmed claim's
    // report folds into the claim's pending one.
    let mut report = false;
    let expected: Vec<String> = [&claim, &running]
        .iter()
        .flat_map(|s| s.transition.effects.iter())
        .filter(|e| match e.kind {
            EffectKind::Project(_) | EffectKind::Comment(_) => true,
            EffectKind::Report => !std::mem::replace(&mut report, true),
            _ => false,
        })
        .map(|e| e.key.to_string())
        .collect();
    let keys: Vec<String> = pending.iter().map(|r| r.effect_key.clone()).collect();
    assert_eq!(keys, expected);
    let first = pending[0].id;
    store
        .writer()
        .write(vec![Write::SendFailed {
            id: first,
            error: "rate limited".to_owned(),
            retry_at_ms: T0 + 1_000,
        }])
        .await
        .unwrap();
    let now = store.reader().outbox(T0).await.unwrap();
    assert!(now.iter().all(|r| r.id != first));
    let later = store.reader().outbox(T0 + 1_000).await.unwrap();
    assert_eq!(later[0].id, first);
    assert_eq!(later[0].tries, 1);
    assert_eq!(later[0].last_error.as_deref(), Some("rate limited"));
    store.close().await.unwrap();
}

/// Records the item up to a running session and returns the session's attempt and number.
async fn running(store: &Store) -> (u32, u32) {
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let running = go(&claim.item, &Event::ClaimConfirmed);
    store
        .writer()
        .write(vec![transition(&claim), transition(&running)])
        .await
        .unwrap();
    let (attempt, spawn, _) = current(&running.item);
    (attempt, spawn)
}

fn run_start(attempt: u32, spawn: u32, session_key: &str) -> Write {
    Write::Run(RunStart {
        item: id(),
        attempt,
        spawn,
        mode: SpawnMode::Fresh,
        provider: PROVIDER.to_owned(),
        session_key: session_key.to_owned(),
        started_ms: T0,
    })
}

async fn kinds(store: &Store) -> Vec<(String, Option<String>, String)> {
    sqlx::query_as(
        "SELECT session_key, agent_key, kind FROM session ORDER BY session_key, agent_key",
    )
    .fetch_all(store.reader().pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn a_transcript_is_a_run_session_in_either_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let (attempt, spawn) = running(&store).await;

    // `session-a`: the run row first, then its transcript. `record` names the session after
    // its file.
    store
        .writer()
        .write(vec![run_start(attempt, spawn, "session-a")])
        .await
        .unwrap();
    store
        .writer()
        .write(vec![source_file("a"), write(record("m1", "a", T0, 5, &[]))])
        .await
        .unwrap();

    // `session-b`: its transcript first (with a sub-agent, and the meta ingest writes), then the
    // run row of a resumed session of the same attempt.
    let mut sub = record("m2", "b", T0, 5, &[]);
    sub.origin.window.agent_id = Some("agent-1".to_owned());
    let meta = |agent: Option<&str>| Write::Session {
        provider: PROVIDER.to_owned(),
        meta: bosun_core::WindowMeta {
            window: bosun_core::ContextWindow {
                key: bosun_core::WindowKey {
                    session_id: "session-b".to_owned(),
                    agent_id: agent.map(str::to_owned),
                },
                parent: None,
                kind: bosun_core::WindowKind::Interactive,
            },
            agent_type: None,
            spawn_depth: None,
            spawned_by: None,
        },
    };
    store
        .writer()
        .write(vec![
            source_file("b"),
            meta(None),
            write(record("m3", "b", T0, 5, &[])),
            write(sub),
            // An interactive session nobody spawned.
            source_file("c"),
            write(record("m4", "c", T0, 5, &[])),
        ])
        .await
        .unwrap();
    store
        .writer()
        .write(vec![run_start(attempt, spawn + 1, "session-b")])
        .await
        .unwrap();
    let run = "run".to_owned();
    let expected = [
        ("session-a".to_owned(), None, run.clone()),
        ("session-b".to_owned(), None, run.clone()),
        ("session-b".to_owned(), Some("agent-1".to_owned()), run),
        ("session-c".to_owned(), None, "interactive".to_owned()),
    ];
    assert_eq!(kinds(&store).await, expected);

    // The transcript is read again afterwards: ingest says interactive, the run row wins.
    store
        .writer()
        .write(vec![meta(None), meta(Some("agent-1"))])
        .await
        .unwrap();
    assert_eq!(kinds(&store).await, expected);
    let runs = store.reader().runs(&id()).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].session_key, "session-b");
    store.close().await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn children_are_recorded_until_they_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let (attempt, spawn) = running(&store).await;
    store
        .writer()
        .write(vec![
            run_start(attempt, spawn, "session-a"),
            Write::SessionChild {
                item: id(),
                attempt,
                pid: 4_242,
                pid_started_ms: T0 + 1,
            },
        ])
        .await
        .unwrap();
    let children = store.reader().children().await.unwrap();
    assert_eq!(
        children,
        [RecordedChild {
            item: id(),
            attempt,
            verdict_run: None,
            pid: 4_242,
            started_ms: T0 + 1,
        }]
    );
    store
        .writer()
        .write(vec![
            Write::RunEnded(RunEnd {
                item: id(),
                attempt,
                spawn,
                exit: Exit::Limit {
                    reset_ms: Some(T0 + 9),
                },
                head_sha: Some("abc".to_owned()),
                ended_ms: T0 + 2,
            }),
            Write::VerdictStarted(VerdictStart {
                item: id(),
                attempt,
                run: 1,
                command: "cargo test".to_owned(),
                pid: 77,
                pid_started_ms: T0 + 3,
                started_ms: T0 + 3,
            }),
        ])
        .await
        .unwrap();
    let children = store.reader().children().await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!((children[0].verdict_run, children[0].pid), (Some(1), 77));
    let runs = store.reader().runs(&id()).await.unwrap();
    assert_eq!(
        runs[0].exit,
        Some(Exit::Limit {
            reset_ms: Some(T0 + 9)
        })
    );
    store
        .writer()
        .write(vec![Write::VerdictEnded(VerdictEnd {
            item: id(),
            attempt,
            run: 1,
            result: RunResult::Fail,
            exit_code: Some(101),
            duration_ms: 1_500,
            verdict_line: Some("test result: FAILED".to_owned()),
            tail: Some("...".to_owned()),
            ended_ms: T0 + 4,
        })])
        .await
        .unwrap();
    assert_eq!(store.reader().children().await.unwrap(), []);
    let verdicts = store.reader().verdict_runs(&id()).await.unwrap();
    assert_eq!(verdicts[0].result, Some(RunResult::Fail));
    assert_eq!(verdicts[0].duration_ms, Some(1_500));
    assert_eq!(
        store.reader().attempts(&id()).await.unwrap()[0]
            .head_sha
            .as_deref(),
        Some("abc")
    );

    // Ending a verdict run that never started fails the batch.
    let err = store
        .writer()
        .write(vec![Write::VerdictEnded(VerdictEnd {
            item: id(),
            attempt,
            run: 9,
            result: RunResult::Pass,
            exit_code: Some(0),
            duration_ms: 1,
            verdict_line: None,
            tail: None,
            ended_ms: T0 + 5,
        })])
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::UnknownRow(_)), "{err}");
    store.close().await.unwrap();
}

#[tokio::test]
async fn the_last_accepted_summary_of_an_attempt_wins() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let (attempt, _) = running(&store).await;
    let call = |kind, body: &str, head: &str, refused, at| {
        Write::AgentCall(AgentCall {
            item: id(),
            attempt,
            kind,
            body: body.to_owned(),
            head_sha: Some(head.to_owned()),
            refused,
            at_ms: T0 + at,
        })
    };
    let summary = |body: &str| AgentCallKind::Summary {
        decisions: vec![format!("{body} decision")],
        followups: Vec::new(),
    };
    store
        .writer()
        .write(vec![
            call(summary("first"), "first", "h1", false, 1),
            call(AgentCallKind::Note, "a note", "h1", false, 2),
            call(summary("second"), "second", "h2", false, 3),
            call(
                AgentCallKind::Question { blocking: false },
                "why?",
                "h2",
                false,
                4,
            ),
            call(summary("over the cap"), "over the cap", "h3", true, 5),
        ])
        .await
        .unwrap();
    let last = store
        .reader()
        .summary(&id(), attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.body, "second");
    assert_eq!(last.head_sha.as_deref(), Some("h2"));
    assert_eq!(
        last.kind,
        AgentCallKind::Summary {
            decisions: vec!["second decision".to_owned()],
            followups: Vec::new(),
        }
    );
    let calls = store.reader().agent_calls(&id()).await.unwrap();
    assert_eq!(calls.len(), 5, "every call is a row, a refused one too");
    assert!(calls[4].refused);
    assert_eq!(calls[3].kind, AgentCallKind::Question { blocking: false });
    // Agent calls are not transitions.
    assert_eq!(store.reader().transitions(&id()).await.unwrap().len(), 2);
    store.close().await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn snapshots_comments_flaky_tests_and_boots() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let snapshot = |key: &str, hash: &str| Snapshot {
        item: id(),
        key: key.to_owned(),
        taken_ms: T0,
        hash: hash.to_owned(),
        body: "Do the thing.".to_owned(),
        contract_json: r#"{"verdict":"cargo test"}"#.to_owned(),
        comments_json: "[]".to_owned(),
        config_hash: Some("c".to_owned()),
        policy_hash: Some("p".to_owned()),
        base_sha: Some("base".to_owned()),
        branch: Some("main".to_owned()),
        base_refs_json: Some(r#"{"refs/heads/main":"base"}"#.to_owned()),
        base_stash_json: Some("[]".to_owned()),
    };
    store
        .writer()
        .write(vec![
            Write::Snapshot(snapshot("local:demo#1/a0/t1/e0", "h1")),
            // A replay of the claim stores nothing new.
            Write::Snapshot(snapshot("local:demo#1/a0/t1/e0", "other")),
        ])
        .await
        .unwrap();
    assert_eq!(
        store.reader().snapshot(&id()).await.unwrap().unwrap(),
        snapshot("local:demo#1/a0/t1/e0", "h1")
    );
    assert_eq!(count(&store, "snapshot").await, 1);

    // The attempt runs from the snapshot current when it started.
    let claim = go(&Item::new(id()), &Event::Claim);
    let running = go(&claim.item, &Event::ClaimConfirmed);
    store
        .writer()
        .write(vec![
            transition(&claim),
            transition(&running),
            // A re-snapshot (a restart) is the next attempt's.
            Write::Snapshot(snapshot("local:demo#1/a1/t9/e0", "h2")),
        ])
        .await
        .unwrap();
    let attempts = store.reader().attempts(&id()).await.unwrap();
    assert_eq!(attempts[0].snapshot_hash.as_deref(), Some("h1"));
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.snapshot_hash.as_deref(), Some("h2"));

    let comment = |node: &str| Write::BoardComment {
        item: id(),
        key: "local:demo#1/report".to_owned(),
        node_id: node.to_owned(),
        at_ms: T0,
    };
    store
        .writer()
        .write(vec![comment("IC_1"), comment("IC_2")])
        .await
        .unwrap();
    assert_eq!(
        store
            .reader()
            .board_comment(&id(), "local:demo#1/report")
            .await
            .unwrap()
            .as_deref(),
        Some("IC_2")
    );
    assert_eq!(
        store.reader().board_comments(&id()).await.unwrap(),
        ["IC_2"]
    );

    let flaky = Write::FlakyTests {
        item: id(),
        attempt: 1,
        names: vec!["tests::b".to_owned(), "tests::a".to_owned()],
        at_ms: T0,
    };
    store
        .writer()
        .write(vec![flaky.clone(), flaky])
        .await
        .unwrap();
    assert_eq!(
        store.reader().flaky_tests(&id()).await.unwrap(),
        [(1, "tests::a".to_owned()), (1, "tests::b".to_owned())]
    );

    // An item's notes (#111): one per kind, the last write wins.
    assert_eq!(store.reader().item_note(&id(), "lint").await.unwrap(), None);
    let note = |kind: &str, body: &str| Write::ItemNote {
        item: id(),
        kind: kind.to_owned(),
        body: body.to_owned(),
        at_ms: T0,
    };
    store
        .writer()
        .write(vec![
            note("lint", "[\"first\"]"),
            note("lint", "[\"second\"]"),
            note("budget", "{}"),
        ])
        .await
        .unwrap();
    assert_eq!(
        store
            .reader()
            .item_note(&id(), "lint")
            .await
            .unwrap()
            .as_deref(),
        Some("[\"second\"]")
    );
    assert_eq!(
        store
            .reader()
            .item_note(&id(), "budget")
            .await
            .unwrap()
            .as_deref(),
        Some("{}")
    );

    assert_eq!(store.reader().last_boot().await.unwrap(), None);
    store
        .writer()
        .write(vec![
            Write::Boot {
                number: 1,
                started_ms: T0,
                pid: 10,
            },
            Write::Boot {
                number: 2,
                started_ms: T0 + 1,
                pid: 11,
            },
        ])
        .await
        .unwrap();
    assert_eq!(store.reader().last_boot().await.unwrap(), Some(2));
    store.close().await.unwrap();
}

#[tokio::test]
async fn a_write_for_an_unknown_item_fails_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let claim = go(&Item::new(id()), &Event::Claim);
    let err = store
        .writer()
        .write(vec![transition(&claim)])
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::UnknownItem(_)), "{err}");
    store.writer().write(vec![register()]).await.unwrap();
    let err = store
        .writer()
        .write(vec![run_start(1, 1, "s")])
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::Error::UnknownAttempt { attempt: 1, .. }),
        "{err}"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn a_transition_from_a_stale_item_fails_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let confirmed = go(&claim.item, &Event::ClaimConfirmed);
    let lost = go(&claim.item, &Event::ClaimLost);
    store
        .writer()
        .write(vec![transition(&claim), transition(&confirmed)])
        .await
        .unwrap();
    // Another arrow under a stored key, computed from the same stale item.
    let err = store
        .writer()
        .write(vec![transition(&lost)])
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::Stale(_)), "{err}");
    // An arrow that skips one.
    let (attempt, spawn, _) = current(&confirmed.item);
    let exited = go(
        &confirmed.item,
        &Event::Exited {
            attempt,
            spawn,
            exit: Exit::Ok,
            text: String::new(),
        },
    );
    let verify = go(
        &exited.item,
        &Event::Preconditions {
            attempt,
            passed: true,
        },
    );
    let err = store
        .writer()
        .write(vec![transition(&verify)])
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::Stale(_)), "{err}");
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.item, confirmed.item);
    store.close().await.unwrap();
}

#[tokio::test]
async fn a_late_session_end_keeps_the_current_child_and_a_reaped_child_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let claim = go(&Item::new(id()), &Event::Claim);
    let first = go(&claim.item, &Event::ClaimConfirmed);
    let (attempt, spawn, _) = current(&first.item);
    // The session crashes and is retried: a second session of the same attempt.
    let retried = go(
        &first.item,
        &Event::Exited {
            attempt,
            spawn,
            exit: Exit::Crash,
            text: String::new(),
        },
    );
    let (again, next, _) = current(&retried.item);
    assert_eq!(again, attempt);
    assert_ne!(next, spawn);
    let child = |pid| Write::SessionChild {
        item: id(),
        attempt,
        pid,
        pid_started_ms: T0 + i64::from(pid),
    };
    store
        .writer()
        .write(vec![
            transition(&claim),
            transition(&first),
            run_start(attempt, spawn, "session-a"),
            child(5),
            transition(&retried),
            run_start(attempt, next, "session-b"),
            child(9),
        ])
        .await
        .unwrap();
    // The first session's end is recorded late, after the second started.
    store
        .writer()
        .write(vec![Write::RunEnded(RunEnd {
            item: id(),
            attempt,
            spawn,
            exit: Exit::Crash,
            head_sha: None,
            ended_ms: T0,
        })])
        .await
        .unwrap();
    let children = store.reader().children().await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].pid, 9);

    // Reaped at start: no row names it any more.
    store
        .writer()
        .write(vec![Write::ChildGone {
            pid: 9,
            started_ms: T0 + 9,
        }])
        .await
        .unwrap();
    assert_eq!(store.reader().children().await.unwrap(), []);

    // Two starts never share a number.
    let boot = |pid| Write::Boot {
        number: 1,
        started_ms: T0,
        pid,
    };
    store.writer().write(vec![boot(1)]).await.unwrap();
    store.writer().write(vec![boot(2)]).await.unwrap_err();
    store.close().await.unwrap();
}

fn range(spawn: u32, base: &str, head: &str, commits: &[&str]) -> Write {
    Write::RunCommits(RunCommits {
        item: id(),
        attempt: 1,
        spawn,
        base_sha: base.to_owned(),
        head_sha: head.to_owned(),
        commits: commits.iter().map(|&c| c.to_owned()).collect(),
    })
}

fn commit(sha: &str, ts_ms: i64) -> Write {
    Write::Commit {
        repo: REPO.to_owned(),
        commit: bosun_core::Commit {
            sha: sha.to_owned(),
            ts_ms,
            author: "A U Thor <author@example.invalid>".to_owned(),
            lines_added: 1,
            lines_removed: 0,
            trailers: std::collections::BTreeMap::new(),
        },
    }
}

#[tokio::test]
async fn a_commit_belongs_to_the_first_session_range_that_lists_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let claim = go(&Item::new(id()), &Event::Claim);
    let running = go(&claim.item, &Event::ClaimConfirmed);
    store
        .writer()
        .write(vec![
            register(),
            transition(&claim),
            transition(&running),
            run_start(1, 1, "session-a"),
            run_start(1, 2, "session-b"),
        ])
        .await
        .unwrap();
    // A range of a session that was never spawned fails the batch.
    let err = store
        .writer()
        .write(vec![range(9, "b", "h", &["c1"])])
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::UnknownRow(_)), "{err}");
    store
        .writer()
        .write(vec![
            range(1, "base", "h1", &["c1", "c2"]),
            // Written again, and a second session that lists c2 again: c2 stays session 1's.
            range(1, "base", "h1", &["c1", "c2"]),
            range(2, "h1", "h2", &["c2", "c3"]),
            commit("c1", T0 + 1_000),
            commit("c2", T0 + 2_000),
            commit("c3", T0 + 3_000),
            commit("outside", T0 + 4_000),
        ])
        .await
        .unwrap();
    let rows = store.reader().run_commits(0, i64::MAX).await.unwrap();
    let links: Vec<(&str, u32, &str)> = rows
        .iter()
        .map(|r| (r.commit.sha.as_str(), r.spawn, r.session_key.as_str()))
        .collect();
    assert_eq!(
        links,
        [
            ("c1", 1, "session-a"),
            ("c2", 1, "session-a"),
            ("c3", 2, "session-b")
        ]
    );
    assert!(rows.iter().all(|r| r.item == id() && r.attempt == 1));
    assert_eq!(count(&store, "run_commit").await, 3);
    let runs = store.reader().runs(&id()).await.unwrap();
    assert_eq!(runs[1].base_sha.as_deref(), Some("h1"));
    assert_eq!(runs[1].head_sha.as_deref(), Some("h2"));
    // The range selects by commit time.
    let ranged = store
        .reader()
        .run_commits(T0 + 2_000, T0 + 3_000)
        .await
        .unwrap();
    assert_eq!(ranged.len(), 1);
    assert_eq!(ranged[0].commit.sha, "c2");
    let histories = store.reader().item_histories(T0, T0 + 10).await.unwrap();
    assert_eq!(histories.len(), 1);
    assert_eq!(histories[0].runs.len(), 2);
    assert_eq!(store.reader().item_histories(0, T0).await.unwrap(), []);
    store.close().await.unwrap();
}

#[tokio::test]
async fn an_attempt_dropped_after_two_failed_runs_is_stored_as_failed() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let mut item = Item::new(id());
    let mut writes = Vec::new();
    let fail = |item: &Item| {
        let (attempt, _, run) = current(item);
        Event::Verdict {
            attempt,
            run,
            result: RunResult::Fail,
        }
    };
    let events: Vec<fn(&Item) -> Event> = vec![
        |_| Event::Claim,
        |_| Event::ClaimConfirmed,
        |item| {
            let (attempt, spawn, _) = current(item);
            Event::Exited {
                attempt,
                spawn,
                exit: Exit::Ok,
                text: String::new(),
            }
        },
        |item| Event::Preconditions {
            attempt: current(item).0,
            passed: true,
        },
    ];
    for event in events {
        let stepped = go(&item, &event(&item));
        writes.push(transition(&stepped));
        item = stepped.item;
    }
    for _ in 0..2 {
        let stepped = go(&item, &fail(&item));
        writes.push(transition(&stepped));
        item = stepped.item;
    }
    assert_eq!(
        current(&item).0,
        2,
        "the second failed run starts attempt 2"
    );
    store.writer().write(writes).await.unwrap();
    let attempts = store.reader().attempts(&id()).await.unwrap();
    let states: Vec<_> = attempts
        .iter()
        .map(|a| (a.attempt.number, a.attempt.state.as_str()))
        .collect();
    assert_eq!(states, [(1, "fail"), (2, "spawned")]);
    assert!(attempts[0].ended_ms.is_some());
}

/// Writes `batch` twice, as a replay after a crash would.
async fn twice(store: &Store, batch: Vec<Write>) {
    store.writer().write(batch.clone()).await.unwrap();
    store.writer().write(batch).await.unwrap();
}

/// #150: the stage-2 rows of one item (its worktree, its session shim and its landing) are
/// written idempotently and read back; output offsets only grow, a quarantined worktree is no
/// longer open, and a landing update keeps what it does not give.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_worktree_its_process_and_its_landing_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let reader = store.reader();

    let worktree = NewWorktree {
        item: id(),
        path: "/work/trees/demo-1".to_owned(),
        branch: "bosun/demo-1".to_owned(),
        base_sha: "a".repeat(40),
        kind: WorktreeKind::Item,
        user_base_json: Some("{\"branch\":\"master\"}".to_owned()),
        created_ms: T0,
    };
    twice(&store, vec![Write::Worktree(worktree.clone())]).await;
    twice(
        &store,
        vec![Write::WorktreeBytes {
            path: worktree.path.clone(),
            bytes: 4096,
        }],
    )
    .await;
    let created = WorktreeRow {
        item: id(),
        path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        base_sha: worktree.base_sha.clone(),
        kind: WorktreeKind::Item,
        state: WorktreeState::Created,
        blocking: None,
        error: None,
        bytes: Some(4096),
        user_base_json: worktree.user_base_json.clone(),
        created_ms: T0,
        ended_ms: None,
    };
    assert_eq!(
        reader.worktrees(&id()).await.unwrap(),
        std::slice::from_ref(&created)
    );
    assert_eq!(
        reader.open_worktrees().await.unwrap(),
        std::slice::from_ref(&created)
    );
    assert_eq!(reader.quarantined_worktrees().await.unwrap(), []);

    let process = NewProcess {
        item: id(),
        attempt: 1,
        spawn: 1,
        session_key: "session-1".to_owned(),
        pid: 4242,
        pid_started_ms: T0 + 1,
        record_path: "/state/shims/1.json".to_owned(),
        credential_digest: Some(vec![0xab; 32]),
        started_ms: T0 + 2,
    };
    twice(&store, vec![Write::Process(process.clone())]).await;
    let offsets = |stdout, stderr| Write::ProcessOffsets {
        record_path: process.record_path.clone(),
        stdout,
        stderr,
    };
    twice(&store, vec![offsets(10, 5)]).await;
    twice(&store, vec![offsets(4, 9)]).await;
    let running = ProcessRow {
        item: id(),
        attempt: 1,
        spawn: 1,
        session_key: process.session_key.clone(),
        pid: 4242,
        pid_started_ms: T0 + 1,
        record_path: process.record_path.clone(),
        stdout_offset: 10,
        stderr_offset: 9,
        credential_digest: process.credential_digest.clone(),
        started_ms: T0 + 2,
        gone_ms: None,
    };
    assert_eq!(reader.processes().await.unwrap(), [running]);
    twice(
        &store,
        vec![Write::ProcessGone {
            record_path: process.record_path.clone(),
            at_ms: T0 + 3,
        }],
    )
    .await;
    assert_eq!(reader.processes().await.unwrap(), []);
    assert_eq!(count(&store, "process").await, 1);

    twice(
        &store,
        vec![Write::WorktreeEnded {
            path: worktree.path.clone(),
            state: WorktreeState::Quarantined,
            blocking: Some("target/lock".to_owned()),
            error: Some("access denied".to_owned()),
            bytes: None,
            at_ms: T0 + 4,
        }],
    )
    .await;
    // A later end does not replace the first.
    store
        .writer()
        .write(vec![Write::WorktreeEnded {
            path: worktree.path.clone(),
            state: WorktreeState::Removed,
            blocking: None,
            error: None,
            bytes: None,
            at_ms: T0 + 5,
        }])
        .await
        .unwrap();
    let quarantined = WorktreeRow {
        state: WorktreeState::Quarantined,
        blocking: Some("target/lock".to_owned()),
        error: Some("access denied".to_owned()),
        ended_ms: Some(T0 + 4),
        ..created
    };
    assert_eq!(reader.open_worktrees().await.unwrap(), []);
    assert_eq!(
        reader.quarantined_worktrees().await.unwrap(),
        std::slice::from_ref(&quarantined)
    );
    assert_eq!(reader.worktrees(&id()).await.unwrap(), [quarantined]);
    assert_eq!(count(&store, "worktree").await, 1);

    let landing = LandingRow {
        item: id(),
        attempt: 1,
        target: "master".to_owned(),
        head_sha: Some("b".repeat(40)),
        verified_tree: Some("c".repeat(40)),
        landed_sha: None,
        delivered_ms: None,
        pr_number: Some(12),
        pr_url: Some("https://forge.invalid/pr/12".to_owned()),
        pr_state: Some(PrState::Open),
        updated_ms: T0 + 6,
    };
    twice(&store, vec![Write::Landing(landing.clone())]).await;
    assert_eq!(reader.landing(&id()).await.unwrap(), Some(landing.clone()));
    assert_eq!(
        reader.open_pull_requests().await.unwrap(),
        std::slice::from_ref(&landing)
    );
    let landed = LandingRow {
        head_sha: None,
        verified_tree: None,
        landed_sha: Some("d".repeat(40)),
        delivered_ms: Some(T0 + 7),
        pr_number: None,
        pr_url: None,
        pr_state: Some(PrState::Merged),
        updated_ms: T0 + 7,
        ..landing.clone()
    };
    twice(&store, vec![Write::Landing(landed)]).await;
    assert_eq!(
        reader.landing(&id()).await.unwrap(),
        Some(LandingRow {
            landed_sha: Some("d".repeat(40)),
            delivered_ms: Some(T0 + 7),
            pr_state: Some(PrState::Merged),
            updated_ms: T0 + 7,
            ..landing
        })
    );
    assert_eq!(reader.open_pull_requests().await.unwrap(), []);
    assert_eq!(count(&store, "landing").await, 1);
    store.close().await.unwrap();
}

/// #150: a lease request is filed, granted and released, each written twice: one row, with its
/// three times. A `bosun with` command's request has no item.
#[tokio::test]
async fn a_lease_wait_is_filed_granted_and_released_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let filed = |item: Option<ItemId>, holder: &str| Write::LeaseFiled {
        item,
        holder: holder.to_owned(),
        resource: "gpu".to_owned(),
        filed_ms: T0,
    };
    twice(&store, vec![filed(Some(id()), "local:demo#1/a1")]).await;
    twice(
        &store,
        vec![Write::LeaseGranted {
            holder: "local:demo#1/a1".to_owned(),
            resource: "gpu".to_owned(),
            filed_ms: T0,
            at_ms: T0 + 10,
        }],
    )
    .await;
    twice(
        &store,
        vec![Write::LeaseReleased {
            holder: "local:demo#1/a1".to_owned(),
            at_ms: T0 + 20,
        }],
    )
    .await;
    // A later release of the holder leaves the released request as it was.
    store
        .writer()
        .write(vec![Write::LeaseReleased {
            holder: "local:demo#1/a1".to_owned(),
            at_ms: T0 + 30,
        }])
        .await
        .unwrap();
    twice(&store, vec![filed(None, "with:cargo test")]).await;

    assert_eq!(count(&store, "lease_wait").await, 2);
    let waits = store.reader().lease_waits(T0, T0 + 1).await.unwrap();
    assert_eq!(
        waits,
        [
            LeaseWaitRow {
                item: Some(id()),
                holder: "local:demo#1/a1".to_owned(),
                resource: "gpu".to_owned(),
                filed_ms: T0,
                granted_ms: Some(T0 + 10),
                released_ms: Some(T0 + 20),
            },
            LeaseWaitRow {
                item: None,
                holder: "with:cargo test".to_owned(),
                resource: "gpu".to_owned(),
                filed_ms: T0,
                granted_ms: None,
                released_ms: None,
            },
        ]
    );
    assert_eq!(
        store.reader().lease_waits(T0 + 1, T0 + 2).await.unwrap(),
        []
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn an_item_after_a_conflict_reads_back_with_its_counter() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    store.writer().write(vec![register()]).await.unwrap();
    let mut item = Item::new(id());
    let mut batch = Vec::new();
    // The happy path up to its landing, then a conflict: one session of the same attempt.
    let mut events = happy_path();
    events.pop();
    events.push(|item| Event::Conflict {
        attempt: current(item).0,
    });
    for event in events {
        let stepped = go(&item, &event(&item));
        batch.push(transition(&stepped));
        item = stepped.item;
    }
    assert_eq!((item.state, item.conflicts), (State::Running, 1));
    store.writer().write(batch).await.unwrap();
    let stored = store.reader().item(&id()).await.unwrap().unwrap();
    assert_eq!(stored.item, item);
    assert_eq!(stored.item.conflicts, 1);

    // A failed setup is Needs you with its own reason.
    let other = ItemId::new("local:demo#2");
    store
        .writer()
        .write(vec![Write::Item(NewItem {
            item: other.clone(),
            repo: REPO.to_owned(),
            card_key: Some("card-2".to_owned()),
            at_ms: T0,
        })])
        .await
        .unwrap();
    let claimed = go(&Item::new(other.clone()), &Event::Claim);
    let setup = go(&claimed.item, &Event::SetupFailed);
    assert_eq!(setup.item.state, State::NeedsYou(bosun_core::Reason::Setup));
    store
        .writer()
        .write(vec![transition(&claimed), transition(&setup)])
        .await
        .unwrap();
    let stored = store.reader().item(&other).await.unwrap().unwrap();
    assert_eq!(stored.item, setup.item);
    store.close().await.unwrap();
}
