//! Crash recovery (#68), the way #19's abort test does it: the daemon's loop runs in a child
//! process (this test binary again, running [`child_daemon`] on the scene's directory), which is
//! killed without warning at a failpoint, or aborts itself at one. The test process is then the
//! restarted daemon: its own loop recovers the item from the same store, board and checkout.
//!
//! The child is the real loop on the real local-file queue, store, git repository and verdict
//! shell, with `fake-agent` as the agent and the real `bosun-mcp`; only the login probe is a
//! fake, as in the rest of the scenario suite. Its sessions run in their own containers, so the
//! kill takes them with it (on Windows the job's kill-on-close); a child that outlives its
//! daemon, as one can on Unix, is planted where the platform leaves none.

use std::process::{Child as Process, Stdio};

use bosun_core::publish::report_key;
use bosun_queue::{Changes, Comment, Cursor, Owned, QueueError};

use super::*;

/// The scene's directory, for the child process; unset in a normal test run.
const ROOT_VAR: &str = "BOSUN_TEST_RECOVERY_ROOT";

/// What the child process does: [`Role`].
const ROLE_VAR: &str = "BOSUN_TEST_RECOVERY_ROLE";

/// The child's test, run with `--exact`.
const CHILD_TEST: &str = "executor::tests::recovery::child_daemon";

/// What a child process plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The daemon's loop, until it is killed.
    Daemon,
    /// The daemon's loop on a board that aborts the process right after the first sticky
    /// report write is carried out: between the board write and its acknowledgement.
    DaemonDyingAfterReport,
    /// A process that outlived its daemon: it sleeps until killed (or a while).
    Survivor,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::DaemonDyingAfterReport => "daemon-dying-after-report",
            Self::Survivor => "survivor",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Daemon, Self::DaemonDyingAfterReport, Self::Survivor]
            .into_iter()
            .find(|r| r.name() == name)
    }
}

/// The local file, aborting the process once the first report write is on the board.
#[derive(Debug)]
struct DyingAfterReport(LocalFile);

impl Queue for DyingAfterReport {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        self.0.poll(cursor)
    }

    fn project(
        &self,
        item: bosun_queue::ItemId,
        state: Owned,
        note: Option<&str>,
    ) -> Result<(), QueueError> {
        self.0.project(item, state, note)
    }

    fn comment(&self, item: bosun_queue::ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        self.0.comment(item, body, key)
    }

    fn upsert_comment(
        &self,
        item: bosun_queue::ItemId,
        key: &str,
        body: &str,
    ) -> Result<(), QueueError> {
        self.0.upsert_comment(item, key, body)?;
        eprintln!("child daemon: the report is written; dying before its acknowledgement");
        std::process::abort();
    }

    fn add_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.0.add_label(item, label)
    }

    fn remove_label(&self, item: bosun_queue::ItemId, label: &str) -> Result<(), QueueError> {
        self.0.remove_label(item, label)
    }

    fn lint_target(&self, item: bosun_queue::ItemId) -> Result<Card, QueueError> {
        self.0.lint_target(item)
    }
}

/// The child process's side: nothing in a normal test run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_daemon() {
    let (Some(root), Some(role)) = (std::env::var_os(ROOT_VAR), std::env::var_os(ROLE_VAR)) else {
        return;
    };
    let role = Role::parse(&role.to_string_lossy()).expect("a known role");
    let root = PathBuf::from(root);
    if role == Role::Survivor {
        // Long enough to be found; it ends by itself should nothing kill it.
        let deadline = Instant::now() + Duration::from_mins(3);
        while Instant::now() < deadline && !root.join("survivors-may-go").exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        return;
    }
    let scene = Scene::reopen(root).await;
    let queue: board::Shared = match role {
        Role::DaemonDyingAfterReport => Arc::new(DyingAfterReport(LocalFile::new(&scene.board))),
        _ => scene.queue(),
    };
    let (_task, _watcher) = scene.start_with(None, queue);
    eprintln!("child daemon {} runs", std::process::id());
    std::future::pending::<()>().await;
}

/// A child process of this test.
struct Spawned(Process);

impl Spawned {
    fn start(scene: &Scene, role: Role) -> Self {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(scene.path("child.log"))
            .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(ROOT_VAR, scene.dir.path())
            .env(ROLE_VAR, role.name())
            .env(BUILT_AGENT_VAR, fake_agent())
            .env(BUILT_MCP_VAR, bosun_mcp())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Kills it without warning (`TerminateProcess`, `SIGKILL`) and waits for it to be gone.
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    /// Waits for it to end by itself.
    async fn ended(&mut self, limit: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the child daemon did not end");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.kill();
    }
}

fn id(n: u64) -> ItemId {
    ItemId::new(format!("demo#{n}"))
}

fn key_of(child: &bosun_store::RecordedChild) -> bosun_proc::ProcessKey {
    bosun_proc::ProcessKey {
        pid: child.pid,
        start: bosun_proc::StartTime::from_raw(child.started_ms.cast_unsigned()),
    }
}

async fn children(scene: &Scene) -> Vec<bosun_store::RecordedChild> {
    scene.store.reader().children().await.unwrap()
}

/// The scene's child log, for a failure message.
fn child_log(scene: &Scene) -> String {
    std::fs::read_to_string(scene.path("child.log")).unwrap_or_default()
}

/// Waits until none of `keys` runs.
async fn all_gone(keys: &[bosun_proc::ProcessKey]) {
    let keys = keys.to_vec();
    wait_for(
        "no recorded child left",
        Duration::from_secs(10),
        async || keys.iter().all(|k| !bosun_proc::is_running(*k)),
    )
    .await;
}

/// The daemon's comments on card `n`, by key.
fn keyed(scene: &Scene, n: u64) -> Vec<Comment> {
    scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == n)
        .expect("the card is on the board")
        .comments
        .into_iter()
        .filter(|c| c.key.is_some())
        .collect()
}

/// Every keyed comment once, one claim marker and one sticky report.
fn one_of_each(scene: &Scene, n: u64) {
    let comments = keyed(scene, n);
    let keys: Vec<&str> = comments.iter().filter_map(|c| c.key.as_deref()).collect();
    let unique: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
    assert_eq!(keys.len(), unique.len(), "a duplicate comment: {keys:?}");
    let report = report_key(&id(n));
    let reports = keys.iter().filter(|k| k.starts_with(&report)).count();
    assert_eq!(reports, 1, "one sticky report: {keys:?}");
    let markers = keys.iter().filter(|k| k.starts_with("claim:")).count();
    assert_eq!(markers, 1, "one claim marker: {keys:?}");
}

async fn causes(scene: &Scene, n: u64) -> Vec<String> {
    scene
        .store
        .reader()
        .transitions(&id(n))
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.cause)
        .collect()
}

/// Waits for the outbox of the scene's repository to be empty.
async fn drained(scene: &Scene) {
    wait_for("the outbox to drain", Duration::from_secs(60), async || {
        outbox::pending(scene.store.reader(), "demo")
            .await
            .unwrap()
            .is_empty()
    })
    .await;
}

/// The session of card 1 writes `flag1`, then the first one waits forever (the daemon dies
/// meanwhile) and a resumed one commits `one.txt`.
fn lost_session(scene: &Scene) {
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Lost session", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("once {} wait {}", p("once1"), p("never")),
            "commit one.txt one".to_owned(),
        ],
    );
}

/// Runs a child daemon until card 1's session is up and recorded, then kills it. Returns the
/// children it had recorded.
async fn kill_mid_session(scene: &Scene) -> Vec<bosun_proc::ProcessKey> {
    let mut daemon = Spawned::start(scene, Role::Daemon);
    wait_file(&scene.path("flag1")).await;
    wait_for(
        "the session's child row",
        Duration::from_secs(30),
        async || !children(scene).await.is_empty(),
    )
    .await;
    let recorded: Vec<_> = children(scene).await.iter().map(key_of).collect();
    daemon.kill();
    recorded
}

/// What a daemon that died can leave on Unix: a child that outlived it. Where the platform
/// took the whole tree (a Windows job), one is planted as the attempt's recorded child, so
/// reaping is exercised everywhere.
async fn plant_survivor(
    scene: &Scene,
    recorded: &[bosun_proc::ProcessKey],
) -> Option<(Spawned, bosun_proc::ProcessKey)> {
    if recorded.iter().any(|k| bosun_proc::is_running(*k)) {
        return None;
    }
    let survivor = Spawned::start(scene, Role::Survivor);
    let start = bosun_proc::start_time(survivor.pid())
        .unwrap()
        .expect("the survivor runs");
    let key = bosun_proc::ProcessKey {
        pid: survivor.pid(),
        start,
    };
    scene
        .store
        .writer()
        .write(vec![Write::SessionChild {
            item: id(1),
            attempt: 1,
            pid: key.pid,
            pid_started_ms: start.raw().cast_signed(),
        }])
        .await
        .unwrap();
    Some((survivor, key))
}

/// #68: the daemon is killed in the middle of a session and started again. No process it had
/// recorded is left running (`mutant_recover_skip_reap` leaves the survivor), the session is
/// resumed exactly once and uncounted (`mutant_recover_counts_crash` takes the lost session for
/// a crash, which uses up the attempt's crash retry), and the board has one of each comment and
/// the store one of each transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_mid_session_resumes_once_and_leaves_no_child() {
    let scene = Scene::new().await;
    lost_session(&scene);
    let mut recorded = kill_mid_session(&scene).await;
    let survivor = plant_survivor(&scene, &recorded).await;
    if let Some((_, key)) = &survivor {
        recorded.push(*key);
    }

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(90)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let log = child_log(&scene);
    assert_eq!(state, State::Done(Outcome::Pass), "{log}");

    all_gone(&recorded).await;
    assert!(
        children(&scene).await.is_empty(),
        "every child row is cleared"
    );

    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "{item:?}");
    let attempts = scene.store.reader().attempts(&id(1)).await.unwrap();
    assert_eq!(
        attempts[0].attempt.crash_retries, 0,
        "the lost session is not a crash of the attempt"
    );
    assert_eq!(
        modes(&scene.runs(1).await),
        vec![(1, SpawnMode::Fresh), (1, SpawnMode::Resume)],
        "resumed exactly once"
    );
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);

    let causes = causes(&scene, 1).await;
    for once in ["claim", "claim_confirmed", "restarted"] {
        assert_eq!(
            causes.iter().filter(|c| *c == once).count(),
            1,
            "{once} once: {causes:?}"
        );
    }
    one_of_each(&scene, 1);
    std::fs::write(scene.path("survivors-may-go"), "").unwrap();
}

/// #68: killed during a verdict run: the verdict runs again under a new run number, and the
/// attempt is neither counted nor flaky (`mutant_recover_counts_crash` takes the lost run for a
/// run that did not pass: the item ends flaky).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_mid_verdict_runs_it_again_uncounted() {
    let scene = Scene::new().await;
    let vflag = sh_path(&scene.path("verdict-started"));
    let stop = sh_path(&scene.path("verdict-stop"));
    scene.write_board(&[card(
        1,
        "Lost verdict",
        &format!(
            "test -f one.txt || exit 1; if [ ! -f {vflag} ]; then echo x > {vflag}; \
             while [ ! -f {stop} ]; do sleep 0.2; done; fi"
        ),
        "Item one.",
    )]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let mut daemon = Spawned::start(&scene, Role::Daemon);
    wait_file(&scene.path("verdict-started")).await;
    wait_for(
        "the verdict's child row",
        Duration::from_secs(30),
        async || {
            children(&scene)
                .await
                .iter()
                .any(|c| c.verdict_run.is_some())
        },
    )
    .await;
    let recorded: Vec<_> = children(&scene).await.iter().map(key_of).collect();
    daemon.kill();

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(90)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    std::fs::write(scene.path("verdict-stop"), "").unwrap();
    let log = child_log(&scene);
    assert_eq!(state, State::Done(Outcome::Pass), "{log}");
    all_gone(&recorded).await;

    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "{item:?}");
    let runs = scene.store.reader().verdict_runs(&id(1)).await.unwrap();
    let numbers: Vec<(u32, u32, Option<bosun_core::RunResult>)> =
        runs.iter().map(|r| (r.attempt, r.run, r.result)).collect();
    assert_eq!(
        numbers,
        vec![(1, 1, None), (1, 2, Some(bosun_core::RunResult::Pass))],
        "the lost run, then the run again"
    );
    assert_eq!(scene.runs(1).await.len(), 1, "no session after the restart");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    one_of_each(&scene, 1);
}

/// #68: the daemon dies right after a board write is carried out, before it is acknowledged
/// in the outbox. The restarted daemon sends the row again under the same key: the board shows
/// one sticky report (`mutant_outbox_no_key` sends it under a new key: a second report).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_between_a_board_write_and_its_acknowledgement_leaves_one_write() {
    let scene = Scene::new().await;
    lost_session(&scene);
    let mut daemon = Spawned::start(&scene, Role::DaemonDyingAfterReport);
    let status = daemon.ended(Duration::from_secs(60)).await;
    assert!(!status.success(), "{}", child_log(&scene));
    // The write is on the board, and the outbox still holds it.
    let report = report_key(&id(1));
    assert_eq!(
        keyed(&scene, 1)
            .iter()
            .filter(|c| c.key.as_deref().is_some_and(|k| k.starts_with(&report)))
            .count(),
        1,
        "the report was written"
    );
    let rows = outbox::pending(scene.store.reader(), "demo").await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.op == bosun_store::OutboxOp::UpsertComment),
        "the report's row is not acknowledged: {rows:?}"
    );

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(90)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    one_of_each(&scene, 1);
}

/// #68: the user cancels the item (the card to Backlog) while the daemon is down. Recovery
/// reads the board first: the item is cancelled and never resumed
/// (`mutant_recover_resume_before_board` resumes it first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_made_while_the_daemon_was_down_wins_over_the_resume() {
    let scene = Scene::new().await;
    lost_session(&scene);
    let base = scene.git(&["rev-parse", "HEAD"]);
    let recorded = kill_mid_session(&scene).await;
    let mut cards = scene.cards();
    cards[0].column = Column::Backlog;
    scene.write_board(&cards);

    let (task, watcher) = scene.start(None);
    let mut state = None;
    wait_for("item 1 to settle", Duration::from_secs(60), async || {
        state = scene.state(1).await;
        state.is_some_and(|s| !s.is_active())
    })
    .await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, Some(State::Cancelled), "{}", child_log(&scene));
    all_gone(&recorded).await;
    let causes = causes(&scene, 1).await;
    assert!(
        !causes.iter().any(|c| c == "restarted"),
        "the lost session is not resumed: {causes:?}"
    );
    assert_eq!(scene.runs(1).await.len(), 1, "no session after the restart");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    assert_eq!(scene.git(&["rev-parse", "HEAD"]), base);
    assert_eq!(scene.git(&["status", "--porcelain"]), "");
    assert_eq!(
        scene.cards()[0].column,
        Column::Backlog,
        "the user's move stays"
    );
}

/// #68: the session had printed its successful result when the daemon died: its output kept
/// on disk says it ended, so the restarted daemon verifies it rather than resuming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_reported_its_result_is_verified_not_resumed() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Ended", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit one.txt one".to_owned(),
            format!("replay {}", Scene::stream("normal-end.jsonl")),
            format!("wait {}", p("never")),
        ],
    );
    let mut daemon = Spawned::start(&scene, Role::Daemon);
    let tails = scene.path("work").join("tails");
    wait_for(
        "the result in the kept output",
        Duration::from_secs(60),
        async || {
            std::fs::read_dir(&tails).is_ok_and(|entries| {
                entries.filter_map(Result::ok).any(|e| {
                    std::fs::read_to_string(e.path())
                        .is_ok_and(|t| t.contains("\"type\":\"result\""))
                })
            })
        },
    )
    .await;
    daemon.kill();

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.failed, item.uncounted), (0, 0), "{item:?}");
    assert_eq!(scene.runs(1).await.len(), 1, "not resumed");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    assert!(
        std::fs::read_dir(&tails).is_ok_and(|mut e| e.next().is_none()),
        "the kept output goes with its session"
    );
}
