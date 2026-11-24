//! The scenario suite of the loop (#62): `fake-agent` as the agent CLI (behind a `.cmd` wrapper
//! on Windows, as npm installs `claude`), the local-file queue, a real git repository, a real
//! store, the real verdict shell. Nothing touches the user's own configuration, agent or daemon:
//! every path is in a temporary directory and the login probe is a fake.
//!
//! As the agent CLI does, `fake-agent` starts the session's MCP servers from `--mcp-config`:
//! the real `bosun-mcp`, which calls the scene's own endpoint, served with the scene's
//! credentials (#64).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bosun_config::{Config, DiskProbe, SystemProbe};
use bosun_core::{Outcome, ParkReason, Reason, SpawnMode, State};
use bosun_identity::{Bundle, ProbeError, TokenLogin};
use bosun_proc::{ChildSpec, NativeSpawner, Spawned, Spawner};
use bosun_queue::{Card, Column, LocalFile, Queue};
use bosun_store::{Options as StoreOptions, Store};
use tokio::sync::watch;

use super::*;
use crate::config_watch;
use crate::events::{Events, Options as EventOptions};

/// How long a session that asked a blocking question may go on in these scenarios.
const GRACE: Duration = Duration::from_secs(2);

/// The login every identity check sees.
const LOGIN: &str = "synthetic-login";

/// `fake-agent` and `bosun-mcp`, built next to this test binary.
fn built() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut build = Command::new(cargo);
        build
            .args([
                "build",
                "--locked",
                "--package",
                "fake-agent",
                "--package",
                "bosun-mcp",
                "--message-format=json",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(std::process::Stdio::inherit());
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        let out = build.output().unwrap();
        assert!(
            out.status.success(),
            "building fake-agent and bosun-mcp failed"
        );
        // Where cargo put them, whatever the target and build directories are.
        let messages = String::from_utf8_lossy(&out.stdout);
        let executable = |name: &str| {
            messages
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|m| m["target"]["name"] == name)
                .find_map(|m| m["executable"].as_str().map(PathBuf::from))
                .unwrap_or_else(|| panic!("cargo names the {name} executable"))
        };
        (executable("fake-agent"), executable("bosun-mcp"))
    })
}

fn fake_agent() -> &'static Path {
    &built().0
}

fn bosun_mcp() -> &'static Path {
    &built().1
}

/// A fresh endpoint for one scene's API.
fn scene_endpoint(dir: &Path) -> bosun_client::Endpoint {
    if cfg!(windows) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        bosun_client::Endpoint::new(format!(
            "bosun-test-loop-{}-{n}-{nanos}",
            std::process::id()
        ))
    } else {
        bosun_client::Endpoint::new(dir.join("bosund.sock"))
    }
}

/// A path as a POSIX shell in a verdict reads it.
fn sh_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Debug)]
struct FixedLogin;

impl TokenLogin for FixedLogin {
    fn login(&self, _host: &str) -> Result<String, ProbeError> {
        Ok(LOGIN.to_owned())
    }
}

#[derive(Debug)]
struct FixedLogins;

impl Logins for FixedLogins {
    fn probe(&self, _bundle: &Bundle) -> Box<dyn TokenLogin> {
        Box::new(FixedLogin)
    }
}

/// The native spawner, counting the agent sessions it spawns (not the verdict shells).
#[derive(Debug, Default)]
struct Counting {
    spawns: AtomicUsize,
}

impl Spawner for Counting {
    fn spawn(&self, spec: &ChildSpec) -> std::io::Result<Spawned> {
        let agent = spec
            .program
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case("claude"));
        if agent {
            self.spawns.fetch_add(1, Ordering::SeqCst);
        }
        NativeSpawner.spawn(spec)
    }
}

/// The system's clocks, with a wall clock the test moves forward (a simulated sleep, a park
/// timer run out) while the monotonic clock goes on as it is.
#[derive(Debug)]
struct TestClock {
    system: SystemClock,
    ahead_ms: AtomicI64,
}

impl TestClock {
    fn advance(&self, by: Duration) {
        let ms = i64::try_from(by.as_millis()).unwrap();
        self.ahead_ms.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn wall_ms(&self) -> i64 {
        self.system.wall_ms() + self.ahead_ms.load(Ordering::SeqCst)
    }

    fn mono(&self) -> Duration {
        self.system.mono()
    }
}

/// The wake check's liveness probe: the real one, unless the test declares the child dead (it
/// died in the sleep); counts its calls.
#[derive(Debug, Default)]
struct Probe {
    dead: AtomicBool,
    calls: AtomicUsize,
}

/// One scenario's world.
struct Scene {
    dir: tempfile::TempDir,
    repo: PathBuf,
    board: PathBuf,
    config_path: PathBuf,
    scripts: PathBuf,
    gitconfig: PathBuf,
    store: Store,
    spawner: Arc<Counting>,
    credentials: crate::agent::Credentials,
    clock: Arc<TestClock>,
    probe: Arc<Probe>,
    /// The endpoint the scene's API is served on, for `bosun-mcp`.
    endpoint: bosun_client::Endpoint,
    /// The MCP server sessions get: the real `bosun-mcp` unless a scenario breaks it.
    mcp: PathBuf,
    /// How long after spawn `bosun-mcp` has to say hello.
    handshake: Duration,
    /// The control state of #67, and the loop's events.
    control: crate::control::Control,
    events: Events,
}

impl Scene {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The configuration file's text, with `protected` as the policy's list.
    fn config_text(&self, protected: &str) -> String {
        let d = self.dir.path();
        format!(
            "[identity.test]\n\
             claude_config_dir = '{claude}'\n\
             database = '{db}'\n\
             github_login = '{LOGIN}'\n\
             gh_config_dir = '{gh}'\n\
             git_config = '{gitconfig}'\n\
             remote_allowlist = ['synthetic-owner']\n\
             author_allowlist = ['local']\n\n\
             [repo.demo]\n\
             path = '{repo}'\n\
             identity = 'test'\n\
             queue = {{ kind = 'local_file', file = '{board}' }}\n\
             verdict = {{ timeout = '60s' }}\n\n\
             [repo.demo.policy]\n\
             protected = [{protected}]\n",
            claude = d.join("claude").display(),
            db = d.join("db").join("bosun.db").display(),
            gh = d.join("gh").display(),
            gitconfig = self.gitconfig.display(),
            repo = self.repo.display(),
            board = self.board.display(),
        )
    }

    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for sub in ["claude", "db", "gh", "repo", "scripts", "bin", "work"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let gitconfig = d.join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Synthetic Agent\n\temail = agent@example.com\n",
        )
        .unwrap();
        let store = Store::open(d.join("db").join("bosun.db"), StoreOptions::default())
            .await
            .unwrap();
        // The scene's API, holding only the credentials: what `bosun-mcp` calls.
        let credentials = crate::agent::Credentials::default();
        let endpoint = scene_endpoint(d);
        let listener = crate::listen::claim(&endpoint).await.unwrap();
        let control = crate::control::Control::with_token(control::TOKEN, None);
        let api = Arc::new(crate::state::State {
            started_ms: 0,
            identities: BTreeMap::new(),
            credentials: credentials.clone(),
            control: control.clone(),
        });
        tokio::spawn(async move {
            crate::state::serve(listener, &api, std::future::pending()).await;
        });
        let scene = Self {
            repo: d.join("repo"),
            board: d.join("board.toml"),
            config_path: d.join("config.toml"),
            scripts: d.join("scripts"),
            gitconfig,
            store,
            spawner: Arc::new(Counting::default()),
            clock: Arc::new(TestClock {
                system: SystemClock::new(),
                ahead_ms: AtomicI64::new(0),
            }),
            probe: Arc::new(Probe::default()),
            credentials,
            endpoint,
            mcp: bosun_mcp().to_owned(),
            handshake: Duration::from_secs(30),
            control,
            events: Events::new("test", EventOptions::default()),
            dir,
        };
        scene.git(&["init", "-q", "-b", "main"]);
        scene.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/synthetic-owner/demo.git",
        ]);
        std::fs::write(scene.repo.join("README.md"), "demo\n").unwrap();
        scene.git(&["add", "README.md"]);
        scene.git(&["commit", "-q", "-m", "Start"]);
        std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
        scene
    }

    /// The agent CLI: `fake-agent` picking each item's script, behind a `.cmd` shim on Windows.
    fn agent(&self) -> PathBuf {
        let bin = self.path("bin");
        if cfg!(windows) {
            let shim = bin.join("claude.cmd");
            std::fs::write(
                &shim,
                format!(
                    "@echo off\r\n\"{}\" --fake-script-dir \"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
                    fake_agent().display(),
                    self.scripts.display()
                ),
            )
            .unwrap();
            shim
        } else {
            let shim = bin.join("claude");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nexec '{}' --fake-script-dir '{}' \"$@\"\n",
                    fake_agent().display(),
                    self.scripts.display()
                ),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            shim
        }
    }

    fn script(&self, card: u64, lines: &[String]) {
        let replay = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream/normal-end.jsonl");
        let mut text = lines.join("\n");
        let _ = writeln!(text, "\nreplay {}", replay.display());
        std::fs::write(self.scripts.join(format!("demo_{card}.script")), text).unwrap();
    }

    fn write_board(&self, cards: &[Card]) {
        std::fs::write(&self.board, LocalFile::render(cards)).unwrap();
    }

    fn cards(&self) -> Vec<Card> {
        LocalFile::new(&self.board).poll(None).unwrap().cards
    }

    fn config(&self) -> Config {
        bosun_config::load(&self.config_path, &SystemProbe::new()).unwrap()
    }

    /// Starts the loop and the configuration watcher.
    fn start(&self, failpoint: Option<Failpoint>) -> (JoinHandle<()>, JoinHandle<()>) {
        let (tx, rx) = watch::channel(Arc::new(self.config()));
        let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(SystemProbe::new());
        let watcher = config_watch::spawn(
            self.config_path.clone(),
            tx,
            probe,
            Duration::from_millis(50),
            config_watch::Reported::default(),
        );
        let options = Options {
            repo: "demo".to_owned(),
            daemon_id: "test-host/test-user".to_owned(),
            agent: Some(self.agent()),
            work_dir: self.path("work"),
            poll: Duration::from_millis(50),
            recheck: Duration::from_hours(1),
            question_grace: GRACE,
            mcp: self.mcp.clone(),
            endpoint: self.endpoint.clone(),
            handshake: self.handshake,
            failpoint,
        };
        let deps = Deps {
            reader: self.store.reader().clone(),
            writer: self.store.writer().clone(),
            events: self.events.clone(),
            queue: Arc::new(LocalFile::new(&self.board)),
            spawner: self.spawner.clone(),
            power: Arc::new(bosun_proc::FakePower::default()),
            logins: Arc::new(FixedLogins),
            credentials: self.credentials.clone(),
            clock: self.clock.clone(),
            alive: {
                let probe = self.probe.clone();
                Arc::new(move |key| {
                    probe.calls.fetch_add(1, Ordering::SeqCst);
                    !probe.dead.load(Ordering::SeqCst) && bosun_proc::is_running(key)
                })
            },
            control: self.control.clone(),
        };
        let task = tokio::spawn(Loop::new(options, deps, rx).run());
        (task, watcher)
    }

    async fn state(&self, card: u64) -> Option<State> {
        self.item(card).await.map(|i| i.state)
    }

    async fn item(&self, card: u64) -> Option<Item> {
        self.store
            .reader()
            .item(&ItemId::new(format!("demo#{card}")))
            .await
            .unwrap()
            .map(|s| s.item)
    }

    async fn runs(&self, card: u64) -> Vec<bosun_store::RunRow> {
        self.store
            .reader()
            .runs(&ItemId::new(format!("demo#{card}")))
            .await
            .unwrap()
    }

    /// A stream fixture of `fixtures/claude-stream`.
    fn stream(name: &str) -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-stream")
            .join(name)
            .display()
            .to_string()
    }
}

fn card(id: u64, title: &str, verdict: &str, prose: &str) -> Card {
    Card {
        id: bosun_queue::ItemId(id),
        title: title.to_owned(),
        column: Column::Ready,
        body: format!("{prose}\n\n```bosun\nverdict = \"{verdict}\"\n```\n"),
        labels: Vec::new(),
        draft: false,
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

async fn wait_for(what: &str, limit: Duration, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_file(path: &Path) {
    wait_for(
        &path.display().to_string(),
        Duration::from_secs(60),
        async || path.exists(),
    )
    .await;
}

fn terminal(state: Option<State>) -> bool {
    matches!(state, Some(State::Done(_) | State::NeedsYou(_)))
}

/// The run of #62's acceptance: seven local-file items (six on the board at start, one added
/// during the run), each ending Done or Needs you with a reason:
///
/// 1. passes on a flaky verdict: Done, its flaky test recorded;
/// 2. prints "done", its verdict fails: Needs you after two attempts, the second prompt
///    carrying the first verdict's output;
/// 3. its verdict passes on the base commit: bounced at claim, never spawned;
/// 4. leaves the file its verdict needs uncommitted: the clean-tree precondition fails it twice
///    (`mutant_git_skip_clean_tree` lets it reach Done);
/// 5. resets below its base: the ancestry precondition fails it twice
///    (`mutant_git_skip_ancestry` lets it reach Done);
/// 6. its verdict is edited on the card during its session: the snapshot's verdict runs
///    (`mutant_loop_live_verdict` runs the edit);
/// 7. added during the run; the policy changes during its session: Needs you
///    (`mutant_loop_ignore_hash_change` lets it reach Done).
///
/// During item 1's session card 4 is edited, card 7 added and cards 4 and 5 swapped; all take
/// effect with no restart. The checkout ends clean on the claim branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn seven_items_end_done_or_needs_you_and_board_edits_take_effect() {
    let scene = Scene::new().await;
    let flag = |name: &str| scene.path(name);
    let p = |name: &str| flag(name).display().to_string();
    let count1 = sh_path(&flag("count1"));
    scene.write_board(&[
        card(
            1,
            "Flaky",
            &format!(
                "n=$(cat {count1} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count1}; \
                 if [ $n -eq 2 ]; then echo 'test a::b ... FAILED'; exit 1; fi; \
                 [ $n -ge 2 ] && test -f one.txt"
            ),
            "Item one.",
        ),
        card(2, "Says done", "echo verdict-says-no; false", "Item two."),
        card(3, "Passes on base", "true", "Item three."),
        card(4, "Uncommitted", "test -f loose.txt", "Item four."),
        card(5, "Below base", "test -f five.txt", "Item five."),
        card(6, "Edited verdict", "test -f six.txt", "Item six."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            format!("argv {}", p("argv2")),
            format!("stdin {}", p("prompt2")),
            "print done".to_owned(),
            "git commit --allow-empty -q -m two".to_owned(),
        ],
    );
    scene.script(3, &[format!("write {} x", p("spawned3"))]);
    scene.script(
        4,
        &[
            "git commit --allow-empty -q -m four".to_owned(),
            "write loose.txt x".to_owned(),
        ],
    );
    scene.script(
        5,
        &[
            "git reset -q --hard HEAD~1".to_owned(),
            "commit five.txt five".to_owned(),
        ],
    );
    scene.script(
        6,
        &[
            format!("write {} x", p("flag6")),
            format!("wait {}", p("release6")),
            "commit six.txt six".to_owned(),
        ],
    );
    scene.script(
        7,
        &[
            format!("write {} x", p("flag7")),
            format!("wait {}", p("release7")),
            "commit seven.txt seven".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);

    // During item 1's session: an edit, an addition and a reorder.
    wait_file(&flag("flag1")).await;
    let mut cards = scene.cards();
    let at = |cards: &[Card], id: u64| cards.iter().position(|c| c.id.0 == id).unwrap();
    let four = at(&cards, 4);
    cards[four].body = cards[four]
        .body
        .replace("Item four.", "Item four, edited while item one ran.");
    let (four, five) = (at(&cards, 4), at(&cards, 5));
    cards.swap(four, five);
    cards.push(card(
        7,
        "Policy changes",
        "test -f seven.txt",
        "Item seven.",
    ));
    scene.write_board(&cards);
    std::fs::write(flag("release1"), "").unwrap();

    // During item 6's session its card's verdict is edited.
    wait_file(&flag("flag6")).await;
    let mut cards = scene.cards();
    let six = at(&cards, 6);
    cards[six].body = cards[six].body.replace("test -f six.txt", "false");
    scene.write_board(&cards);
    std::fs::write(flag("release6"), "").unwrap();

    // During item 7's session the policy changes.
    wait_file(&flag("flag7")).await;
    std::fs::write(
        &scene.config_path,
        scene.config_text("'docs/frozen', 'tests/harness'"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !terminal(scene.state(7).await) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A loop that missed the change gets its session released, and must not reach Done.
    std::fs::write(flag("release7"), "").unwrap();

    let mut states = BTreeMap::new();
    wait_for("every item to end", Duration::from_secs(180), async || {
        for n in 1..=7 {
            states.insert(n, scene.state(n).await);
        }
        states.values().all(|s| terminal(*s))
    })
    .await;
    task.abort();
    watcher.abort();

    let id = |n: u64| ItemId::new(format!("demo#{n}"));
    let reader = scene.store.reader();
    let exhausted = Some(State::NeedsYou(Reason::AttemptsExhausted));
    assert_eq!(states[&1], Some(State::Done(Outcome::Flaky)), "item 1");
    assert_eq!(states[&2], exhausted, "item 2");
    assert_eq!(states[&3], Some(State::NeedsYou(Reason::Lint)), "item 3");
    assert_eq!(
        states[&4], exhausted,
        "item 4: an uncommitted file fails the clean tree"
    );
    assert_eq!(
        states[&5], exhausted,
        "item 5: a reset below the base fails ancestry"
    );
    assert_eq!(states[&6], Some(State::Done(Outcome::Pass)), "item 6");
    assert_eq!(
        states[&7],
        Some(State::NeedsYou(Reason::ConfigChanged)),
        "item 7: the policy changed during its session"
    );

    // Claimed in board order, the reorder and the addition included.
    let mut claimed = Vec::new();
    for n in 1..=7 {
        let ts = reader
            .transitions(&id(n))
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.cause == "claim")
            .map(|t| (t.ts_ms, t.seq));
        claimed.push((ts.expect("every card was claimed"), n));
    }
    claimed.sort_unstable();
    let order: Vec<u64> = claimed.iter().map(|(_, n)| *n).collect();
    assert_eq!(order, [1, 2, 3, 5, 4, 6, 7], "claim order");
    // The edit took effect: item 4 ran from the edited body.
    let snapshot = reader.snapshot(&id(4)).await.unwrap().unwrap();
    assert!(
        snapshot.body.contains("edited while item one ran"),
        "{}",
        snapshot.body
    );

    // Item 3 was bounced by its verdict on the base commit and never spawned.
    assert_eq!(reader.runs(&id(3)).await.unwrap().len(), 0);
    assert!(!flag("spawned3").exists());

    // Item 2: two attempts; the dropped one is stored as failed; the second prompt carries the
    // first verdict's output; the command loads no user settings, through the shim intact.
    let attempts = reader.attempts(&id(2)).await.unwrap();
    let states2: Vec<&str> = attempts.iter().map(|a| a.attempt.state.as_str()).collect();
    assert_eq!(states2, ["fail", "fail"]);
    let prompt = std::fs::read_to_string(flag("prompt2")).unwrap();
    assert!(prompt.contains("verdict-says-no"), "{prompt}");
    let argv: Vec<String> = std::fs::read_to_string(flag("argv2"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let after = |flag: &str| {
        argv.iter()
            .position(|a| a == flag)
            .map(|i| argv[i + 1].as_str())
    };
    assert_eq!(after("--setting-sources"), Some("project"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "Bash(git push:*)"), "{argv:?}");

    // Item 1: its flaky test, and its session's commits linked to it.
    let flaky = reader.flaky_tests(&id(1)).await.unwrap();
    assert_eq!(flaky, [(1, "a::b".to_owned())]);
    let runs = reader.runs(&id(1)).await.unwrap();
    assert!(
        runs.iter()
            .all(|r| r.base_sha.is_some() && r.head_sha.is_some()),
        "{runs:?}"
    );

    // Item 6 ran the snapshot's verdict, not the edited card's.
    let verdicts = reader.verdict_runs(&id(6)).await.unwrap();
    assert!(
        verdicts.iter().all(|v| v.command == "test -f six.txt"),
        "{verdicts:?}"
    );

    // The checkout ends clean on the claim branch, with the Done items' work only.
    assert_eq!(scene.git(&["status", "--porcelain"]), "");
    assert_eq!(scene.git(&["symbolic-ref", "--short", "HEAD"]), "main");
    for (file, kept) in [
        ("one.txt", true),
        ("six.txt", true),
        ("five.txt", false),
        ("loose.txt", false),
        ("seven.txt", false),
    ] {
        assert_eq!(scene.repo.join(file).exists(), kept, "{file}");
    }
    // The work of an item that did not end Done is set aside, not lost.
    let aside = scene.git(&["for-each-ref", "--format=%(refname)", "refs/bosun/items/"]);
    assert!(aside.contains("refs/bosun/items/demo-5"), "{aside}");
}

/// Every transition is written before its effects: when the store refuses the transition that
/// carries the claim, the board is not written and nothing is spawned
/// (`mutant_loop_effect_before_transition` performs the claim first: the card moves to In
/// progress and gets its claim marker with no row behind them). The claim is the effect
/// observed because the store itself refuses a session's run row without its attempt, which
/// only the transition creates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_transition_leaves_no_effect_without_its_row() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let refused = Arc::new(AtomicUsize::new(0));
    let seen = refused.clone();
    let failpoint: Failpoint = Arc::new(move |t: &bosun_core::Transition| {
        let claims = t
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Claim));
        if claims {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        claims
    });
    let (task, watcher) = scene.start(Some(failpoint));
    // The loop halts once the store refuses a transition.
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("the loop stops after a refused transition")
        .unwrap();
    watcher.abort();
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    // An effect performed before its row would show up here.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let cards = scene.cards();
    assert_eq!(
        (cards[0].column, cards[0].comments.len()),
        (Column::Ready, 0),
        "the board was written without the claim's transition"
    );
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let id = ItemId::new("demo#1");
    assert_eq!(scene.store.reader().runs(&id).await.unwrap().len(), 0);
    assert_eq!(scene.state(1).await, Some(State::Ready));
}

/// The session's credential, as its MCP configuration carries it to `bosun-mcp`.
fn credential_of_session(work: &Path) -> String {
    let sessions = std::fs::read_dir(work.join("sessions")).unwrap();
    let mut found = Vec::new();
    for dir in sessions {
        let text = std::fs::read_to_string(dir.unwrap().path().join("mcp.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();
        let env = &config["mcpServers"]["bosun"]["env"];
        found.push(env[session::CREDENTIAL_VAR].as_str().unwrap().to_owned());
    }
    assert_eq!(found.len(), 1, "one session, one credential");
    found.remove(0)
}

/// Sends one request on `conn` and returns its response.
async fn rpc(
    state: &crate::state::State,
    conn: &mut crate::api::Connection,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let line = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    match crate::api::handle_line(state, conn, &line.to_string()).await {
        crate::api::Answer::Reply(Some(response)) => serde_json::to_value(response).unwrap(),
        other => panic!("{method}: {other:?}"),
    }
}

fn error_code(response: &serde_json::Value) -> Option<i64> {
    response["error"]["code"].as_i64()
}

/// The `agent.*` methods of #63 against a running session of the loop:
///
/// - the session's credential is bound to its item: a call naming another item is refused
///   (`mutant_agent_any_item` accepts it), and it cannot call `tokens.*`, on its own connection
///   or carried to another;
/// - a summary that says "verdict passed" changes no state;
/// - of two summaries the report data holds the second, with the HEAD at its call
///   (`mutant_agent_first_summary_wins` is the store's own test);
/// - the note after the cap is refused and recorded once, as refused (`mutant_agent_no_rate_cap`
///   accepts it);
/// - a blocking question moves the item to Needs you and the session is ended within the grace
///   period; then the credential is revoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn agent_calls_are_bound_capped_recorded_and_a_blocking_question_ends_the_session() {
    use bosun_api::rpc::code;
    use bosun_store::AgentCallKind;

    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Asks", "test -f never.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit a.txt a".to_owned(),
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit b.txt b".to_owned(),
            format!("write {} x", p("flag2")),
            format!("wait {}", p("release2")),
        ],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_file(&scene.path("flag1")).await;
    let credential = credential_of_session(&scene.path("work"));
    assert!(credential.starts_with("bosun-agent-"), "{credential}");
    let state = crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
    };
    let mut conn = crate::api::Connection::default();
    let own = |extra: serde_json::Value| {
        let mut params = serde_json::json!({"credential": credential, "item": "demo#1"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };

    let hello = rpc(&state, &mut conn, "agent.hello", own(serde_json::json!({}))).await;
    assert_eq!(hello["result"]["item"], "demo#1", "{hello}");
    assert_eq!(hello["result"]["attempt"], 1, "{hello}");
    assert_eq!(hello["result"]["identity"], "test", "{hello}");

    // Bound to its item.
    let forged = rpc(
        &state,
        &mut conn,
        "agent.note",
        serde_json::json!({"credential": credential, "item": "demo#2", "text": "forged"}),
    )
    .await;
    assert_eq!(
        error_code(&forged),
        Some(code::UNAUTHORIZED.into()),
        "{forged}"
    );
    // `agent.*` only: on its own connection, or carried to a fresh one.
    let tokens = rpc(
        &state,
        &mut conn,
        "tokens.by_model",
        serde_json::json!({"identity": "test"}),
    )
    .await;
    assert_eq!(
        error_code(&tokens),
        Some(code::UNAUTHORIZED.into()),
        "{tokens}"
    );
    let mut fresh = crate::api::Connection::default();
    let carried = rpc(
        &state,
        &mut fresh,
        "tokens.by_model",
        serde_json::json!({"identity": "test", "credential": credential}),
    )
    .await;
    assert_eq!(
        error_code(&carried),
        Some(code::UNAUTHORIZED.into()),
        "{carried}"
    );
    // Without a credential `agent.*` admits nothing.
    let bare = rpc(
        &state,
        &mut fresh,
        "agent.note",
        serde_json::json!({"credential": "bosun-agent-00", "item": "demo#1", "text": "x"}),
    )
    .await;
    assert_eq!(error_code(&bare), Some(code::UNAUTHORIZED.into()), "{bare}");

    // A summary that claims the verdict passed moves nothing.
    let transitions = scene.store.reader().transitions(&id).await.unwrap().len();
    let claim = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "verdict passed", "decisions": ["done"]})),
    )
    .await;
    assert!(claim["result"].is_object(), "{claim}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(scene.state(1).await, Some(State::Running));
    assert_eq!(
        scene.store.reader().transitions(&id).await.unwrap().len(),
        transitions
    );

    // Two summaries, each at its HEAD.
    let first = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "first", "followups": ["later"]})),
    )
    .await;
    let head1 = scene.git(&["rev-parse", "HEAD"]);
    assert_eq!(first["result"]["head_sha"], head1.as_str(), "{first}");
    std::fs::write(scene.path("release1"), "x").unwrap();
    wait_file(&scene.path("flag2")).await;
    let second = rpc(
        &state,
        &mut conn,
        "agent.submit_summary",
        own(serde_json::json!({"body": "second", "decisions": ["kept"]})),
    )
    .await;
    let head2 = scene.git(&["rev-parse", "HEAD"]);
    assert_ne!(head1, head2);
    assert_eq!(second["result"]["head_sha"], head2.as_str(), "{second}");
    let summary = scene
        .store
        .reader()
        .summary(&id, 1)
        .await
        .unwrap()
        .expect("a summary");
    assert_eq!(summary.body, "second");
    assert_eq!(summary.head_sha.as_deref(), Some(head2.as_str()));
    assert_eq!(
        summary.kind,
        AgentCallKind::Summary {
            decisions: vec!["kept".to_owned()],
            followups: Vec::new(),
        }
    );

    // Notes up to the cap, then one over it: refused, and recorded once, as refused.
    let cap = crate::agent::cap(crate::agent::Tool::Note);
    for n in 0..cap {
        let note = rpc(
            &state,
            &mut conn,
            "agent.note",
            own(serde_json::json!({"text": format!("note {n}")})),
        )
        .await;
        assert!(note["result"].is_object(), "{note}");
    }
    let over = rpc(
        &state,
        &mut conn,
        "agent.note",
        own(serde_json::json!({"text": "over the cap"})),
    )
    .await;
    assert_eq!(error_code(&over), Some(code::RATE_CAPPED.into()), "{over}");
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    let notes: Vec<_> = calls
        .iter()
        .filter(|c| c.kind == AgentCallKind::Note)
        .collect();
    assert_eq!(notes.len(), cap as usize + 1, "every call is a row");
    let refused: Vec<_> = notes.iter().filter(|c| c.refused).collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].body, "over the cap");

    // A blocking question: Needs you, and the session ends within the grace period.
    let asked = std::time::SystemTime::now();
    let question = rpc(
        &state,
        &mut conn,
        "agent.ask_question",
        own(serde_json::json!({"text": "which way?", "blocking": true})),
    )
    .await;
    assert!(question["result"].is_object(), "{question}");
    wait_for("needs you", Duration::from_secs(30), async || {
        scene.state(1).await == Some(State::NeedsYou(Reason::Question))
    })
    .await;
    wait_for("the session's end", Duration::from_secs(30), async || {
        let runs = scene.store.reader().runs(&id).await.unwrap();
        runs.last().is_some_and(|r| r.ended_ms.is_some())
    })
    .await;
    let ended = asked.elapsed().unwrap();
    assert!(
        ended < GRACE + Duration::from_secs(5),
        "the session ended {ended:?} after the question"
    );
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    assert!(
        calls
            .iter()
            .any(|c| c.kind == AgentCallKind::Question { blocking: true }
                && c.body == "which way?"
                && !c.refused)
    );
    // The session is gone, and its credential with it.
    let revoked = rpc(
        &state,
        &mut conn,
        "agent.note",
        own(serde_json::json!({"text": "late"})),
    )
    .await;
    assert_eq!(
        error_code(&revoked),
        Some(code::UNAUTHORIZED.into()),
        "{revoked}"
    );
    assert!(!scene.credentials.live_for(&id));

    task.abort();
    watcher.abort();
}

/// Waits for item `card` to end; at most `limit`.
async fn ended(scene: &Scene, card: u64, limit: Duration) -> State {
    let mut state = None;
    wait_for(&format!("item {card} to end"), limit, async || {
        state = scene.state(card).await;
        terminal(state)
    })
    .await;
    state.unwrap()
}

fn modes(runs: &[bosun_store::RunRow]) -> Vec<(u32, SpawnMode)> {
    runs.iter().map(|r| (r.attempt, r.mode)).collect()
}

/// #66: a limit message parks the item, the park timer (the default backoff, then a parsed
/// reset) runs out on the loop's clock, and the session is resumed by id; the attempt counter
/// does not move (`mutant_loop_limit_counts` reports the limit as a crash: the second one fails
/// the attempt).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_parks_and_resumes_without_counting() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Limited", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!(
                "once {} replay {}",
                m("m1"),
                Scene::stream("limit-no-reset.jsonl")
            ),
            format!("once {} exit 1", m("m2")),
            format!(
                "once {} replay {}",
                m("m3"),
                Scene::stream("limit-reset.jsonl")
            ),
            format!("once {} exit 1", m("m4")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);

    // Parked for the default backoff: it stays parked while the clock does not move.
    let mut park = None;
    wait_for(
        "the limit to park the item",
        Duration::from_secs(30),
        async || {
            park = match scene.state(1).await {
                Some(State::Parked(park)) => Some(park),
                _ => None,
            };
            park.is_some()
        },
    )
    .await;
    let park = park.unwrap();
    assert_eq!(park.reason, ParkReason::Limit);
    let until = park.until_ms.expect("a limit parks until a time");
    assert!(
        until > scene.clock.wall_ms() + 14 * 60_000,
        "the default backoff: {park:?}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(scene.state(1).await, Some(State::Parked(_))));
    assert_eq!(
        scene.runs(1).await.len(),
        1,
        "no resume before the park ends"
    );

    scene.clock.advance(Duration::from_mins(16));
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!(
        (item.attempts, item.failed),
        (1, 0),
        "the attempt counter did not move"
    );
    assert_eq!(item.uncounted, 2);
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        [
            (1, SpawnMode::Fresh),
            (1, SpawnMode::Resume),
            (1, SpawnMode::Resume)
        ]
    );
    assert!(
        runs.iter().all(|r| r.session_key == runs[0].session_key),
        "resumed by id: {runs:?}"
    );
    let parks = scene
        .store
        .reader()
        .transitions(&ItemId::new("demo#1"))
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.to == "parked")
        .count();
    assert_eq!(parks, 2);
}

/// #66: a resume that fails (the CLI finds no conversation) resets the tree to the last commit
/// and starts a fresh session of the same attempt, uncounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_resume_starts_a_fresh_session_from_the_last_commit() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(
        1,
        "Resume fails",
        "test -f one.txt && test -f kept.txt",
        "Item one.",
    )]);
    scene.script(
        1,
        &[
            format!("once {} commit kept.txt kept", m("m1")),
            format!("once {} write loose.txt uncommitted", m("m2")),
            format!(
                "once {} replay {}",
                m("m3"),
                Scene::stream("limit-reset.jsonl")
            ),
            format!("once {} exit 1", m("m4")),
            format!(
                "once {} replay {}",
                m("m5"),
                Scene::stream("crash-error-result.jsonl")
            ),
            format!("once {} exit 1", m("m6")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    // The loose file would fail the clean-tree precondition had the tree not been reset.
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "uncounted");
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        [
            (1, SpawnMode::Fresh),
            (1, SpawnMode::Resume),
            (1, SpawnMode::Reset)
        ]
    );
    assert_eq!(runs[1].session_key, runs[0].session_key);
    assert_ne!(runs[2].session_key, runs[0].session_key, "a fresh session");
    // Reset to the last commit, not to the base.
    assert!(scene.repo.join("kept.txt").exists());
    assert!(!scene.repo.join("loose.txt").exists());
}

/// #66: an agent that prints a fake limit forever cannot park its item forever: the cap of
/// uncounted parks sends it to Needs you, well inside the timeout (`mutant_loop_no_park_cap`
/// does not apply the cap: the test fails on its timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fake_limit_forever_ends_in_needs_you_at_the_cap() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Always limited", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("replay {}", Scene::stream("limit-reset.jsonl")),
            "exit 1".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::ParkCap));
    let cap = scene.config().daemon.park_cap;
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0));
    assert_eq!(scene.runs(1).await.len(), usize::try_from(cap).unwrap() + 1);
}

/// #66: an expired authentication is Needs you at once, with no retry
/// (`mutant_loop_auth_retries` retries it like a crash).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_authentication_is_needs_you_with_no_retry() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Logged out", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("replay {}", Scene::stream("auth-expired.jsonl")),
            "exit 1".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    // Time for a retry to show.
    tokio::time::sleep(Duration::from_millis(500)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::Auth));
    assert_eq!(scene.runs(1).await.len(), 1);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
}

/// #66: a simulated 20-minute sleep (the wall clock jumps, the monotonic clock does not). Item
/// 1's child died in it: a crash of the attempt, retried once uncounted by resuming. Item 2's
/// child lives: its session runs on and ends normally, with no second session
/// (`mutant_loop_skip_wake_check` never revalidates: item 1 waits on its dead session).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_sleep_a_dead_child_is_retried_once_uncounted_and_a_live_one_runs_on() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "Dies asleep", "test -f one.txt", "Item one."),
        card(2, "Sleeps through", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("once {} wait {}", p("m1"), p("never")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            format!("write {} x", p("flag2")),
            format!("wait {}", p("release2")),
            "commit two.txt two".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let checks = || scene.probe.calls.load(Ordering::SeqCst);

    // Item 1: the child is declared dead in the sleep.
    wait_file(&scene.path("flag1")).await;
    scene.probe.dead.store(true, Ordering::SeqCst);
    scene.clock.advance(Duration::from_mins(20));
    wait_for("the wake check", Duration::from_secs(20), async || {
        checks() > 0
    })
    .await;
    scene.probe.dead.store(false, Ordering::SeqCst);
    let state = ended(&scene, 1, Duration::from_secs(30)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed, item.uncounted), (1, 0, 1));
    assert_eq!(
        modes(&scene.runs(1).await),
        [(1, SpawnMode::Fresh), (1, SpawnMode::Resume)]
    );

    // Item 2: the child lives through the sleep.
    wait_file(&scene.path("flag2")).await;
    let before = checks();
    scene.clock.advance(Duration::from_mins(20));
    wait_for("the wake check", Duration::from_secs(20), async || {
        checks() > before
    })
    .await;
    std::fs::write(scene.path("release2"), "").unwrap();
    let state = ended(&scene, 2, Duration::from_secs(30)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    assert_eq!(modes(&scene.runs(2).await), [(1, SpawnMode::Fresh)]);
    assert_eq!(scene.item(2).await.unwrap().uncounted, 0);
}

/// The flag after `--mcp-config` in a session's argv, as `fake-agent`'s `argv` wrote it.
fn mcp_config_of(argv: &Path) -> PathBuf {
    let argv = std::fs::read_to_string(argv).unwrap();
    let mut words = argv.lines();
    words.find(|w| *w == "--mcp-config").expect("--mcp-config");
    PathBuf::from(words.next().expect("its file"))
}

/// The configuration is readable by the user only.
fn assert_private(path: &Path) {
    #[cfg(windows)]
    {
        let file = std::fs::File::open(path).unwrap();
        bosun_proc::pipe::OwnerOnly::new()
            .unwrap()
            .verify(&file)
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", path.display());
    }
}

/// #64, through the real `bosun-mcp` that `fake-agent` starts from the session's MCP
/// configuration:
///
/// - `tools/list` names exactly the three tools (`mutant_mcp_extra_tool` is `bosun-mcp`'s own
///   test);
/// - a tool call reaches the store through the API, as a row of the session's attempt;
/// - the configuration is outside the working tree (`mutant_mcp_config_in_tree` writes it
///   into the tree), readable by the user only, and gone after the session; the tree stays
///   clean and the item reaches Done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_call_reaches_the_store_through_bosun_mcp_and_the_configuration_stays_private() {
    use bosun_store::AgentCallKind;

    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Talks", "test -f done.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("argv {}", p("argv.txt")),
            format!("mcp-tools bosun {}", p("tools.txt")),
            r#"mcp bosun note {"text": "from the agent"}"#.to_owned(),
            format!("write {} x", p("flag")),
            format!("wait {}", p("release")),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_file(&scene.path("flag")).await;

    assert_eq!(
        std::fs::read_to_string(scene.path("tools.txt")).unwrap(),
        "ask_question\nsubmit_summary\nnote\n"
    );
    let calls = scene.store.reader().agent_calls(&id).await.unwrap();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].kind, AgentCallKind::Note);
    assert_eq!(calls[0].body, "from the agent");
    assert_eq!(calls[0].attempt, 1);
    assert!(!calls[0].refused);
    assert_eq!(
        calls[0].head_sha.as_deref(),
        Some(scene.git(&["rev-parse", "HEAD"]).as_str())
    );

    let config = mcp_config_of(&scene.path("argv.txt"));
    let repo = scene.repo.canonicalize().unwrap();
    let canonical = config.canonicalize().unwrap();
    assert!(
        !canonical.starts_with(&repo),
        "{} is in the working tree",
        config.display()
    );
    assert_private(&config);
    assert_eq!(scene.git(&["status", "--porcelain", "--ignored"]), "");

    std::fs::write(scene.path("release"), "").unwrap();
    wait_for("done", Duration::from_secs(60), async || {
        matches!(scene.state(1).await, Some(State::Done(_)))
    })
    .await;
    assert!(
        !config.exists(),
        "{} outlived its session",
        config.display()
    );
    task.abort();
    watcher.abort();
}

/// #64: a session whose `bosun-mcp` cannot start never says hello. It is killed at the
/// handshake's timeout and started again, uncounted (the attempt stays 1, nothing failed); the
/// second miss is Needs you (`handshake`). `mutant_mcp_skip_handshake` lets the session run on
/// without its tools, and the item reaches Done instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_whose_mcp_server_cannot_start_is_retried_once_then_needs_you() {
    let mut scene = Scene::new().await;
    scene.mcp = scene.path("bin").join("no-such-bosun-mcp");
    scene.handshake = Duration::from_secs(2);
    scene.write_board(&[card(1, "Toolless", "test -f done.txt", "Item one.")]);
    // Longer than the handshake: the timeout, not the end of the session, finds the miss.
    scene.script(
        1,
        &["sleep 5000".to_owned(), "commit done.txt done".to_owned()],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    wait_for("an end", Duration::from_secs(60), async || {
        matches!(
            scene.state(1).await,
            Some(State::NeedsYou(_) | State::Done(_))
        )
    })
    .await;
    assert_eq!(
        scene.state(1).await,
        Some(State::NeedsYou(Reason::Handshake))
    );
    let stored = scene.store.reader().item(&id).await.unwrap().unwrap().item;
    assert_eq!(
        stored.attempt.as_ref().map(|a| a.number),
        Some(1),
        "{stored:?}"
    );
    assert_eq!(stored.failed, 0, "{stored:?}");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 2);
    let runs = scene.store.reader().runs(&id).await.unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs.iter().all(|r| r.exit == Some(bosun_core::Exit::Crash)),
        "{runs:?}"
    );
    // Killed at the timeout, well before the script's end.
    for run in &runs {
        let lasted = run.ended_ms.unwrap() - run.started_ms;
        assert!(lasted < 4_500, "a session lasted {lasted} ms");
    }
    task.abort();
    watcher.abort();
}

#[path = "control_tests.rs"]
mod control;
