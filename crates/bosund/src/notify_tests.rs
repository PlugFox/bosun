//! The `notify.channel.*` methods (#9) over a served state of two identities, `a` and `b`, each
//! with its own store and its own credential store (in memory under `cfg(test)`).

use std::fmt::Write as _;

use bosun_api::rpc::code;
use bosun_config::SystemProbe;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::api::{Answer, Connection, handle_line};
use crate::state::{self, State};

pub(crate) const TOKEN: &str = "bosun-operator-notify-tests";
const SECRET: &str = "synthetic-secret-4f1c9a";

/// Two identities, `a` (repository `alpha`) and `b` (`beta`), served from one configuration;
/// `notify_outbox_tests` shares it.
pub(crate) struct World {
    pub(crate) dir: TempDir,
    pub(crate) state: State,
}

impl World {
    pub(crate) async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut config = String::new();
        for (identity, repo) in [("a", "alpha"), ("b", "beta")] {
            let claude = d.join(identity).join("claude");
            std::fs::create_dir_all(claude.join("projects")).unwrap();
            std::fs::create_dir_all(d.join(repo)).unwrap();
            let _ = write!(
                config,
                "[identity.{identity}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\n\
                 [repo.{repo}]\npath = '{}'\nidentity = '{identity}'\n\n",
                claude.display(),
                d.join(identity).join("bosun.db").display(),
                d.join(repo).display(),
            );
        }
        let path = d.join("config.toml");
        std::fs::write(&path, config).unwrap();
        let identities = Self::open(&path).await;
        Self {
            state: State {
                started_ms: 0,
                identities,
                credentials: crate::agent::Credentials::default(),
                control: crate::control::Control::with_token(TOKEN, None),
                leases: std::sync::Arc::new(crate::leases::Leases::none()),
                gh: std::path::PathBuf::from("gh"),
                blockers: std::sync::Arc::default(),
            },
            dir,
        }
    }

    /// Opens every identity of the configuration in `dir`.
    async fn open(path: &std::path::Path) -> std::collections::BTreeMap<String, state::Served> {
        let config = bosun_config::load(path, &SystemProbe::new()).unwrap();
        state::open(&config, "notify-epoch", crate::events::Options::default())
            .await
            .map_err(|(_, e)| e)
            .unwrap()
    }

    /// Shuts every identity down and opens them again on the same configuration: a daemon
    /// restart, as far as the stores and their drainers go.
    pub(crate) async fn restart(&mut self) {
        let identities = std::mem::take(&mut self.state.identities);
        state::shut_down(identities).await;
        self.state.identities = Self::open(&self.dir.path().join("config.toml")).await;
    }

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

    pub(crate) async fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params.clone())
            .await
            .unwrap_or_else(|e| panic!("{method} {params}: {e}"))
    }

    pub(crate) async fn refused(&self, method: &str, params: Value) -> i64 {
        let error = self.call(method, params).await.unwrap_err();
        error["code"].as_i64().unwrap()
    }

    /// The secret `identity`'s credential store holds under `entry`.
    fn secret(&self, identity: &str, entry: &str) -> Option<String> {
        self.state.identities[identity]
            .notify
            .secrets()
            .get(entry)
            .unwrap()
    }

    /// Adds a `fake` channel `name` writing to `sink` for `identity`.
    async fn add(&self, identity: &str, name: &str, sink: &str, secret: &str) -> Value {
        self.ok(
            "notify.channel.add",
            add_params(identity, name, sink, secret),
        )
        .await
    }

    async fn list(&self, identity: &str) -> Value {
        self.ok("notify.channel.list", json!({"identity": identity}))
            .await
    }

    pub(crate) async fn close(self) {
        state::shut_down(self.state.identities).await;
    }
}

fn add_params(identity: &str, name: &str, sink: &str, secret: &str) -> Value {
    json!({
        "operator": TOKEN,
        "identity": identity,
        "name": name,
        "kind": "fake",
        "params": {"sink": sink},
        "events": ["needs_you", "repo_paused"],
        "min_gap_s": 60,
        "secret": secret,
    })
}

fn names(list: &Value) -> Vec<&str> {
    list["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect()
}

/// Whether `haystack` contains `needle`, as bytes.
fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// #9: a channel's secret is in the identity's credential store under `notify/a/<name>` and
/// nowhere in its database or in any result (`mutant_notify_secret_in_row` writes it into the
/// row's parameters).
#[tokio::test]
async fn a_channel_secret_goes_to_the_credential_store_not_the_database() {
    let world = World::new().await;
    let added = world.add("a", "ops", "notify-tests-secret", SECRET).await;
    assert!(!added.to_string().contains(SECRET), "{added}");
    assert_eq!(world.secret("a", "notify/a/ops").as_deref(), Some(SECRET));

    // The row is in the file or in its WAL: both are read, and the row must be in one of them,
    // so the test reads where it was written.
    let database = world.dir.path().join("a").join("bosun.db");
    let wal = format!("{}-wal", database.display());
    let mut written = Vec::new();
    for file in [database.display().to_string(), wal] {
        let bytes = std::fs::read(&file).unwrap_or_default();
        assert!(!contains(&bytes, SECRET), "{file} holds the secret");
        written.extend(bytes);
    }
    assert!(
        contains(&written, "notify/a/ops"),
        "the row was not written"
    );

    let list = world.list("a").await;
    assert_eq!(names(&list), ["ops"]);
    let ops = &list["channels"][0];
    assert_eq!(ops["secret_present"], true, "{list}");
    assert_eq!(ops["secret_entry"], "notify/a/ops", "{list}");
    assert_eq!(ops["kind"], "fake", "{list}");
    assert_eq!(ops["min_gap_s"], 60, "{list}");
    assert!(!list.to_string().contains(SECRET), "{list}");

    world
        .ok(
            "notify.channel.remove",
            json!({"operator": TOKEN, "identity": "a", "name": "ops"}),
        )
        .await;
    assert_eq!(world.secret("a", "notify/a/ops"), None);
    assert_eq!(names(&world.list("a").await), Vec::<&str>::new());
    world.close().await;
}

/// #9, #2: identity `b` never sees, removes or overwrites a channel of `a`
/// (`mutant_notify_list_every_identity` lists every identity's channels).
#[tokio::test]
async fn another_identitys_channels_are_never_listed_or_removed() {
    let world = World::new().await;
    world.add("a", "ops", "notify-tests-a-ops", SECRET).await;

    let list = world.list("b").await;
    assert_eq!(names(&list), Vec::<&str>::new(), "{list}");
    let text = list.to_string();
    assert!(
        !text.contains("ops") && !text.contains("notify/a/"),
        "{text}"
    );

    let code = world
        .refused(
            "notify.channel.remove",
            json!({"operator": TOKEN, "identity": "b", "name": "ops"}),
        )
        .await;
    assert_eq!(code, i64::from(code::INVALID_PARAMS));
    assert_eq!(names(&world.list("a").await), ["ops"]);
    assert_eq!(world.secret("a", "notify/a/ops").as_deref(), Some(SECRET));

    let own = world
        .add(
            "b",
            "ops",
            "notify-tests-b-ops",
            "synthetic-secret-b-77e0b2",
        )
        .await;
    assert_eq!(own["channel"]["secret_entry"], "notify/b/ops", "{own}");
    assert_eq!(
        world.secret("b", "notify/b/ops").as_deref(),
        Some("synthetic-secret-b-77e0b2")
    );
    assert_eq!(world.secret("a", "notify/a/ops").as_deref(), Some(SECRET));
    assert_eq!(world.secret("a", "notify/b/ops"), None);
    assert_eq!(world.secret("b", "notify/a/ops"), None);
    assert_eq!(
        world.list("a").await["channels"][0]["secret_entry"],
        "notify/a/ops"
    );
    world.close().await;
}

/// #9: `add`, `remove` and `test` take the operator token and change nothing without it;
/// `list` takes none (`mutant_notify_no_operator_token` skips the check).
#[tokio::test]
async fn the_channel_writes_take_the_operator_token() {
    let world = World::new().await;
    let unauthorized = i64::from(code::UNAUTHORIZED);
    let without = |mut params: Value, token: Option<&str>| {
        match token {
            Some(token) => params["operator"] = json!(token),
            None => {
                params.as_object_mut().unwrap().remove("operator");
            }
        }
        params
    };
    for token in [None, Some("not-the-token")] {
        let add = without(
            add_params("a", "ops", "notify-tests-token-ops", SECRET),
            token,
        );
        assert_eq!(
            world.refused("notify.channel.add", add).await,
            unauthorized,
            "{token:?}"
        );
    }
    assert_eq!(names(&world.list("a").await), Vec::<&str>::new());
    assert_eq!(world.secret("a", "notify/a/ops"), None);

    world
        .add("a", "kept", "notify-tests-token-kept", SECRET)
        .await;
    for token in [None, Some("not-the-token")] {
        for method in ["notify.channel.remove", "notify.channel.test"] {
            let params = without(
                json!({"operator": TOKEN, "identity": "a", "name": "kept"}),
                token,
            );
            assert_eq!(
                world.refused(method, params).await,
                unauthorized,
                "{method} {token:?}"
            );
        }
    }
    assert_eq!(names(&world.list("a").await), ["kept"]);
    assert_eq!(world.secret("a", "notify/a/kept").as_deref(), Some(SECRET));
    assert_eq!(
        bosun_notify::fake::sent("notify-tests-token-kept"),
        Vec::<String>::new()
    );
    assert_eq!(
        bosun_notify::fake::sent("notify-tests-token-ops"),
        Vec::<String>::new()
    );
    world.close().await;
}

/// #9: `notify.channel.test` sends exactly one message naming the channel, now; a send that
/// fails is a result that is not accepted, with the error.
#[tokio::test]
async fn notify_test_sends_one_message_on_the_channel() {
    let world = World::new().await;
    world.add("a", "ops", "notify-tests-send-ops", SECRET).await;
    let tested = world
        .ok(
            "notify.channel.test",
            json!({"operator": TOKEN, "identity": "a", "name": "ops"}),
        )
        .await;
    assert_eq!(tested["accepted"], true, "{tested}");
    let sent = bosun_notify::fake::sent("notify-tests-send-ops");
    assert_eq!(sent, ["bosun a: test message from channel ops"]);

    let mut failing = add_params("a", "pager", "notify-tests-send-pager", SECRET);
    failing["params"]["fail"] = json!("1");
    world.ok("notify.channel.add", failing).await;
    let tested = world
        .ok(
            "notify.channel.test",
            json!({"operator": TOKEN, "identity": "a", "name": "pager"}),
        )
        .await;
    assert_eq!(tested["accepted"], false, "{tested}");
    let error = tested["error"].as_str().unwrap();
    assert!(error.contains("fails send 1"), "{tested}");
    assert!(!tested.to_string().contains(SECRET), "{tested}");
    assert_eq!(
        bosun_notify::fake::sent("notify-tests-send-pager"),
        Vec::<String>::new()
    );
    assert_eq!(bosun_notify::fake::sent("notify-tests-send-ops").len(), 1);
    world.close().await;
}

/// #9: a `telegram` channel is added through the registry alone: its chat is a parameter, its
/// token (synthetic) goes to the credential store and into no result; a bad chat is refused.
/// Nothing is sent: no test reaches Telegram.
#[tokio::test]
async fn a_telegram_channel_is_added_with_its_token_in_the_credential_store() {
    let world = World::new().await;
    let token = format!("123456789:{}", "A".repeat(35));
    let mut params = add_params("a", "chat", "unused", &token);
    params["kind"] = json!("telegram");
    params["params"] = json!({"chat": "-1001234567890"});
    let added = world.ok("notify.channel.add", params.clone()).await;
    assert!(!added.to_string().contains(&token), "{added}");
    assert_eq!(world.secret("a", "notify/a/chat").as_deref(), Some(&*token));
    let list = world.list("a").await;
    assert_eq!(list["channels"][0]["kind"], "telegram", "{list}");
    assert!(!list.to_string().contains(&token), "{list}");

    params["name"] = json!("other");
    params["params"] = json!({"chat": "hello"});
    assert_eq!(
        world.refused("notify.channel.add", params).await,
        i64::from(code::INVALID_PARAMS)
    );
    world.close().await;
}
