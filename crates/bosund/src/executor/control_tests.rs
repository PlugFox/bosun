//! The scenarios of #67 against the loop: a pause, the kill switch, the item caps, the daily
//! cap and the automatic pause, over the same world as the loop's own suite.

use bosun_api::EventFilter;
use bosun_api::rpc::code;
use bosun_core::{Message, Origin, Tokens, Usage, WindowKey};
use bosun_store::{Repo as StoreRepo, SourceFile, Write};

use super::*;
use crate::events::{Batch, Subscriber};

/// The operator token of every scene.
pub(super) const TOKEN: &str = "bosun-operator-test-token";

/// How long the kill switch may take to leave no child (#67).
const KILL_LIMIT: Duration = Duration::from_secs(5);

/// A message of session `session` at `ts_ms` with `input` tokens of the reference model
/// (`input` eqv), in `cwd`.
fn message(n: u64, session: &str, ts_ms: i64, input: u64, cwd: &Path) -> Vec<Write> {
    let file = format!("file-{n}");
    vec![
        Write::SourceFile(SourceFile {
            provider: "claude".to_owned(),
            file_key: file.clone(),
            path: format!("projects/p/{file}.jsonl"),
            size: 100,
            parsed_offset: 100,
            parser_version: 1,
            last_line_hash: None,
            parsed_lines: Some(1),
            last_ts_ms: None,
        }),
        Write::Message {
            message: Message {
                provider: "claude".to_owned(),
                id: format!("msg-{n}"),
                origin: Origin {
                    ts_ms,
                    file,
                    window: WindowKey {
                        session_id: session.to_owned(),
                        agent_id: None,
                    },
                },
                model: bosun_core::DEFAULT_REFERENCE_MODEL.to_owned(),
                cwd: cwd.display().to_string(),
                usage: Usage {
                    tokens: Tokens {
                        input,
                        ..Tokens::default()
                    },
                    raw: serde_json::json!({}),
                },
                tool_calls: BTreeMap::new(),
            },
            parser_version: 1,
        },
    ]
}

impl Scene {
    /// Writes the configuration with `extra` appended (`[budget]`, `[daemon]`).
    fn configure(&self, extra: &str) {
        let text = format!("{}\n{extra}", self.config_text("'docs/frozen'"));
        std::fs::write(&self.config_path, text).unwrap();
    }

    /// Registers the checkout as a repository root of the store, as the ingest does at start.
    async fn register_root(&self) {
        let key = self.config().repos["demo"].key.clone();
        self.store
            .writer()
            .write(vec![Write::Repo(StoreRepo {
                path: key,
                remote: None,
                default_branch: None,
            })])
            .await
            .unwrap();
    }

    async fn usage(&self, n: u64, session: &str, input: u64) {
        let writes = message(n, session, self.clock.wall_ms(), input, &self.repo);
        self.store.writer().write(writes).await.unwrap();
    }

    fn subscribe(&self, kind: &str) -> Subscriber {
        let filter = EventFilter {
            kinds: Some(vec![kind.to_owned()]),
            tail: None,
        };
        self.events.subscribe("test", None, None, filter).1
    }
}

/// The bodies of the events `sub` has received so far.
fn bodies(sub: &mut Subscriber) -> Vec<serde_json::Value> {
    match sub.take(1_000) {
        Batch::Events(events) => events
            .iter()
            .map(|e| {
                let v: serde_json::Value = serde_json::from_slice(&e.line).unwrap();
                v["params"]["body"].clone()
            })
            .collect(),
        Batch::Lagged(_) => panic!("lagged"),
    }
}

/// The session's child, as the store recorded it, if it still runs.
async fn live_children(scene: &Scene) -> Vec<bosun_store::RecordedChild> {
    scene.store.reader().children().await.unwrap()
}

/// #67: a per-repository pause during an attempt lets it finish (its verdict runs, the item is
/// Done) and starts no new session until resume (`mutant_control_pause_kills` kills the
/// session: the item parks instead of finishing). The switches take the operator token: the
/// agent, calling `control.pause` with its own credential or with none, is refused
/// (`mutant_control_no_operator_token` lets it pause).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_lets_the_attempt_finish_and_starts_nothing_until_resume() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;

    // The agent opens its own connection: without the operator token it cannot pause.
    let credential = credential_of_session(&scene.path("work"));
    let state = crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
        leases: Arc::new(crate::leases::Leases::none()),
    };
    let pause = serde_json::json!({"identity": "test", "repo": "demo"});
    let mut agent_conn = crate::api::Connection::default();
    let hello = rpc(
        &state,
        &mut agent_conn,
        "agent.hello",
        serde_json::json!({"credential": credential, "item": "demo#1"}),
    )
    .await;
    assert!(hello["result"].is_object(), "{hello}");
    for (conn, params) in [
        (&mut agent_conn, pause.clone()),
        (&mut crate::api::Connection::default(), pause.clone()),
        (
            &mut crate::api::Connection::default(),
            serde_json::json!({"identity": "test", "repo": "demo", "operator": "bosun-operator-guess"}),
        ),
    ] {
        let refused = rpc(&state, conn, "control.pause", params).await;
        assert_eq!(
            error_code(&refused),
            Some(code::UNAUTHORIZED.into()),
            "{refused}"
        );
    }
    let killed = rpc(
        &state,
        &mut crate::api::Connection::default(),
        "control.kill",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        error_code(&killed),
        Some(code::UNAUTHORIZED.into()),
        "{killed}"
    );
    assert_eq!(
        scene.control.hold("test", "demo", 0),
        None,
        "nothing was paused"
    );
    assert!(!scene.control.killed());

    // The operator pauses the repository during the attempt.
    scene.control.pause("test", Some("demo"));
    std::fs::write(scene.path("release1"), "").unwrap();
    let one = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(one, State::Done(Outcome::Pass), "the attempt finished");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(scene.state(2).await.unwrap_or(State::Ready), State::Ready);

    scene
        .control
        .resume("test", Some("demo"), scene.clock.wall_ms());
    let two = ended(&scene, 2, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(two, State::Done(Outcome::Pass));
}

/// #67: the kill switch leaves no child within 5 s, parks the item, claims nothing more and
/// writes nothing further to the board; every board writer sees the stop
/// (`mutant_control_kill_keeps_writing` leaves the writes going). Released, the item resumes
/// its session and ends Done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kill_switch_leaves_no_child_and_writes_no_board() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("once {} write {} x", m("m1"), m("flag1")),
            // The first session waits (at most a minute) for a file nobody writes.
            format!("once {} wait {}", m("m2"), m("never")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    wait_for(
        "the session's child to be recorded",
        Duration::from_secs(30),
        async || !live_children(&scene).await.is_empty(),
    )
    .await;
    let child = live_children(&scene).await.remove(0);
    let key = bosun_proc::ProcessKey {
        pid: child.pid,
        start: bosun_proc::start_time(child.pid)
            .unwrap()
            .expect("the session runs"),
    };
    assert!(bosun_proc::is_running(key));
    // The writes queued before the kill (the claim's column and report) are sent first.
    drained(&scene).await;
    let board = std::fs::read_to_string(&scene.board).unwrap();

    let thrown = Instant::now();
    scene.control.set_killed(true);
    wait_for("no child", KILL_LIMIT, async || {
        !bosun_proc::is_running(key)
    })
    .await;
    assert!(thrown.elapsed() <= KILL_LIMIT);
    assert!(
        scene.control.board_writes_stopped(),
        "the outbox and the claim must see the stop"
    );
    wait_for("the item to park", Duration::from_secs(10), async || {
        matches!(
            scene.state(1).await,
            Some(State::Parked(bosun_core::Park {
                reason: ParkReason::Killed,
                ..
            }))
        )
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        std::fs::read_to_string(&scene.board).unwrap(),
        board,
        "the board was written after the kill switch"
    );
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(scene.state(2).await.unwrap_or(State::Ready), State::Ready);

    scene.control.set_killed(false);
    let one = ended(&scene, 1, Duration::from_secs(60)).await;
    let two = ended(&scene, 2, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(
        (one, two),
        (State::Done(Outcome::Pass), State::Done(Outcome::Pass))
    );
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        [(1, SpawnMode::Fresh), (1, SpawnMode::Resume)],
        "resumed after the release"
    );
}

/// #67: an item over its eqv cap and an item over its wall-clock cap are killed and go to
/// Needs you, each with its figure (`mutant_budget_ignore_item_cap` lets both run on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_over_its_eqv_or_wall_clock_cap_is_killed_and_needs_you() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.configure("[budget]\nitem_eqv = 1_000_000\nitem_wall_clock = '2h'\n");
    scene.write_board(&[
        card(1, "Expensive", "test -f one.txt", "Item one."),
        card(2, "Slow", "test -f two.txt", "Item two."),
    ]);
    for n in [1, 2] {
        scene.script(
            n,
            &[
                format!("write {} x", p(&format!("flag{n}"))),
                format!("wait {}", p(&format!("release{n}"))),
                format!("commit {n}.txt {n}"),
            ],
        );
    }
    let mut events = scene.subscribe("budget_exceeded");
    let (task, watcher) = scene.start(None);

    // Item 1's session spends 1.5 M eqv.
    wait_file(&scene.path("flag1")).await;
    let session = scene.runs(1).await.remove(0).session_key;
    scene.usage(1, &session, 1_500_000).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !terminal(scene.state(1).await) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A loop that ignores the cap gets its session released, and must not reach Done.
    std::fs::write(scene.path("release1"), "").unwrap();
    let one = ended(&scene, 1, Duration::from_secs(60)).await;

    // Item 2's session runs past two hours of the loop's clock.
    wait_file(&scene.path("flag2")).await;
    scene.clock.advance(Duration::from_mins(121));
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !terminal(scene.state(2).await) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(scene.path("release2"), "").unwrap();
    let two = ended(&scene, 2, Duration::from_secs(60)).await;
    // The board gets every row before the loop (and its drainer) stops.
    drained(&scene).await;
    task.abort();
    watcher.abort();

    assert_eq!(one, State::NeedsYou(Reason::Budget), "item 1");
    assert_eq!(two, State::NeedsYou(Reason::Budget), "item 2");
    let figures = bodies(&mut events);
    assert_eq!(figures.len(), 2, "{figures:?}");
    assert_eq!(figures[0]["cap"], "item_eqv");
    assert_eq!(figures[0]["item"], "demo#1");
    assert_eq!(figures[0]["used"], "1500000");
    assert_eq!(figures[0]["limit"], "1000000");
    assert_eq!(figures[1]["cap"], "item_wall_clock");
    assert_eq!(figures[1]["item"], "demo#2");
    let used: u64 = figures[1]["used"].as_str().unwrap().parse().unwrap();
    assert!(used >= 2 * 3_600_000, "{figures:?}");

    // #111: the Needs-you comment and the sticky report show the figure that tripped.
    for (n, figure) in [
        (1, "1,500,000 eqv of its 1,000,000 eqv cap"),
        (2, "active wall clock 121 min of its 120 min cap"),
    ] {
        let card = scene.cards().into_iter().find(|c| c.id.0 == n).unwrap();
        let keyed = |prefix: &str| -> Vec<String> {
            card.comments
                .iter()
                .filter(|c| c.key.as_deref().is_some_and(|k| k.starts_with(prefix)))
                .map(|c| c.body.clone())
                .collect()
        };
        let reports = keyed("report:");
        assert_eq!(reports.len(), 1, "item {n}: {:#?}", card.comments);
        assert!(reports[0].contains(figure), "item {n}: {}", reports[0]);
        let needs: Vec<String> = keyed(&format!("demo#{n}/"))
            .into_iter()
            .filter(|b| b.contains("needs you"))
            .collect();
        assert_eq!(needs.len(), 1, "item {n}: {:#?}", card.comments);
        assert!(needs[0].contains(figure), "item {n}: {}", needs[0]);
    }
}

/// #111: the sticky report of a finished item shows its cost, the eqv of its run sessions'
/// messages, and the model each attempt ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sticky_report_of_a_finished_item_shows_its_eqv_and_model() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Costed", "test -f one.txt", "Item one.")]);
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
    let session = scene.runs(1).await.remove(0).session_key;
    scene.usage(1, &session, 123_456).await;
    std::fs::write(scene.path("release1"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let card = scene.cards().into_iter().find(|c| c.id.0 == 1).unwrap();
    let report = card
        .comments
        .iter()
        .find(|c| c.key.as_deref() == Some("report:demo#1"))
        .map(|c| c.body.clone())
        .expect("the sticky report");
    for want in [
        "Cost: eqv 123,456; tokens 123,456 in".to_owned(),
        format!("model `{}`", bosun_core::DEFAULT_REFERENCE_MODEL),
    ] {
        assert!(report.contains(&want), "{want:?} missing from {report}");
    }
}

/// #67: reaching the daily cap (less the reserve) parks the queue; the loop's clock passing
/// local midnight releases it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daily_cap_parks_the_queue_until_local_midnight() {
    let scene = Scene::new().await;
    scene.configure("[budget]\ndaily_eqv = 2_000_000\nreserve_eqv = 500_000\n");
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    // Interactive work today: 1.6 M eqv, over the 1.5 M left to bosun.
    scene.usage(1, "interactive-session", 1_600_000).await;
    let mut events = scene.subscribe("budget_exceeded");
    let (task, watcher) = scene.start(None);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(scene.state(1).await.unwrap_or(State::Ready), State::Ready);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let now = scene.clock.wall_ms();
    let midnight = bosun_proc::next_local_midnight(now);
    assert_eq!(
        scene.control.hold("test", "demo", now),
        Some(bosun_core::Hold::DailyCap)
    );
    let figures = bodies(&mut events);
    assert_eq!(figures.len(), 1, "{figures:?}");
    assert_eq!(figures[0]["cap"], "daily_eqv");
    assert_eq!(figures[0]["used"], "1600000");
    assert_eq!(figures[0]["limit"], "1500000");
    assert_eq!(figures[0]["until_ms"], midnight);

    // Past local midnight: a new day, the queue runs.
    let to_midnight = u64::try_from(midnight - now).unwrap();
    scene
        .clock
        .advance(Duration::from_millis(to_midnight + 60_000));
    let one = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(one, State::Done(Outcome::Pass));
}

/// #67: an interactive transcript in the checkout pauses that repository only, until it has
/// been idle for `daemon.auto_pause_idle`; a session bosun spawned does not
/// (`mutant_control_autopause_own_sessions` pauses on the item's own session: item 2 is never
/// claimed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interactive_session_pauses_its_repository_and_a_run_session_does_not() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.register_root().await;
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
        card(3, "Three", "test -f three.txt", "Item three."),
    ]);
    for (n, file) in [(1, "one"), (2, "two")] {
        scene.script(
            n,
            &[
                format!("write {} x", p(&format!("flag{n}"))),
                format!("wait {}", p(&format!("release{n}"))),
                format!("commit {file}.txt {file}"),
            ],
        );
    }
    scene.script(3, &["commit three.txt three".to_owned()]);
    let (task, watcher) = scene.start(None);

    // Item 1's own session writes its transcript in the checkout: no pause.
    wait_file(&scene.path("flag1")).await;
    let session = scene.runs(1).await.remove(0).session_key;
    scene.usage(1, &session, 10).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(scene.path("release1"), "").unwrap();
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(60)).await,
        State::Done(Outcome::Pass)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !scene.path("flag2").exists() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        scene.path("flag2").exists(),
        "a run session paused its own repository"
    );

    // The user starts an interactive session in the checkout during item 2.
    scene.usage(2, "interactive-session", 10).await;
    let now = scene.clock.wall_ms();
    wait_for("the automatic pause", Duration::from_secs(10), async || {
        scene.control.hold("test", "demo", now) == Some(bosun_core::Hold::Pause)
    })
    .await;
    assert_eq!(
        scene.control.hold("test", "elsewhere", now),
        None,
        "another repository is not paused"
    );
    std::fs::write(scene.path("release2"), "").unwrap();
    assert_eq!(
        ended(&scene, 2, Duration::from_secs(60)).await,
        State::Done(Outcome::Pass),
        "the attempt in flight finishes"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(scene.state(3).await.unwrap_or(State::Ready), State::Ready);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 2);

    // Idle for the default 30 minutes: the pause lifts by itself.
    scene.clock.advance(Duration::from_mins(31));
    let three = ended(&scene, 3, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(three, State::Done(Outcome::Pass));
}
