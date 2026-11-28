//! The stage-1 commands against a real `bosund` running its loop (#70): `status`, `queue`,
//! `item`, `lint`, `session tail` and the operator actions, with their JSON held to the
//! committed schema and their exit codes to the help.
//!
//! The daemon runs a local-file board in a temporary git repository. The agent is `fake-agent`
//! copied as `claude`, and the identity's login probe is `fake-agent` copied as `gh`: each copy
//! reads the script named like it (`claude.script`, `gh.script`). Nothing reaches the user's
//! configuration, agent, `gh` or the installed daemon: every path is in a temporary directory
//! and the endpoint is the test's own.
//!
//! `bosund`, `bosun-mcp` and `fake-agent` are binaries of other packages, none linked: under
//! nextest the setup script `build-daemon` builds them once per run, under `cargo test` the
//! test builds them with `cargo build` into the target directory it runs from.
#![allow(clippy::disallowed_methods, reason = "a test's own children")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bosun_client::{Client, Endpoint};
use serde_json::Value;

/// How long the daemon may take to start, and the item to reach a state.
const PATIENCE: Duration = Duration::from_secs(90);

fn bin_dir() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        // nextest's setup script `build-daemon` built them once for the whole run.
        if let Some(dir) = std::env::var_os("BOSUN_TEST_BIN_DIR") {
            return PathBuf::from(dir);
        }
        let dir = Path::new(env!("CARGO_BIN_EXE_bosun"))
            .parent()
            .unwrap()
            .to_path_buf();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut build = Command::new(cargo);
        // The packages of `.config/build-daemon.sh` and `tests/cli.rs`: one feature set, one
        // build for both test binaries.
        build
            .args(["build", "--locked"])
            .args(["--package", "bosund", "--package", "bosun-mcp"])
            .args(["--package", "fake-agent", "--package", "recount"])
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if dir.file_name().is_some_and(|n| n == "release") {
            build.arg("--release");
        }
        assert!(
            build.status().unwrap().success(),
            "building the daemon failed"
        );
        dir
    })
}

fn exe(name: &str) -> PathBuf {
    bin_dir().join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// A path as the configuration's TOML literal strings and a POSIX shell read it.
fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn git(dir: &Path, gitconfig: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn card(id: u64, title: &str, body: &str) -> String {
    format!(
        "[[card]]\nid = {id}\ntitle = \"{title}\"\ncolumn = \"Ready\"\nbody = '''\n{body}\n'''\n\n"
    )
}

/// The temporary world: a repository, its board, an identity, the fake tools, an endpoint.
struct World {
    dir: tempfile::TempDir,
    endpoint: Endpoint,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for sub in ["claude/projects", "gh", "repo", "bin"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let gitconfig = d.join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Synthetic Agent\n\temail = agent@example.com\n",
        )
        .unwrap();
        let repo = d.join("repo");
        git(&repo, &gitconfig, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &gitconfig,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/synthetic-owner/demo.git",
            ],
        );
        std::fs::write(repo.join("README.md"), "demo\n").unwrap();
        git(&repo, &gitconfig, &["add", "README.md"]);
        git(&repo, &gitconfig, &["commit", "-q", "-m", "Start"]);

        let mut board = card(
            1,
            "One",
            "Write one.txt.\n\n```bosun\nverdict = \"test -f one.txt\"\n```",
        );
        board.push_str(&card(2, "Two", "No contract block: lint refuses it."));
        std::fs::write(d.join("board.toml"), board).unwrap();
        let config = format!(
            "[identity.personal]\n\
             claude_config_dir = '{claude}'\n\
             database = '{db}'\n\
             github_login = 'synthetic-login'\n\
             gh_config_dir = '{gh}'\n\
             git_config = '{gitconfig}'\n\
             remote_allowlist = ['synthetic-owner']\n\
             author_allowlist = ['local']\n\n\
             [repo.demo]\n\
             path = '{repo}'\n\
             identity = 'personal'\n\
             queue = {{ kind = 'local_file', file = '{board}' }}\n\
             model = 'opus'\n\
             verdict = {{ timeout = '60s' }}\n",
            claude = slashed(&d.join("claude")),
            db = slashed(&d.join("db").join("bosun.db")),
            gh = slashed(&d.join("gh")),
            gitconfig = slashed(&gitconfig),
            repo = slashed(&repo),
            board = slashed(&d.join("board.toml")),
        );
        std::fs::write(d.join("config.toml"), config).unwrap();

        // The fake tools, each reading the script named like it.
        for tool in ["claude", "gh"] {
            let copy = d
                .join("bin")
                .join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(exe("fake-agent"), copy).unwrap();
        }
        std::fs::write(d.join("bin").join("gh.script"), "print synthetic-login\n").unwrap();
        let replay = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream/normal-end.jsonl");
        std::fs::write(
            d.join("bin").join("claude.script"),
            format!(
                "eprint line-before\nwrite {started} x\nwait {release}\neprint line-one\n\
                 eprint line-two\ncommit one.txt one\nreplay {replay}\n",
                started = slashed(&d.join("started")),
                release = slashed(&d.join("release")),
                replay = slashed(&replay),
            ),
        )
        .unwrap();

        let endpoint = if cfg!(windows) {
            Endpoint::new(format!("bosun-cli-loop-test-{}", std::process::id()))
        } else {
            Endpoint::new(d.join("bosund.sock"))
        };
        Self { dir, endpoint }
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.dir.path().join(rest)
    }

    async fn start(&self) -> Daemon {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.path("bin")];
        dirs.extend(std::env::split_paths(&path));
        let log = std::fs::File::create(self.path("daemon.log")).unwrap();
        let mut child = Command::new(exe("bosund"))
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env("BOSUN_CONFIG", self.path("config.toml"))
            .env("BOSUN_ENDPOINT", self.endpoint.path())
            .env("BOSUN_OPERATOR_FILE", self.path("operator.token"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("the daemon exited at start: {status}: {}", self.log());
            }
            match Client::connect(&self.endpoint).await {
                Ok(_) => return Daemon { child },
                Err(e) if Instant::now() > deadline => panic!("the daemon never answered: {e}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.path("daemon.log")).unwrap_or_default()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosun"));
        command
            .args(args)
            .env("BOSUN_CONFIG", self.path("config.toml"))
            .env("BOSUN_ENDPOINT", self.endpoint.path())
            .env("BOSUN_OPERATOR_FILE", self.path("operator.token"))
            .stdin(Stdio::null());
        command
    }

    fn bosun(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs `bosun`; its stdout as JSON, which must exit 0.
    fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.extend(["--format", "json"]);
        let output = self.bosun(&all);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    async fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what} never happened; the daemon said:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The committed schema.
fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/bosun-api.schema.json");
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    })
}

/// Checks `value` against `schema` (the subset of JSON Schema the generator writes), strictly:
/// a key the schema does not name is an error too.
fn check(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.strip_prefix("#/$defs/").unwrap();
        return check(&self::schema()["$defs"][name], value, at);
    }
    if let Some(branches) = schema.get("oneOf").or_else(|| schema.get("anyOf")) {
        let branches = branches.as_array().unwrap();
        return if branches.iter().any(|b| check(b, value, at).is_ok()) {
            Ok(())
        } else {
            Err(format!("{at}: no branch matches {value}"))
        };
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return Err(format!("{at}: {value} is not {constant}"));
    }
    if let Some(types) = schema.get("type") {
        let types: Vec<&str> = match types {
            Value::String(t) => vec![t.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let ok = types.iter().any(|t| match *t {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => false,
        });
        if !ok {
            return Err(format!("{at}: {value} is not of type {types:?}"));
        }
    }
    if let (Some(object), Some(properties)) = (value.as_object(), schema.get("properties")) {
        for required in schema["required"].as_array().into_iter().flatten() {
            let key = required.as_str().unwrap();
            if !object.contains_key(key) {
                return Err(format!("{at}: `{key}` is missing"));
            }
        }
        for (key, member) in object {
            let Some(property) = properties.get(key) else {
                return Err(format!("{at}: `{key}` is not in the schema"));
            };
            check(property, member, &format!("{at}.{key}"))?;
        }
    }
    if let (Some(items), Some(schema)) = (value.as_array(), schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            check(schema, item, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

/// Asserts that `value` is the result of `method` as the committed schema has it.
fn assert_result(method: &str, value: &Value) {
    let result = &schema()["x-methods"][method]["result"];
    assert!(result.is_object(), "the schema has no method {method}");
    if let Err(e) = check(result, value, method) {
        panic!("{e}\n{value:#}");
    }
}

fn assert_event(value: &Value) {
    let event = &schema()["x-notifications"]["events.event"]["params"];
    if let Err(e) = check(event, value, "event") {
        panic!("{e}\n{value:#}");
    }
}

/// #70 acceptance: each command's JSON matches the committed schema; `session tail` follows a
/// running `fake-agent` and ends when it exits; `lint` of a bad card exits 1 with its findings;
/// with the daemon down every command exits 4.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_stage_1_commands_follow_a_running_session() {
    let world = World::new();
    let daemon = world.start().await;
    world
        .wait_for("the session to start", || world.path("started").exists())
        .await;

    let status = world.json(&["status"]);
    let personal = &status["personal"];
    assert_result("status.overview", personal);
    assert_eq!(
        personal["repos"][0]["current"]["item"], "demo#1",
        "{status}"
    );
    assert_eq!(personal["repos"][0]["current"]["state"], "running");

    let queue = world.json(&["queue", "--identity", "personal"]);
    assert_result("queue.get", &queue);
    let ready: Vec<&Value> = queue["repos"][0]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["cards"].as_array().unwrap())
        .collect();
    let one = ready.iter().find(|c| c["card"] == 1).unwrap();
    assert_eq!(one["state"], "running", "{queue}");

    let item = world.json(&["item", "demo#1"]);
    assert_result("item.get", &item);
    assert_eq!(
        item["attempts"][0]["sessions"][0]["running"], true,
        "{item}"
    );

    // Two tails, text and JSON; the session goes on once both are subscribed.
    let tail = |format: &str| {
        world
            .command(&["session", "tail", "demo#1", "--format", format])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (text_tail, json_tail) = (tail("text"), tail("json"));
    world
        .wait_for("both tails to subscribe", || {
            let status = world.json(&["status", "--identity", "personal"]);
            status["events"]["subscribers"].as_u64() >= Some(2)
        })
        .await;
    std::fs::write(world.path("release"), "").unwrap();
    let (text, json) = (
        text_tail.wait_with_output().unwrap(),
        json_tail.wait_with_output().unwrap(),
    );
    assert_eq!(text.status.code(), Some(0), "{text:?}");
    let lines = String::from_utf8(text.stdout.clone()).unwrap();
    assert!(lines.contains("line-one\nline-two\n"), "{lines}");
    assert!(
        String::from_utf8_lossy(&text.stderr).contains("ended: ok"),
        "{text:?}"
    );
    assert_eq!(json.status.code(), Some(0), "{json:?}");
    let events: Vec<Value> = String::from_utf8(json.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    events.iter().for_each(assert_event);
    assert_eq!(events.last().unwrap()["body"]["type"], "session_ended");
    assert!(
        events.iter().any(|e| e["body"]["line"] == "line-two"),
        "{events:?}"
    );

    world
        .wait_for("the item to be done", || {
            world.json(&["item", "demo#1"])["state"] == "done"
        })
        .await;
    // A session that has ended is followed no further.
    let output = world.bosun(&["session", "tail", "demo#1"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    // Lint: a bad card exits 1 with its findings, in text and in JSON.
    let output = world.bosun(&["lint", "demo#2"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("not claimable") && text.contains("error: "),
        "{text}"
    );
    let output = world.bosun(&["lint", "2", "--repo", "demo", "--format", "json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let lint: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_result("queue.lint", &lint);
    assert_eq!(lint["claimable"], false);
    let lint = world.json(&["lint", "demo#1"]);
    assert_result("queue.lint", &lint);
    assert_eq!(lint["claimable"], true, "{lint}");

    // The operator actions: restart-item refuses an item no longer in flight.
    let output = world.bosun(&["restart-item", "demo#1"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not in flight"),
        "{output:?}"
    );
    assert_result(
        "control.pause",
        &world.json(&["pause", "--repo", "demo"])["personal"],
    );
    assert_result(
        "control.resume",
        &world.json(&["resume", "--identity", "personal"]),
    );
    assert_result("control.kill", &world.json(&["kill"]));
    assert_result("control.kill", &world.json(&["kill", "--release"]));

    // With the daemon down, every command exits 4 and prints nothing on stdout.
    drop(daemon);
    for args in [
        &["status"][..],
        &["queue"],
        &["item", "demo#1"],
        &["lint", "demo#2"],
        &["session", "tail", "demo#1"],
        &["pause"],
        &["resume", "--repo", "demo"],
        &["kill"],
        &["restart-item", "demo#1"],
    ] {
        let output = world.bosun(args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    }
}

/// The schema check refuses what the schema does not allow, so the test above can go red.
#[test]
fn the_schema_check_refuses_a_document_off_the_schema() {
    assert_result("control.kill", &serde_json::json!({"killed": true}));
    let result = &schema()["x-methods"]["control.kill"]["result"];
    for bad in [
        serde_json::json!({"killed": true, "extra": 1}),
        serde_json::json!({}),
        serde_json::json!({"killed": "yes"}),
    ] {
        assert!(check(result, &bad, "kill").is_err(), "{bad}");
    }
}
