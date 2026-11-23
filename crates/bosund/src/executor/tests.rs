//! The scenario suite of the loop (#62): `fake-agent` as the agent CLI (behind a `.cmd` wrapper
//! on Windows, as npm installs `claude`), the local-file queue, a real git repository, a real
//! store, the real verdict shell. Nothing touches the user's own configuration, agent or daemon:
//! every path is in a temporary directory and the login probe is a fake.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bosun_config::{Config, DiskProbe, SystemProbe};
use bosun_core::{Outcome, Reason, State};
use bosun_identity::{Bundle, ProbeError, TokenLogin};
use bosun_proc::{ChildSpec, NativeSpawner, Spawned, Spawner};
use bosun_queue::{Card, Column, LocalFile, Queue};
use bosun_store::{Options as StoreOptions, Store};
use tokio::sync::watch;

use super::*;
use crate::config_watch;
use crate::events::{Events, Options as EventOptions};

/// The login every identity check sees.
const LOGIN: &str = "synthetic-login";

/// `fake-agent`, built next to this test binary.
fn fake_agent() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut build = Command::new(cargo);
        build
            .args([
                "build",
                "--locked",
                "--package",
                "fake-agent",
                "--message-format=json",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(std::process::Stdio::inherit());
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        let out = build.output().unwrap();
        assert!(out.status.success(), "building fake-agent failed");
        // Where cargo put it, whatever the target and build directories are.
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|m| m["target"]["name"] == "fake-agent")
            .find_map(|m| m["executable"].as_str().map(PathBuf::from))
            .expect("cargo names the fake-agent executable")
    })
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
        let scene = Self {
            repo: d.join("repo"),
            board: d.join("board.toml"),
            config_path: d.join("config.toml"),
            scripts: d.join("scripts"),
            gitconfig,
            store,
            spawner: Arc::new(Counting::default()),
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
        text.push_str(&format!("\nreplay {}\n", replay.display()));
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
            failpoint,
        };
        let deps = Deps {
            reader: self.store.reader().clone(),
            writer: self.store.writer().clone(),
            events: Events::new("test", EventOptions::default()),
            queue: Arc::new(LocalFile::new(&self.board)),
            spawner: self.spawner.clone(),
            power: Arc::new(bosun_proc::FakePower::default()),
            logins: Arc::new(FixedLogins),
        };
        let task = tokio::spawn(Loop::new(options, deps, rx).run());
        (task, watcher)
    }

    async fn state(&self, card: u64) -> Option<State> {
        self.store
            .reader()
            .item(&ItemId::new(format!("demo#{card}")))
            .await
            .unwrap()
            .map(|s| s.item.state)
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
    assert!(reader.runs(&id(3)).await.unwrap().is_empty());
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
/// carries a spawn, nothing is spawned (`mutant_loop_effect_before_transition` spawns first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_transition_leaves_no_spawn_without_its_row() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let refused = Arc::new(AtomicUsize::new(0));
    let seen = refused.clone();
    let failpoint: Failpoint = Arc::new(move |t: &bosun_core::Transition| {
        let spawns = t
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Spawn { .. }));
        if spawns {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        spawns
    });
    let (task, watcher) = scene.start(Some(failpoint));
    // The loop halts once the store refuses a transition.
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("the loop stops after a refused transition")
        .unwrap();
    watcher.abort();
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    // A spawn performed before its row would show up here.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        scene.spawner.spawns.load(Ordering::SeqCst),
        0,
        "a session was spawned without its transition"
    );
    let id = ItemId::new("demo#1");
    assert!(scene.store.reader().runs(&id).await.unwrap().is_empty());
    assert_eq!(scene.state(1).await, Some(State::Claimed));
}
