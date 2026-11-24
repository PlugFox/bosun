//! The outbox, the sticky report and the user's commands (#65), on the loop of the scenario
//! suite: the local-file board, behind a fault-injecting wrapper where a scenario needs the
//! board to fail.

use std::sync::atomic::{AtomicBool, AtomicI64};

use bosun_core::publish::report_key;
use bosun_queue::{Changes, Comment, Cursor, Owned, QueueError};

use super::*;

/// The local file, with an outage on the scene's clock and lost responses on demand.
#[derive(Debug)]
struct Faulty {
    inner: LocalFile,
    clock: Arc<TestClock>,
    /// Every call fails while the clock is before this (ms); 0: no outage.
    down_until_ms: AtomicI64,
    /// The next comment write after the outage is carried out but its response is lost.
    lose_next_comment: AtomicBool,
    /// Report writes fail (are not carried out) while set.
    hold_reports: AtomicBool,
    /// Writes carried out (column writes and comments).
    writes: AtomicUsize,
    /// Thrown as soon as the next write is carried out: a kill in the middle of a drain.
    kill_after_write: std::sync::Mutex<Option<crate::control::Control>>,
}

impl Faulty {
    fn new(scene: &Scene) -> Arc<Self> {
        Arc::new(Self {
            inner: LocalFile::new(&scene.board),
            clock: scene.clock.clone(),
            down_until_ms: AtomicI64::new(0),
            lose_next_comment: AtomicBool::new(false),
            hold_reports: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            kill_after_write: std::sync::Mutex::new(None),
        })
    }

    /// A write was carried out.
    fn wrote(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if let Some(control) = self.kill_after_write.lock().unwrap().take() {
            control.set_killed(true);
        }
    }

    fn down(&self) -> Result<(), QueueError> {
        if self.clock.wall_ms() < self.down_until_ms.load(Ordering::SeqCst) {
            return Err(QueueError::Remote("the board is unreachable".to_owned()));
        }
        Ok(())
    }

    /// A comment write: carried out, and its response lost when asked.
    fn comment_write(
        &self,
        write: impl FnOnce() -> Result<(), QueueError>,
    ) -> Result<(), QueueError> {
        self.down()?;
        write()?;
        self.wrote();
        if self.lose_next_comment.swap(false, Ordering::SeqCst) {
            return Err(QueueError::Remote("the response was lost".to_owned()));
        }
        Ok(())
    }
}

impl Queue for Faulty {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        self.down()?;
        self.inner.poll(cursor)
    }

    fn project(
        &self,
        item: bosun_queue::ItemId,
        state: Owned,
        note: Option<&str>,
    ) -> Result<(), QueueError> {
        self.down()?;
        self.inner.project(item, state, note)?;
        self.wrote();
        Ok(())
    }

    fn comment(&self, item: bosun_queue::ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        self.comment_write(|| self.inner.comment(item, body, key))
    }

    fn upsert_comment(
        &self,
        item: bosun_queue::ItemId,
        key: &str,
        body: &str,
    ) -> Result<(), QueueError> {
        if self.hold_reports.load(Ordering::SeqCst) {
            return Err(QueueError::Remote("the board refuses for now".to_owned()));
        }
        self.comment_write(|| self.inner.upsert_comment(item, key, body))
    }

    fn add_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.down()?;
        self.inner.add_label(item, label)
    }

    fn remove_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.down()?;
        self.inner.remove_label(item, label)
    }

    fn lint_target(&self, item: bosun_queue::ItemId) -> Result<Card, QueueError> {
        self.down()?;
        self.inner.lint_target(item)
    }
}

fn id(n: u64) -> ItemId {
    ItemId::new(format!("demo#{n}"))
}

/// The card `n` as the board holds it.
fn card_now(scene: &Scene, n: u64) -> Card {
    scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == n)
        .expect("the card is on the board")
}

/// The daemon's comments on card `n` whose key starts with `prefix`.
fn keyed(scene: &Scene, n: u64, prefix: &str) -> Vec<Comment> {
    card_now(scene, n)
        .comments
        .into_iter()
        .filter(|c| c.key.as_deref().is_some_and(|k| k.starts_with(prefix)))
        .collect()
}

/// Acts as the user on card `n`: `edit` changes it, the board is written back.
fn user_edits(scene: &Scene, n: u64, edit: impl FnOnce(&mut Card)) {
    let mut cards = scene.cards();
    let card = cards.iter_mut().find(|c| c.id.0 == n).unwrap();
    edit(card);
    scene.write_board(&cards);
}

/// The credential of the item's latest session, from its MCP configuration.
async fn latest_credential(scene: &Scene, n: u64) -> String {
    let runs = scene.runs(n).await;
    let session = &runs.last().expect("a session").session_key;
    let path = scene
        .path("work")
        .join("sessions")
        .join(session)
        .join("mcp.json");
    let text = std::fs::read_to_string(path).unwrap();
    let config: serde_json::Value = serde_json::from_str(&text).unwrap();
    config["mcpServers"]["bosun"]["env"][session::CREDENTIAL_VAR]
        .as_str()
        .unwrap()
        .to_owned()
}

/// #65: the board fails for ten simulated minutes in the middle of an item. The item finishes
/// anyway (`mutant_outbox_stop_on_outage` makes landing wait for the board: it never ends while
/// the board is down). Once the board is back, and the first comment write after the outage
/// lost its response, the board converges: the card in Done, one claim marker and one sticky
/// report holding the final state (`mutant_outbox_no_key` sends without the key: the replay
/// posts a second report).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ten_minute_outage_mid_item_ends_the_item_and_the_board_converges_once() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Outage", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    let queue = Faulty::new(&scene);
    let (task, watcher) = scene.start_with(None, queue.clone());

    wait_file(&scene.path("flag1")).await;
    drained(&scene).await;
    assert_eq!(card_now(&scene, 1).column, Column::InProgress);
    let ten = Duration::from_mins(10);
    let until = scene.clock.wall_ms() + i64::try_from(ten.as_millis()).unwrap();
    queue.down_until_ms.store(until, Ordering::SeqCst);
    queue.lose_next_comment.store(true, Ordering::SeqCst);
    std::fs::write(scene.path("release1"), "").unwrap();

    // The item ends while the board is still down: nothing waits for it.
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    assert!(scene.clock.wall_ms() < until, "the outage is still on");
    let waiting = outbox::pending(scene.store.reader(), "demo").await.unwrap();
    assert!(!waiting.is_empty(), "the board writes wait in the outbox");
    assert!(waiting[0].tries > 0, "{waiting:?}");
    assert_eq!(card_now(&scene, 1).column, Column::InProgress);

    // Ten minutes later the board is back, and catches up; the clock goes on, so the backoff
    // after the lost response runs out.
    scene.clock.advance(ten + Duration::from_secs(1));
    wait_for("the outbox to drain", Duration::from_secs(60), async || {
        scene.clock.advance(Duration::from_secs(30));
        outbox::pending(scene.store.reader(), "demo")
            .await
            .unwrap()
            .is_empty()
    })
    .await;
    // Give a duplicate time to show.
    tokio::time::sleep(Duration::from_millis(500)).await;
    task.abort();
    watcher.abort();
    assert!(
        !queue.lose_next_comment.load(Ordering::SeqCst),
        "a comment write lost its response"
    );
    let now = card_now(&scene, 1);
    assert_eq!(now.column, Column::Done);
    let daemons: Vec<&Comment> = now.comments.iter().filter(|c| c.key.is_some()).collect();
    assert_eq!(
        daemons.len(),
        2,
        "one claim marker and one report: {daemons:#?}"
    );
    let reports = keyed(&scene, 1, "report:");
    assert_eq!(reports.len(), 1, "{reports:#?}");
    assert_eq!(reports[0].key.as_deref(), Some(report_key(&id(1)).as_str()));
    assert!(
        reports[0].body.contains("Done: the verdict passed"),
        "{}",
        reports[0].body
    );
}

/// #65: a report send that failed is replayed after the item moved on: the replay carries the
/// newest report, rendered from the database when it is sent (`mutant_outbox_stored_body`
/// replays the body rendered for the first send: the board keeps a report of a running item).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_out_of_order_replay_leaves_the_newest_report() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Replayed", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let queue = Faulty::new(&scene);
    queue.hold_reports.store(true, Ordering::SeqCst);
    let (task, watcher) = scene.start_with(None, queue.clone());

    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    let waiting = outbox::pending(scene.store.reader(), "demo").await.unwrap();
    assert!(
        waiting.iter().any(|r| r.tries > 0),
        "the report was sent before: {waiting:?}"
    );
    queue.hold_reports.store(false, Ordering::SeqCst);
    scene.clock.advance(Duration::from_mins(2));
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let reports = keyed(&scene, 1, "report:");
    assert_eq!(reports.len(), 1, "{reports:#?}");
    assert!(
        reports[0].body.contains("Done: the verdict passed"),
        "an older report: {}",
        reports[0].body
    );
}

/// #65: each item has one sticky report, which holds the agent's summary; each blocking
/// question is its own comment, exactly once, also after a retry (the card moved from Needs you
/// to Ready) asks a second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn one_sticky_report_holds_the_summary_and_each_question_is_one_comment() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Asks twice", "test -f never.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("once {} write {} x", p("m1"), p("flag1")),
            format!("once {} wait {}", p("m2"), p("never")),
            format!("write {} x", p("flag2")),
            format!("wait {}", p("never")),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
    };
    let mut conn = crate::api::Connection::default();
    let call = async |conn: &mut crate::api::Connection,
                      credential: &str,
                      method: &str,
                      extra: serde_json::Value| {
        let mut params = serde_json::json!({"credential": credential, "item": "demo#1"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = rpc(&state, conn, method, params).await;
        assert!(response["result"].is_object(), "{method}: {response}");
    };

    // Attempt 1: a summary, a non-blocking question, then a blocking one.
    wait_file(&scene.path("flag1")).await;
    let first = latest_credential(&scene, 1).await;
    call(
        &mut conn,
        &first,
        "agent.submit_summary",
        serde_json::json!({"body": "Synthetic summary of the work.", "decisions": ["kept it small"]}),
    )
    .await;
    call(
        &mut conn,
        &first,
        "agent.ask_question",
        serde_json::json!({"text": "Is a later follow-up fine?", "blocking": false}),
    )
    .await;
    call(
        &mut conn,
        &first,
        "agent.ask_question",
        serde_json::json!({"text": "Which file format, A or B?", "blocking": true}),
    )
    .await;
    wait_for("needs you", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Question))
    })
    .await;
    drained(&scene).await;
    wait_for(
        "the Needs-you column",
        Duration::from_secs(30),
        async || card_now(&scene, 1).column == Column::NeedsYou,
    )
    .await;

    // The user answers in the card and retries: attempt 2 asks again.
    user_edits(&scene, 1, |c| {
        c.column = Column::Ready;
        c.body = c.body.replace("Item one.", "Item one. Format A.");
    });
    wait_file(&scene.path("flag2")).await;
    let second = latest_credential(&scene, 1).await;
    assert_ne!(first, second);
    call(
        &mut conn,
        &second,
        "agent.ask_question",
        serde_json::json!({"text": "And the encoding?", "blocking": true}),
    )
    .await;
    wait_for("needs you again", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Question))
            && scene.item(1).await.unwrap().attempts == 2
    })
    .await;
    drained(&scene).await;
    // Give a replay time to duplicate.
    tokio::time::sleep(Duration::from_millis(500)).await;
    task.abort();
    watcher.abort();

    let now = card_now(&scene, 1);
    let questions: Vec<&Comment> = now
        .comments
        .iter()
        .filter(|c| c.key.as_deref().is_some_and(|k| k.starts_with("demo#1/")))
        .collect();
    assert_eq!(questions.len(), 2, "{questions:#?}");
    assert!(
        questions[0].body.contains("Which file format"),
        "{}",
        questions[0].body
    );
    assert!(
        questions[1].body.contains("And the encoding"),
        "{}",
        questions[1].body
    );
    let reports = keyed(&scene, 1, "report:");
    assert_eq!(reports.len(), 1, "one sticky report: {reports:#?}");
    let report = &reports[0].body;
    assert!(
        report.contains("Needs you: the agent asked a blocking question"),
        "{report}"
    );
    let markers = keyed(&scene, 1, "claim:");
    assert_eq!(markers.len(), 1, "{markers:#?}");
}

/// #65: the sticky report of a Done item holds the agent's summary, marked as the agent's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sticky_report_of_a_done_item_holds_the_agents_summary() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Summarised", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    let credential = latest_credential(&scene, 1).await;
    let state = crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
    };
    let mut conn = crate::api::Connection::default();
    let response = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        serde_json::json!({
            "credential": credential,
            "item": "demo#1",
            "body": "Added the synthetic file.",
            "decisions": ["one file, no tests"],
            "followups": ["a later cleanup"],
        }),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    std::fs::write(scene.path("release1"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let reports = keyed(&scene, 1, "report:");
    assert_eq!(reports.len(), 1, "{reports:#?}");
    let report = &reports[0].body;
    for want in [
        "Done: the verdict passed",
        "Added the synthetic file.",
        "one file, no tests",
        "a later cleanup",
        "Written by the agent",
    ] {
        assert!(report.contains(want), "{want:?} missing from {report}");
    }
    // No comment but the claim marker and the report: Done notifies nobody.
    let daemons = card_now(&scene, 1)
        .comments
        .into_iter()
        .filter(|c| c.key.is_some())
        .count();
    assert_eq!(daemons, 2);
}

/// #65: the `restart` label added while the verdict runs kills the verdict, re-snapshots from
/// the base and starts again; the daemon removes the label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_restart_label_during_a_verdict_kills_resnapshots_and_restarts() {
    let scene = Scene::new().await;
    let count = sh_path(&scene.path("count"));
    let started = sh_path(&scene.path("verdict-started"));
    let never = sh_path(&scene.path("never"));
    // Run 1 is the base verdict at claim; run 2 (the first attempt's) blocks until killed.
    let verdict = format!(
        "n=$(cat {count} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count}; \
         if [ $n -eq 2 ]; then touch {started}; while [ ! -f {never} ]; do sleep 0.1; done; fi; \
         test -f one.txt"
    );
    scene.write_board(&[card(1, "Restarted", &verdict, "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);

    wait_file(&scene.path("verdict-started")).await;
    assert_eq!(scene.state(1).await, Some(State::Verifying));
    drained(&scene).await;
    let first_snapshot = scene
        .store
        .reader()
        .snapshot(&id(1))
        .await
        .unwrap()
        .unwrap()
        .key;
    user_edits(&scene, 1, |c| c.labels.push("restart".to_owned()));

    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.attempts, 2, "the restart is a new attempt");
    let transitions = scene.store.reader().transitions(&id(1)).await.unwrap();
    let causes: Vec<&str> = transitions.iter().map(|t| t.cause.as_str()).collect();
    assert_eq!(
        causes.iter().filter(|c| **c == "user_restart").count(),
        1,
        "{causes:?}"
    );
    let snapshot = scene
        .store
        .reader()
        .snapshot(&id(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(snapshot.key, first_snapshot, "re-snapshotted");
    // The killed verdict never passed; the new attempt's did.
    let verdicts = scene.store.reader().verdict_runs(&id(1)).await.unwrap();
    assert!(
        verdicts
            .iter()
            .all(|v| v.attempt == 2 || v.result != Some(bosun_core::RunResult::Pass)),
        "{verdicts:?}"
    );
    assert_eq!(
        std::fs::read_to_string(scene.path("count")).unwrap().trim(),
        "4"
    );
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 2);
    let now = card_now(&scene, 1);
    assert!(
        now.labels.is_empty(),
        "the label is removed: {:?}",
        now.labels
    );
    assert_eq!(now.column, Column::Done);
    // The first attempt's work was set aside before the restart.
    let aside = scene.git(&["for-each-ref", "--format=%(refname)", "refs/bosun/items/"]);
    assert!(aside.contains("refs/bosun/items/demo-1"), "{aside}");
}

/// #65: a Status change bosun did not make on a daemon-owned card is reverted, with one comment
/// however often the user repeats it, and the revert does not loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_status_change_is_reverted_with_one_comment_not_a_loop() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Moved", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    drained(&scene).await;

    for foreign in [Column::Verify, Column::NeedsYou] {
        user_edits(&scene, 1, |c| c.column = foreign);
        wait_for("the revert", Duration::from_secs(30), async || {
            card_now(&scene, 1).column == Column::InProgress
        })
        .await;
        drained(&scene).await;
    }
    // Nothing more happens: no loop of writes or comments.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let reverts = keyed(&scene, 1, "revert:");
    assert_eq!(reverts.len(), 1, "{reverts:#?}");
    assert!(reverts[0].body.contains("reverted"), "{}", reverts[0].body);
    assert_eq!(scene.state(1).await, Some(State::Running));
    assert_eq!(
        outbox::pending(scene.store.reader(), "demo").await.unwrap(),
        []
    );

    std::fs::write(scene.path("release1"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    assert_eq!(card_now(&scene, 1).column, Column::Done);
    assert_eq!(keyed(&scene, 1, "revert:").len(), 1);
}

/// #65: a card moved into Done by hand ends its item user-closed, its session killed; a card
/// moved to Backlog cancels its item.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manual_done_and_a_cancel_are_commands() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "Closed by hand", "test -f one.txt", "Item one."),
        card(2, "Cancelled", "test -f two.txt", "Item two."),
    ]);
    for n in [1, 2] {
        scene.script(
            n,
            &[
                format!("write {} x", p(&format!("flag{n}"))),
                format!("wait {}", p("never")),
            ],
        );
    }
    let (task, watcher) = scene.start(None);

    wait_file(&scene.path("flag1")).await;
    drained(&scene).await;
    user_edits(&scene, 1, |c| c.column = Column::Done);
    let state = ended(&scene, 1, Duration::from_secs(30)).await;
    assert_eq!(state, State::Done(Outcome::UserClosed));

    wait_file(&scene.path("flag2")).await;
    drained(&scene).await;
    user_edits(&scene, 2, |c| c.column = Column::Backlog);
    wait_for("the cancel", Duration::from_secs(30), async || {
        scene.state(2).await == Some(State::Cancelled)
    })
    .await;
    wait_for("the sessions' end", Duration::from_secs(30), async || {
        let mut ended = true;
        for n in [1, 2] {
            ended &= scene.runs(n).await.iter().all(|r| r.ended_ms.is_some());
        }
        ended
    })
    .await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    // The user's columns stay as the user left them.
    assert_eq!(card_now(&scene, 1).column, Column::Done);
    assert_eq!(card_now(&scene, 2).column, Column::Backlog);
    assert_eq!(scene.git(&["status", "--porcelain"]), "");
}

/// #65, #67: the kill switch thrown in the middle of a drain: the row being sent is the last;
/// nothing more reaches the board while it is thrown, and the rows wait in the outbox, to go
/// out once it is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_mid_drain_sends_no_further_row() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Killed", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    let queue = Faulty::new(&scene);
    let (task, watcher) = scene.start_with(None, queue.clone());
    wait_file(&scene.path("flag1")).await;
    drained(&scene).await;

    // The next write throws the kill switch; the session's exit queues more than one row.
    *queue.kill_after_write.lock().unwrap() = Some(scene.control.clone());
    std::fs::write(scene.path("release1"), "").unwrap();
    wait_for("the kill", Duration::from_secs(30), async || {
        scene.control.killed()
    })
    .await;
    let at_kill = queue.writes.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        queue.writes.load(Ordering::SeqCst),
        at_kill,
        "a board write after the kill"
    );
    assert!(
        !outbox::pending(scene.store.reader(), "demo")
            .await
            .unwrap()
            .is_empty(),
        "the rows wait in the outbox"
    );

    scene.control.set_killed(false);
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert!(queue.writes.load(Ordering::SeqCst) > at_kill);
}

/// #111: a card bounced by lint while the kill switch holds every board write leaves its
/// Needs-you comment in the outbox. The daemon restarts (a new loop, nothing in memory), the
/// switch is released, and the comment is posted with the findings of the bounce.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lint_bounce_pending_across_a_restart_is_posted_with_its_findings() {
    let scene = Scene::new().await;
    let mut bad = card(1, "Bad", "true", "Item one.");
    bad.body = "No contract block at all.".to_owned();
    scene.write_board(&[bad]);
    scene.control.set_killed(true);
    let (task, watcher) = scene.start(None);
    wait_for("the bounce", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Lint))
    })
    .await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    assert!(
        !outbox::pending(scene.store.reader(), "demo")
            .await
            .unwrap()
            .is_empty(),
        "the comment waits in the outbox"
    );
    assert_eq!(keyed(&scene, 1, "demo#1/"), Vec::<Comment>::new());

    // The daemon again: a fresh loop.
    let (task, watcher) = scene.start(None);
    scene.control.set_killed(false);
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let comments = keyed(&scene, 1, "demo#1/");
    assert_eq!(comments.len(), 1, "{comments:#?}");
    let body = &comments[0].body;
    assert!(body.contains("fails lint"), "{body}");
    assert!(
        body.contains("was not claimed.\n\n- "),
        "no findings: {body}"
    );
}
