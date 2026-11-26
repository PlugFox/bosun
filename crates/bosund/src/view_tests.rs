//! The read methods and restart-item of #70 over a served state: two identities, each with a
//! local-file board, their stores written as the loop writes them; and the same methods on a
//! fake Projects v2 board (#111).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bosun_api::rpc::code;
use bosun_config::SystemProbe;
use bosun_core::{Ctx, Event, Item, ItemId, SpawnMode, Stepped, step};
use bosun_queue::{Card, Column, LocalFile, Queue};
use bosun_store::{NewItem, RunStart, Snapshot, Write};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::api::{Answer, Connection, handle_line};
use crate::state::{self, State};

const TOKEN: &str = "bosun-operator-view-tests";
const T0: i64 = 1_800_000_000_000;

fn body(verdict: Option<&str>, prose: &str) -> String {
    match verdict {
        Some(v) => format!("{prose}\n\n```bosun\nverdict = \"{v}\"\n```\n"),
        None => prose.to_owned(),
    }
}

fn card(id: u64, column: Column, verdict: Option<&str>, prose: &str) -> Card {
    Card {
        id: bosun_queue::ItemId(id),
        title: format!("Card {id}"),
        column,
        body: body(verdict, prose),
        labels: Vec::new(),
        draft: false,
        closed: false,
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

struct World {
    dir: TempDir,
    state: State,
}

/// A machine-scope device: one holder at a time, shared by every identity.
const RIG: &str = "[resource.rig]\nkind = 'device'\n";

impl World {
    async fn new() -> Self {
        Self::with_resources("").await
    }

    /// A world whose configuration also has `resources` (TOML): its lease manager has them,
    /// journalled in the world's directory. With none, it has a manager of no resource.
    async fn with_resources(resources: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut config = String::new();
        for (identity, repo) in [("a", "alpha"), ("b", "beta")] {
            let claude = d.join(identity).join("claude");
            std::fs::create_dir_all(claude.join("projects")).unwrap();
            std::fs::create_dir_all(d.join(repo)).unwrap();
            let _ = write!(
                config,
                "[identity.{identity}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\
                 author_allowlist = ['me']\n\n\
                 [repo.{repo}]\npath = '{}'\nidentity = '{identity}'\n\
                 queue = {{ kind = 'local_file', file = '{}' }}\n\
                 model = 'opus'\n\n",
                claude.display(),
                d.join(identity).join("bosun.db").display(),
                d.join(repo).display(),
                d.join(format!("{repo}.toml")).display(),
            );
        }
        config.push_str(resources);
        let path = d.join("config.toml");
        std::fs::write(&path, config).unwrap();
        let config = bosun_config::load(&path, &SystemProbe::new()).unwrap();
        let identities = state::open(&config, "view-epoch", crate::events::Options::default())
            .await
            .map_err(|(_, e)| e)
            .unwrap();
        let leases = if resources.is_empty() {
            crate::leases::Leases::none()
        } else {
            let journal = crate::leases::journal_path(&d.join("state").join("bosun.token"));
            crate::leases::Leases::new(&config, &journal).unwrap()
        };
        let world = Self {
            state: State {
                started_ms: T0,
                identities,
                credentials: crate::agent::Credentials::default(),
                control: crate::control::Control::with_token(TOKEN, None),
                leases: std::sync::Arc::new(leases),
                gh: std::path::PathBuf::from("gh"),
            },
            dir,
        };
        world.board(
            "alpha",
            &[
                card(1, Column::Ready, Some("true"), "Good."),
                card(2, Column::Ready, None, "No contract block."),
                card(3, Column::InProgress, Some("true"), "In flight."),
            ],
        );
        world.board(
            "beta",
            &[card(7, Column::Ready, Some("true"), "Beta's own.")],
        );
        world
    }

    fn board_path(&self, repo: &str) -> PathBuf {
        self.dir.path().join(format!("{repo}.toml"))
    }

    fn board(&self, repo: &str, cards: &[Card]) {
        std::fs::write(self.board_path(repo), LocalFile::render(cards)).unwrap();
    }

    fn cards(&self, repo: &str) -> Vec<Card> {
        LocalFile::new(self.board_path(repo))
            .poll(None)
            .unwrap()
            .cards
    }

    /// Writes alpha#3 as the loop does: claimed, a snapshot of its card, running with a
    /// session.
    async fn claim_three(&self) -> String {
        let served = &self.state.identities["a"];
        let id = ItemId::new("alpha#3");
        let go =
            |item: &Item, event: &Event| -> Stepped { step(item, event, &Ctx::at(T0)).unwrap() };
        let claimed = go(&Item::new(id.clone()), &Event::Claim);
        let running = go(&claimed.item, &Event::ClaimConfirmed);
        let card = self
            .cards("alpha")
            .into_iter()
            .find(|c| c.id.0 == 3)
            .unwrap();
        let snapshot = bosun_queue::Snapshot::take(&card, |c| c.author == "me").unwrap();
        let transition = |stepped: &Stepped| Write::Transition {
            stepped: stepped.clone(),
            at_ms: T0 + i64::from(stepped.transition.key.seq),
        };
        served
            .store
            .writer()
            .write(vec![
                Write::Item(NewItem {
                    item: id.clone(),
                    name: served.repos[0].0.clone(),
                    repo: served.repos[0].1.key.clone(),
                    card_key: Some("3".to_owned()),
                    at_ms: T0,
                }),
                transition(&claimed),
                Write::Snapshot(Snapshot {
                    item: id.clone(),
                    key: "alpha#3/claim".to_owned(),
                    taken_ms: T0,
                    hash: snapshot.hash.clone(),
                    body: snapshot.body,
                    contract_json: "{}".to_owned(),
                    comments_json: "[]".to_owned(),
                    config_hash: None,
                    policy_hash: None,
                    base_sha: Some("base".to_owned()),
                    branch: Some("main".to_owned()),
                    base_refs_json: None,
                    base_stash_json: None,
                }),
                transition(&running),
                Write::Run(RunStart {
                    item: id,
                    attempt: 1,
                    spawn: 1,
                    mode: SpawnMode::Fresh,
                    provider: "claude".to_owned(),
                    session_key: "session-3".to_owned(),
                    started_ms: T0 + 10,
                    start_sha: None,
                }),
            ])
            .await
            .unwrap();
        snapshot.hash
    }

    /// Calls `method` through the daemon's line handler.
    async fn call(&self, method: &str, params: Value) -> Result<Value, Value> {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        match handle_line(&self.state, &mut Connection::default(), &line.to_string()).await {
            Answer::Reply(Some(response)) => {
                let response = serde_json::to_value(response).unwrap();
                match response.get("result") {
                    Some(result) => Ok(result.clone()),
                    None => Err(response["error"].clone()),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{method} {params}: {e}"))
    }

    async fn refused(&self, method: &str, params: Value) -> i64 {
        let error = self.call(method, params).await.unwrap_err();
        error["code"].as_i64().unwrap()
    }

    async fn close(self) {
        state::shut_down(self.state.identities).await;
    }
}

fn repo_names(view: &Value) -> Vec<&str> {
    view["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["repo"].as_str().unwrap())
        .collect()
}

/// #70: the queue of an identity holds its own repositories only; another identity's is
/// refused by name, for the board and for the lint (`mutant_api_queue_other_identity` lets
/// identity `a` read `beta`).
#[tokio::test]
async fn a_queue_holds_only_the_identitys_repositories() {
    let world = World::new().await;
    let view = world.ok("queue.get", json!({"identity": "a"})).await;
    assert_eq!(repo_names(&view), ["alpha"]);
    assert!(!view.to_string().contains("Beta's own"), "{view}");
    assert_eq!(
        world
            .refused("queue.get", json!({"identity": "a", "repo": "beta"}))
            .await,
        i64::from(code::INVALID_PARAMS)
    );
    assert_eq!(
        world
            .refused(
                "queue.lint",
                json!({"identity": "a", "repo": "beta", "card": 7})
            )
            .await,
        i64::from(code::INVALID_PARAMS)
    );
    let view = world.ok("queue.get", json!({"identity": "b"})).await;
    assert_eq!(repo_names(&view), ["beta"]);
    world.close().await;
}

fn card_of<'a>(view: &'a Value, column: &str, card: u64) -> &'a Value {
    view["repos"][0]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["column"] == column)
        .and_then(|c| {
            c["cards"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["card"] == card)
        })
        .unwrap_or_else(|| panic!("no card {card} in {column}: {view}"))
}

/// The board in every column with each card's state, snapshot hash, pending edit and the lint
/// of Ready cards; the lint of one card; the item's detail and session; the Overview.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_read_methods_show_the_board_the_item_and_its_session() {
    let world = World::new().await;
    let hash = world.claim_three().await;

    let view = world
        .ok("queue.get", json!({"identity": "a", "repo": "alpha"}))
        .await;
    let columns: Vec<&str> = view["repos"][0]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["column"].as_str().unwrap())
        .collect();
    assert_eq!(
        columns,
        [
            "Backlog",
            "Ready",
            "In progress",
            "Verify",
            "Done",
            "Needs you"
        ]
    );
    assert!(view["repos"][0]["hash"].is_string());
    let three = card_of(&view, "In progress", 3);
    assert_eq!(three["state"], "running");
    assert_eq!(three["snapshot_hash"], hash.as_str());
    assert_eq!(three["edit_pending"], false);
    assert_eq!(card_of(&view, "Ready", 1)["lint"], json!([]));
    let bad = &card_of(&view, "Ready", 2)["lint"];
    assert_eq!(bad[0]["severity"], "error", "{bad}");

    // An edit of the card in flight waits for its next claim.
    let mut cards = world.cards("alpha");
    cards[2].body = body(Some("true"), "In flight, edited.");
    world.board("alpha", &cards);
    let view = world.ok("queue.get", json!({"identity": "a"})).await;
    assert_eq!(card_of(&view, "In progress", 3)["edit_pending"], true);

    let lint = world
        .ok(
            "queue.lint",
            json!({"identity": "a", "repo": "alpha", "card": 2}),
        )
        .await;
    assert_eq!(lint["claimable"], false);
    assert_eq!(lint["item"], "alpha#2");
    assert_eq!(
        world
            .refused(
                "queue.lint",
                json!({"identity": "a", "repo": "alpha", "card": 99})
            )
            .await,
        i64::from(code::NOT_FOUND)
    );

    let item = world
        .ok("item.get", json!({"identity": "a", "item": "alpha#3"}))
        .await;
    assert_eq!(item["state"], "running");
    assert_eq!(item["attempt"], 1);
    assert_eq!(item["snapshot"]["hash"], hash.as_str());
    assert_eq!(item["transitions"].as_array().unwrap().len(), 2);
    assert_eq!(item["attempts"][0]["sessions"][0]["session"], "session-3");
    // Another identity's store does not hold it.
    assert_eq!(
        world
            .refused("item.get", json!({"identity": "b", "item": "alpha#3"}))
            .await,
        i64::from(code::NOT_FOUND)
    );

    for ask in [
        json!({"identity": "a", "item": "alpha#3"}),
        json!({"identity": "a", "session": "session-3"}),
    ] {
        let session = world.ok("session.get", ask).await;
        assert_eq!(session["running"], true);
        assert_eq!(
            (session["attempt"].clone(), session["spawn"].clone()),
            (json!(1), json!(1))
        );
    }
    assert_eq!(
        world
            .refused(
                "session.get",
                json!({"identity": "b", "session": "session-3"})
            )
            .await,
        i64::from(code::NOT_FOUND)
    );
    assert_eq!(
        world.refused("session.get", json!({"identity": "a"})).await,
        i64::from(code::INVALID_PARAMS)
    );

    let overview = world.ok("status.overview", json!({"identity": "a"})).await;
    let alpha = &overview["repos"][0];
    assert_eq!(alpha["state"], "working");
    assert_eq!(alpha["current"]["item"], "alpha#3");
    assert_eq!(alpha["current"]["attempt"], 1);
    assert!(alpha.get("hold").is_none(), "{alpha}");
    assert_eq!(overview["events"]["epoch"], "view-epoch");
    world.state.control.pause("a", Some("alpha"));
    let overview = world.ok("status.overview", json!({"identity": "a"})).await;
    assert_eq!(overview["repos"][0]["hold"], "pause");
    let beta = world.ok("status.overview", json!({"identity": "b"})).await;
    assert_eq!(beta["repos"][0]["state"], "idle");
    world.close().await;
}

fn labels(path: &Path, card: u64) -> Vec<String> {
    LocalFile::new(path)
        .lint_target(bosun_queue::ItemId(card))
        .unwrap()
        .labels
}

/// restart-item takes the operator token, restarts only an item in flight, puts the `restart`
/// label on its card, and writes nothing while the kill switch is thrown.
#[tokio::test]
async fn restart_item_puts_the_label_under_the_operator_token() {
    let world = World::new().await;
    world.claim_three().await;
    let ask =
        |operator: &str, item: &str| json!({"operator": operator, "identity": "a", "item": item});
    assert_eq!(
        world
            .refused("item.restart", json!({"identity": "a", "item": "alpha#3"}))
            .await,
        i64::from(code::UNAUTHORIZED)
    );
    assert_eq!(
        world
            .refused("item.restart", ask("forged", "alpha#3"))
            .await,
        i64::from(code::UNAUTHORIZED)
    );
    assert_eq!(labels(&world.board_path("alpha"), 3), Vec::<String>::new());

    world.state.control.set_killed(true);
    assert_eq!(
        world.refused("item.restart", ask(TOKEN, "alpha#3")).await,
        i64::from(code::FAILED)
    );
    assert_eq!(labels(&world.board_path("alpha"), 3), Vec::<String>::new());
    world.state.control.set_killed(false);

    let result = world.ok("item.restart", ask(TOKEN, "alpha#3")).await;
    assert_eq!(result, json!({"item": "alpha#3", "label": "restart"}));
    assert_eq!(labels(&world.board_path("alpha"), 3), ["restart"]);

    // Not recorded, or not in flight: nothing to restart.
    assert_eq!(
        world.refused("item.restart", ask(TOKEN, "alpha#1")).await,
        i64::from(code::NOT_FOUND)
    );
    assert_eq!(
        world
            .refused(
                "item.restart",
                json!({"operator": TOKEN, "identity": "b", "item": "alpha#3"})
            )
            .await,
        i64::from(code::NOT_FOUND)
    );
    world.close().await;
}

/// The repository the fake Projects v2 board of `alpha` holds the issues of.
const FAKE_REPO: &str = "synthetic-owner/alpha";

/// #111: `queue.get`, `queue.lint` and `item.restart` on a Projects v2 board, through the
/// backend the loop uses (connected on first use, here the fake GitHub): the board in its
/// columns with the lint of Ready cards, one card's lint, and the `restart` label put on the
/// issue of the item in flight.
#[tokio::test]
async fn the_queue_methods_and_restart_item_work_on_a_projects_v2_board() {
    use std::sync::Arc;

    use bosun_queue::ProjectsV2;
    use bosun_queue::projects_v2::fake::{FakeGitHub, FakeItem};
    use bosun_queue::projects_v2::{Binding, ProjectRef};

    use crate::executor::board::{Board, Lazy, Shared, Visibility};

    let mut world = World::new().await;
    world.claim_three().await;
    let fake = Arc::new(FakeGitHub::new());
    fake.add_project(
        "synthetic-owner",
        1,
        &Column::ALL.map(bosun_queue::Column::name),
    );
    let cards = [
        card(1, Column::Ready, Some("true"), "Good."),
        card(2, Column::Ready, None, "No contract block."),
        card(3, Column::InProgress, Some("true"), "In flight."),
    ];
    for c in &cards {
        fake.issue(FAKE_REPO, c.id.0, |issue| {
            issue.title = format!("{} on GitHub", c.title);
            issue.body.clone_from(&c.body);
            issue.author = Some("me".to_owned());
        });
    }
    let items: Vec<FakeItem<'_>> = cards
        .iter()
        .map(|c| FakeItem::Issue(FAKE_REPO, c.id.0, Some(c.column.name())))
        .collect();
    fake.set_items(0, &items);
    fake.define_label(FAKE_REPO, "restart");
    let connect = fake.clone();
    let queue = Lazy::new(Box::new(move || {
        let binding = Binding::new(FAKE_REPO, [ProjectRef::new("synthetic-owner", 1)], ["me"]);
        Ok(Arc::new(ProjectsV2::connect(connect.clone(), binding)?) as Shared)
    }));
    let board = Board {
        queue: Arc::new(queue),
        idle_backoff: true,
        visibility: Arc::new(Visibility::fixed(false)),
    };
    world
        .state
        .identities
        .get_mut("a")
        .unwrap()
        .boards
        .insert("alpha".to_owned(), Ok(board));

    let view = world
        .ok("queue.get", json!({"identity": "a", "repo": "alpha"}))
        .await;
    assert!(view["repos"][0]["error"].is_null(), "{view}");
    assert_eq!(card_of(&view, "Ready", 1)["title"], "Card 1 on GitHub");
    assert_eq!(card_of(&view, "In progress", 3)["state"], "running");
    assert_eq!(card_of(&view, "Ready", 1)["lint"], json!([]));
    assert_eq!(card_of(&view, "Ready", 2)["lint"][0]["severity"], "error");

    let lint = world
        .ok(
            "queue.lint",
            json!({"identity": "a", "repo": "alpha", "card": 2}),
        )
        .await;
    assert_eq!(lint["claimable"], false, "{lint}");
    let lint = world
        .ok(
            "queue.lint",
            json!({"identity": "a", "repo": "alpha", "card": 1}),
        )
        .await;
    assert_eq!(lint["claimable"], true, "{lint}");

    let ask = json!({"operator": TOKEN, "identity": "a", "item": "alpha#3"});
    let result = world.ok("item.restart", ask).await;
    assert_eq!(result, json!({"item": "alpha#3", "label": "restart"}));
    assert_eq!(fake.issue_now(FAKE_REPO, 3).labels, ["restart"]);
    // The local file of the configuration is not the board any more.
    assert_eq!(labels(&world.board_path("alpha"), 3), Vec::<String>::new());
    assert_eq!(
        fake.count("Resolve"),
        1,
        "connected once: {:?}",
        fake.requests()
    );
    world.close().await;
}
/// A stored message of window `session` at `ts_ms`: `input` uncached and `cached` read from
/// the cache, of the reference model (one eqv per uncached input token).
fn message(n: u64, session: &str, ts_ms: i64, input: u64, cached: u64) -> Vec<Write> {
    let file = format!("file-{n}");
    vec![
        Write::SourceFile(bosun_store::SourceFile {
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
            message: bosun_core::Message {
                provider: "claude".to_owned(),
                id: format!("msg-{n}"),
                origin: bosun_core::Origin {
                    ts_ms,
                    file,
                    window: bosun_core::WindowKey {
                        session_id: session.to_owned(),
                        agent_id: None,
                    },
                },
                model: bosun_core::DEFAULT_REFERENCE_MODEL.to_owned(),
                cwd: "/elsewhere".to_owned(),
                usage: bosun_core::Usage {
                    tokens: bosun_core::Tokens {
                        input,
                        cache_read: cached,
                        ..bosun_core::Tokens::default()
                    },
                    raw: json!({}),
                },
                tool_calls: std::collections::BTreeMap::new(),
            },
            parser_version: 1,
        },
    ]
}

fn output(item: &str, line: &str, tool: Option<&str>) -> bosun_api::EventBody {
    bosun_api::EventBody::SessionOutput {
        item: item.to_owned(),
        attempt: 1,
        spawn: 1,
        stream: bosun_api::OutputStream::Stdout,
        line: line.to_owned(),
        tool: tool.map(ToOwned::to_owned),
    }
}

/// #74: `session.tail` returns the item's own output lines the ring holds, from `from_seq`
/// on, with its latest session and the context size of that session's last message.
#[tokio::test]
async fn session_tail_returns_the_items_held_lines_and_its_context() {
    let world = World::new().await;
    world.claim_three().await;
    let served = &world.state.identities["a"];
    served.events.publish(output("alpha#3", "started", None));
    served
        .events
        .publish(output("alpha#9", "another item", None));
    served
        .events
        .publish(output("alpha#3", "tool: Read", Some("Read")));
    let ask = json!({"identity": "a", "item": "alpha#3"});
    let tail = world.ok("session.tail", ask.clone()).await;
    let lines: Vec<&str> = tail["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"]["line"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["started", "tool: Read"], "{tail}");
    assert_eq!(tail["lines"][1]["body"]["tool"], "Read");
    assert_eq!(tail["lines"][1]["seq"], 3);
    assert_eq!(tail["head"], 3);
    assert_eq!(tail["epoch"], "view-epoch");
    assert_eq!(tail["gap"], false);
    assert_eq!(tail["session"]["session"], "session-3");
    assert!(tail.get("context").is_none(), "{tail}");

    let from = world
        .ok(
            "session.tail",
            json!({"identity": "a", "item": "alpha#3", "from_seq": 2}),
        )
        .await;
    assert_eq!(from["lines"].as_array().unwrap().len(), 1, "{from}");

    // The last message of the session's main transcript is its context size.
    let mut writes = message(1, "session-3", T0 + 20, 1_000, 60_000);
    writes.extend(message(2, "session-3", T0 + 30, 500, 90_000));
    writes.extend(message(3, "session-3", T0 + 40, 10, 80_000));
    served.store.writer().write(writes).await.unwrap();
    let tail = world.ok("session.tail", ask).await;
    assert_eq!(tail["context"]["prompt"], 80_010, "{tail}");
    assert_eq!(tail["context"]["peak"], 90_500);
    assert_eq!(tail["context"]["window"], 200_000);

    assert_eq!(
        world
            .refused("session.tail", json!({"identity": "b", "item": "alpha#3"}))
            .await,
        i64::from(code::NOT_FOUND)
    );
    world.close().await;
}

/// #74: `tokens.pace` puts the identity's recent eqv in its buckets and odometers, and its
/// reference is the configured budget's, named so; without a daily budget there is none.
#[tokio::test]
async fn tokens_pace_measures_against_the_configured_budget() {
    let mut world = World::new().await;
    let now = crate::overview::now_ms();
    world.state.started_ms = now - 3_600_000;
    let ask = json!({"identity": "a"});
    let pace = world.ok("tokens.pace", ask.clone()).await;
    assert!(pace.get("reference").is_none(), "{pace}");
    assert_eq!(
        pace["buckets"].as_array().unwrap().len(),
        crate::pace::BUCKETS
    );
    assert_eq!(pace["bucket_ms"], crate::pace::BUCKET_MS);

    let served = &world.state.identities["a"];
    let mut writes = message(1, "s", now - 30_000, 1_000, 0);
    // Before the run, and so before the last hour too.
    writes.extend(message(2, "s", now - 2 * 3_600_000, 7_000, 0));
    served.store.writer().write(writes).await.unwrap();
    // Identity b's messages are b's alone.
    world.state.identities["b"]
        .store
        .writer()
        .write(message(3, "t", now - 10_000, 50_000, 0))
        .await
        .unwrap();
    {
        let mut budget = served.budget.lock().unwrap();
        budget.daily_eqv = Some(2_000_000);
        budget.reserve_eqv = 500_000;
    }
    let pace = world.ok("tokens.pace", ask).await;
    let buckets: Vec<f64> = pace["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_f64().unwrap())
        .collect();
    assert!(
        (buckets.iter().sum::<f64>() - 1_000.0).abs() < 1e-6,
        "{pace}"
    );
    assert!(
        (buckets[buckets.len() - 1] - 1_000.0).abs() < 1e-6,
        "{pace}"
    );
    assert!((pace["run_eqv"].as_f64().unwrap() - 1_000.0).abs() < 1e-6);
    let today = pace["today_eqv"].as_f64().unwrap();
    let reference = &pace["reference"];
    assert_eq!(reference["type"], "configured", "{pace}");
    assert_eq!(reference["cap_eqv"], "1500000");
    let left_ms = pace["reset_ms"].as_i64().unwrap() - now;
    let minutes = f64::from(i32::try_from(left_ms / 1000).unwrap()) / 60.0;
    let want = (1_500_000.0 - today) / minutes;
    let got = reference["eqv_per_minute"].as_f64().unwrap();
    assert!((got - want).abs() / want < 0.01, "{got} vs {want}");
    world.close().await;
}

/// alpha#3's request for `rig`, as its loop files it: of repository `alpha` of identity `a`.
fn rig_for_three() -> crate::leases::Request {
    crate::leases::Request {
        holder: crate::leases::Holder::Item(ItemId::new("alpha#3")),
        names: vec!["rig".to_owned()],
        repo: "alpha".to_owned(),
        identity: "a".to_owned(),
        writer: None,
    }
}

/// #8: `lease.list` shows each identity the machine-scope `rig` with its holder, named to the
/// identity whose item holds it and only `busy` to the other, whose answer holds neither the
/// item's id nor its repository's name; the worktree of `a` is listed for `a` alone.
/// `mutant_api_lease_holder_across_identities` names the holder to both.
#[tokio::test]
async fn the_resource_list_shows_another_identitys_holder_only_as_busy() {
    let world = World::with_resources(RIG).await;
    world.claim_three().await;
    let leases = world.state.leases.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        leases.acquire(rig_for_three()),
    )
    .await
    .expect("rig is free")
    .unwrap();
    let path = world.dir.path().join("worktrees").join("three");
    world.state.identities["a"]
        .store
        .writer()
        .write(vec![
            Write::Worktree(bosun_store::NewWorktree {
                item: ItemId::new("alpha#3"),
                path: path.display().to_string(),
                branch: "bosun/alpha-3".to_owned(),
                base_sha: "base".to_owned(),
                kind: bosun_store::WorktreeKind::Item,
                user_base_json: None,
                created_ms: T0,
            }),
            Write::WorktreeBytes {
                path: path.display().to_string(),
                bytes: 4096,
            },
        ])
        .await
        .unwrap();

    let mine = world.ok("lease.list", json!({"identity": "a"})).await;
    assert_eq!(
        mine["resources"],
        json!([{
            "name": "rig",
            "kind": "device",
            "capacity": 1,
            "scope": "machine",
            "holders": ["item alpha#3"],
            "waiting": 0,
        }]),
        "{mine}"
    );
    assert_eq!(mine["worktrees"].as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(mine["worktrees"][0]["item"], "alpha#3");
    assert_eq!(mine["worktrees"][0]["state"], "created");
    assert_eq!(mine["worktrees"][0]["bytes"], 4096);

    let theirs = world.ok("lease.list", json!({"identity": "b"})).await;
    assert_eq!(theirs["resources"][0]["name"], "rig", "{theirs}");
    assert_eq!(
        theirs["resources"][0]["holders"],
        json!(["busy"]),
        "{theirs}"
    );
    assert_eq!(theirs["worktrees"], json!([]), "{theirs}");
    let text = theirs.to_string();
    assert!(!text.contains("alpha#3"), "{text}");
    assert!(!text.contains("alpha"), "{text}");

    leases.release(&rig_for_three().holder).await;
    world.close().await;
}

/// The `waiting` of alpha#3 in `status.overview` and `item.get` for identity `a`, once its
/// request waits (within 5 s).
async fn waiting_of_three(world: &World) -> (Value, Value) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let overview = world.ok("status.overview", json!({"identity": "a"})).await;
        let waiting = overview["repos"][0]["current"]["waiting"].clone();
        if !waiting.is_null() || std::time::Instant::now() > deadline {
            let item = world
                .ok("item.get", json!({"identity": "a", "item": "alpha#3"}))
                .await;
            return (waiting, item["waiting"].clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// #8: while alpha#3's request for `rig` waits behind a `bosun with` command (this test's own
/// process), `status.overview` and `item.get` say it waits for `rig` and that the command
/// holds it, when the command named identity `a`; when it named `b`, the holder is `busy` and
/// its pid is not shown. Once the request is withdrawn, neither says it waits.
/// `mutant_api_wait_unnamed` leaves `waiting` out.
#[tokio::test]
async fn a_waiting_item_names_the_resource_and_its_holder() {
    let world = World::with_resources(RIG).await;
    world.claim_three().await;
    let pid = std::process::id();
    for (named, shown) in [("a", format!("bosun with {pid}")), ("b", "busy".to_owned())] {
        let granted = world
            .ok(
                "lease.acquire",
                json!({"operator": TOKEN, "identity": named, "resources": ["rig"], "pid": pid}),
            )
            .await;
        assert_eq!(granted, json!({"granted": true}), "{named}");
        let leases = world.state.leases.clone();
        let asked = tokio::spawn(async move { leases.acquire(rig_for_three()).await });

        let (overview, item) = waiting_of_three(&world).await;
        let want = json!([{"resource": "rig", "holders": [shown]}]);
        assert_eq!(overview, want, "command named {named}: overview");
        assert_eq!(item, want, "command named {named}: item");
        if named == "b" {
            for waiting in [&overview, &item] {
                assert!(!waiting.to_string().contains(&pid.to_string()), "{waiting}");
            }
        }

        // The request is withdrawn with its future; the command's lease is released.
        asked.abort();
        assert!(asked.await.unwrap_err().is_cancelled());
        let overview = world.ok("status.overview", json!({"identity": "a"})).await;
        assert!(
            overview["repos"][0]["current"].get("waiting").is_none(),
            "{overview}"
        );
        let released = world
            .ok("lease.release", json!({"operator": TOKEN, "pid": pid}))
            .await;
        assert_eq!(released, json!({"released": true}));
    }
    world.close().await;
}

/// Writes a stand-in for `gh` into `dir` that prints `line` (to standard error when `fail`,
/// then exits 3; else to standard output, then exits 0); its path.
fn stand_in_gh(dir: &Path, name: &str, line: &str, fail: bool) -> PathBuf {
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    if cfg!(windows) {
        let path = dir.join("gh.cmd");
        let (redirect, exit) = if fail { (" 1>&2", 3) } else { ("", 0) };
        std::fs::write(
            &path,
            format!("@echo off\r\necho {line}{redirect}\r\nexit /b {exit}\r\n"),
        )
        .unwrap();
        path
    } else {
        let path = dir.join("gh");
        let (redirect, exit) = if fail { (" 1>&2", 3) } else { ("", 0) };
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho '{line}'{redirect}\nexit {exit}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// #131: `doctor.probe` starts the daemon's `gh` and says whether it ran, with why not when it
/// did not; only an identity the daemon does not serve is an error
/// (`mutant_api_probe_always_ok` reports the failing stand-in as ok).
#[tokio::test]
async fn the_doctor_probe_fails_when_the_daemon_cannot_start_gh() {
    async fn probe(world: &World) -> Value {
        world.ok("doctor.probe", json!({"identity": "a"})).await
    }
    let mut world = World::new().await;

    world.state.gh = stand_in_gh(
        world.dir.path(),
        "failing",
        "the stand-in cannot start",
        true,
    );
    let failed = probe(&world).await;
    assert_eq!(failed["identity"], "a");
    assert_eq!(failed["ok"], false, "{failed}");
    let detail = failed["detail"].as_str().unwrap();
    assert!(detail.contains("exit code 3"), "{detail}");
    assert!(detail.contains("the stand-in cannot start"), "{detail}");
    assert_eq!(failed["program"], world.state.gh.display().to_string());

    world.state.gh = stand_in_gh(
        world.dir.path(),
        "working",
        "gh version 0.0.0 (demo)",
        false,
    );
    let ran = probe(&world).await;
    assert_eq!(ran["ok"], true, "{ran}");
    assert_eq!(ran["detail"], "gh version 0.0.0 (demo)");

    world.state.gh = world
        .dir
        .path()
        .join("nowhere")
        .join("gh-that-is-not-there");
    let missing = probe(&world).await;
    assert_eq!(missing["ok"], false, "{missing}");
    assert!(!missing["detail"].as_str().unwrap().is_empty(), "{missing}");

    assert_eq!(
        world
            .refused("doctor.probe", json!({"identity": "nobody"}))
            .await,
        i64::from(code::UNKNOWN_IDENTITY)
    );
    world.close().await;
}
