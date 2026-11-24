//! The read methods and restart-item of #70 over a served state: two identities, each with a
//! local-file board, their stores written as the loop writes them.

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
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

struct World {
    dir: TempDir,
    state: State,
}

impl World {
    async fn new() -> Self {
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
                 queue = {{ kind = 'local_file', file = '{}' }}\n\n",
                claude.display(),
                d.join(identity).join("bosun.db").display(),
                d.join(repo).display(),
                d.join(format!("{repo}.toml")).display(),
            );
        }
        let path = d.join("config.toml");
        std::fs::write(&path, config).unwrap();
        let config = bosun_config::load(&path, &SystemProbe::new()).unwrap();
        let identities = state::open(&config, "view-epoch", crate::events::Options::default())
            .await
            .map_err(|(_, e)| e)
            .unwrap();
        let world = Self {
            state: State {
                started_ms: T0,
                identities,
                credentials: crate::agent::Credentials::default(),
                control: crate::control::Control::with_token(TOKEN, None),
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
