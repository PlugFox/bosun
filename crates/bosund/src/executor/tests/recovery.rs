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
use bosun_store::WorktreeState;

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
            .env(BUILT_SHIM_VAR, bosun_shim())
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
pub(super) fn child_log(scene: &Scene) -> String {
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

pub(super) async fn causes(scene: &Scene, n: u64) -> Vec<String> {
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
    // A child that dies with the daemon (Linux's parent-death signal, a Windows job) may still
    // be on its way out just after the kill: asked at once, it would count as a survivor, none
    // would be planted, and the check would pass with nothing left to reap
    // (`mutant_recover_skip_reap` went green that way). It is given a moment to go.
    let deadline = Instant::now() + Duration::from_secs(3);
    while recorded.iter().any(|k| bosun_proc::is_running(*k)) {
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

/// #8: as [`a_daemon_killed_mid_verdict_runs_it_again_uncounted`], in worktree mode: the lost
/// run's checkout is closed at the restart as no item's, the verdict runs again in a new one
/// and the item ends Done; every row of kind `verdict` is `removed` and the worktree root holds
/// only the clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_mid_verdict_in_worktree_mode_leaves_no_checkout() {
    let scene = Scene::with_worktrees(None).await;
    let vflag = sh_path(&scene.path("verdict-started"));
    let stop = sh_path(&scene.path("verdict-stop"));
    scene.write_board(&[card(
        1,
        "Lost verdict in its checkout",
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
    let lost = worktrees::verdict_rows(&scene, 1).await;
    assert!(
        lost.iter().any(|r| r.state == WorktreeState::Created),
        "the lost run's checkout is left: {lost:?}"
    );

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(90)).await;
    worktrees::closed(&scene, 1).await;
    let mut rows = Vec::new();
    wait_for("no checkout left", Duration::from_secs(30), async || {
        rows = worktrees::verdict_rows(&scene, 1).await;
        rows.iter().all(|r| r.state == WorktreeState::Removed)
            && worktrees::root_entries(&scene) == [workspace::CLONE]
    })
    .await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    std::fs::write(scene.path("verdict-stop"), "").unwrap();
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    all_gone(&recorded).await;
    assert!(rows.len() > lost.len(), "the verdict ran again: {rows:?}");
    for row in &rows {
        assert!(!Path::new(&row.path).exists(), "{}", row.path);
    }
    assert_eq!(worktrees::root_entries(&scene), [workspace::CLONE]);
}

/// #8 (E3): in worktree mode the item's `setup` marks its start outside the repository and
/// sleeps for two minutes (only its first run: once the mark is there it ends at once). The
/// daemon is killed once the mark is there and the worktree's row records the setup's child.
/// The restarted daemon reaps that child (one planted in its place where the platform took the
/// whole tree with the daemon), closes the worktree as no item's, and claims the item again:
/// it ends Done, the first worktree's row is `removed` and the root holds only the clone
/// (`mutant_loop_setup_child_unrecorded` never records the child).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_during_setup_leaves_no_setup_running() {
    let mut scene = Scene::with_worktrees(None).await;
    let started = scene.path("setup-started");
    let mark = sh_path(&started);
    scene.setup = Some(format!(
        "if [ -f {mark} ]; then exit 0; fi; echo x > {mark}; sleep 120"
    ));
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    scene.write_board(&[card(1, "Killed in setup", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let mut daemon = Spawned::start(&scene, Role::Daemon);
    wait_file(&started).await;
    let mut first = None;
    wait_for(
        "the setup's child on the worktree's row",
        Duration::from_secs(30),
        async || {
            first = worktrees::rows(&scene, 1)
                .await
                .into_iter()
                .find(|r| r.child.is_some());
            first.is_some()
        },
    )
    .await;
    let first = first.unwrap();
    let (pid, started_ms) = first.child.unwrap();
    let mut setup = bosun_proc::ProcessKey {
        pid,
        start: bosun_proc::StartTime::from_raw(started_ms.cast_unsigned()),
    };
    daemon.kill();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Where the daemon's tree went with it, a survivor stands in for the setup.
    let mut survivor = None;
    if !bosun_proc::is_running(setup) {
        let planted = Spawned::start(&scene, Role::Survivor);
        let start = bosun_proc::start_time(planted.pid())
            .unwrap()
            .expect("the survivor runs");
        setup = bosun_proc::ProcessKey {
            pid: planted.pid(),
            start,
        };
        scene
            .store
            .writer()
            .write(vec![Write::WorktreeChild {
                path: first.path.clone(),
                child: Some((setup.pid, start.raw().cast_signed())),
            }])
            .await
            .unwrap();
        survivor = Some(planted);
    }

    let (task, watcher) = scene.start(None);
    all_gone(&[setup]).await;
    let state = ended(&scene, 1, Duration::from_secs(90)).await;
    worktrees::closed(&scene, 1).await;
    wait_for(
        "the root to hold only the clone",
        Duration::from_secs(30),
        async || worktrees::root_entries(&scene) == [workspace::CLONE],
    )
    .await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    std::fs::write(scene.path("survivors-may-go"), "").unwrap();
    drop(survivor);
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    let rows = worktrees::rows(&scene, 1).await;
    let again = rows.iter().find(|r| r.path == first.path).unwrap();
    assert_eq!(again.state, WorktreeState::Removed, "{rows:?}");
    assert_eq!(again.child, None, "{rows:?}");
    assert!(rows.len() >= 2, "the item was claimed again: {rows:?}");
    assert!(!Path::new(&first.path).exists(), "{}", first.path);
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

/// The `process` rows of card `n`, gone or not.
async fn shims(scene: &Scene, n: u64) -> Vec<bosun_store::ProcessRow> {
    scene.store.reader().processes_of(&id(n)).await.unwrap()
}

/// An assistant text line of the stream format, which the runner turns into a live entry.
fn print_text(text: &str) -> String {
    format!(
        "print {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\
         \"text\":\"{text}\"}}]}}}}"
    )
}

/// A worktree-mode scene (#8) whose card 1 prints `before-the-kill`, writes `started`, waits
/// for `release`, prints `after-the-release`, commits `one.txt` and ends normally.
pub(super) async fn shimmed_session() -> Scene {
    let scene = Scene::with_worktrees(None).await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Under the shim", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            print_text("before-the-kill"),
            format!("write {} x", p("started")),
            format!("wait {}", p("release")),
            print_text("after-the-release"),
            "commit one.txt one".to_owned(),
        ],
    );
    scene
}

/// Runs a child daemon until card 1's session waits under its shim and its row holds the
/// offsets past the first line, then kills it. Returns the shim's record; the agent runs on.
pub(super) async fn kill_under_shim(scene: &Scene) -> bosun_proc::shim::Record {
    let mut daemon = Spawned::start(scene, Role::Daemon);
    wait_file(&scene.path("started")).await;
    let mut row = None;
    wait_for(
        "the shim's row and its offsets",
        Duration::from_secs(30),
        async || {
            row = shims(scene, 1).await.into_iter().next();
            row.as_ref().is_some_and(|r| r.stdout_offset > 0)
        },
    )
    .await;
    let row = row.unwrap();
    let record = bosun_proc::shim::Record::read(Path::new(&row.record_path)).unwrap();
    daemon.kill();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        bosun_proc::is_running(record.child),
        "the agent died with its daemon: {}",
        child_log(scene)
    );
    record
}

/// Ends the agent of `record` if it still runs, so that nothing outlives the scene.
pub(super) fn end_agent(record: &bosun_proc::shim::Record) {
    let _ = bosun_proc::reap(record.child);
}

/// The test loop's API over the scene's credentials, as `bosun-mcp` calls it.
pub(super) fn api_of(scene: &Scene) -> crate::state::State {
    crate::state::State {
        started_ms: 0,
        identities: BTreeMap::new(),
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
        leases: Arc::new(crate::leases::Leases::none()),
        gh: std::path::PathBuf::from("gh"),
    }
}

/// Waits until the item's re-attach is recorded.
pub(super) async fn reattached(scene: &Scene) {
    wait_for("the re-attach", Duration::from_secs(60), async || {
        causes(scene, 1).await.iter().any(|c| c == "reattached")
    })
    .await;
}

/// #8 (E3, E10): in worktree mode the daemon is killed while the session waits; the agent
/// runs on under its shim. The restarted daemon finds the item `running`, re-attaches the
/// session and finishes it: one attempt, one run (fresh, `ok`), nothing uncounted, a
/// `reattached` transition and no `restarted`, the shim's row gone; the session's credential is
/// accepted again before the release (`mutant_loop_reap_instead_of_attach` reaps and resumes
/// it, `mutant_loop_reattach_without_credential` refuses the call).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_killed_mid_session_reattaches_and_finishes_with_one_attempt_and_one_run() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    let credential = credential_of_session(&scene.path("work"));

    let (task, watcher) = scene.start(None);
    reattached(&scene).await;
    let running = scene.state(1).await;
    let api = api_of(&scene);
    let mut conn = crate::api::Connection::default();
    let note = rpc(
        &api,
        &mut conn,
        "agent.note",
        serde_json::json!({"credential": credential, "item": "demo#1", "text": "still here"}),
    )
    .await;
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    let log = child_log(&scene);
    assert_eq!(running, Some(State::Running), "{log}");
    assert!(
        note["result"].is_object(),
        "the credential is refused after the restart: {note}"
    );
    assert_eq!(state, State::Done(Outcome::Pass), "{log}");

    let item = scene.item(1).await.unwrap();
    assert_eq!(
        (item.attempts, item.failed, item.uncounted),
        (1, 0, 0),
        "{item:?}"
    );
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs),
        vec![(1, SpawnMode::Fresh)],
        "one run: {runs:?}"
    );
    assert_eq!(runs[0].exit, Some(bosun_core::Exit::Ok), "{runs:?}");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let causes = causes(&scene, 1).await;
    assert_eq!(
        causes.iter().filter(|c| *c == "reattached").count(),
        1,
        "{causes:?}"
    );
    assert!(
        !causes.iter().any(|c| c == "restarted"),
        "the session is not restarted: {causes:?}"
    );
    let rows = shims(&scene, 1).await;
    assert!(
        !rows.is_empty() && rows.iter().all(|r| r.gone_ms.is_some()),
        "{rows:?}"
    );
}

/// #8 (E3): the session ends while no daemon runs, and its shim records the exit. The
/// restarted daemon reads the rest of its output and the recorded exit, and the item ends Done
/// with one run, nothing resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_ended_while_the_daemon_was_down_is_read_from_its_record() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    std::fs::write(scene.path("release"), "").unwrap();
    let path = PathBuf::from(&shims(&scene, 1).await[0].record_path);
    wait_for(
        "the shim to record the exit",
        Duration::from_secs(60),
        async || bosun_proc::shim::Record::read(&path).is_ok_and(|r| r.exit.is_some()),
    )
    .await;
    assert!(!bosun_proc::is_running(record.child));

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.uncounted), (1, 0), "{item:?}");
    let runs = scene.runs(1).await;
    assert_eq!(modes(&runs), vec![(1, SpawnMode::Fresh)], "{runs:?}");
    assert_eq!(runs[0].exit, Some(bosun_core::Exit::Ok), "{runs:?}");
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    let causes = causes(&scene, 1).await;
    assert!(causes.iter().any(|c| c == "reattached"), "{causes:?}");
    assert!(!causes.iter().any(|c| c == "restarted"), "{causes:?}");
}

/// #8 (E9): the kill switch reaches a re-attached session as any other: within 5 s its agent
/// is gone, by its recorded pid and start time, and the item is parked as `killed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kill_switch_ends_a_reattached_session() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    let (task, watcher) = scene.start(None);
    reattached(&scene).await;

    let thrown = Instant::now();
    scene.control.set_killed(true);
    wait_for("the agent to be gone", Duration::from_secs(5), async || {
        !bosun_proc::is_running(record.child)
    })
    .await;
    assert!(thrown.elapsed() <= Duration::from_secs(5));
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
    task.abort();
    watcher.abort();
    end_agent(&record);
}

/// #8: an item in flight is recovered in the mode it was claimed in. A worktree-mode item's
/// daemon is killed mid-session; meanwhile the user commits on the target and removes
/// `repo.demo.worktree` from the configuration. The restarted daemon sends the item to Needs
/// you (`config_changed`), closes its worktree with its work kept in the clone, and leaves the
/// user's checkout alone: no `refs/bosun/` ref there, HEAD, branch and status as the user left
/// them (`mutant_loop_mode_from_config` rebuilds the item as a stage-1 one and sets it aside in
/// the user's checkout).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_item_recovered_without_the_worktree_key_leaves_the_users_checkout_alone() {
    let mut scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    scene.git(&["commit", "--allow-empty", "-q", "-m", "user"]);
    scene.worktrees = false;
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    let user = |scene: &Scene| {
        (
            scene.git(&["rev-parse", "HEAD"]),
            scene.git(&["symbolic-ref", "HEAD"]),
            scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        )
    };
    let before = user(&scene);

    let (task, watcher) = scene.start(None);
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    worktrees::closed(&scene, 1).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    let log = child_log(&scene);
    assert_eq!(state, State::NeedsYou(Reason::ConfigChanged), "{log}");

    assert_eq!(
        scene.git(&["for-each-ref", "refs/bosun/"]),
        "",
        "the user's repository got a bosun ref"
    );
    assert_eq!(user(&scene), before, "the user's checkout changed");
    let rows = worktrees::rows(&scene, 1).await;
    let items: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == bosun_store::WorktreeKind::Item)
        .collect();
    assert!(
        !items.is_empty() && items.iter().all(|r| r.state == WorktreeState::Removed),
        "{rows:?}"
    );
    let clone = scene.worktree_root().join(workspace::CLONE);
    let aside = git::aside_ref(&id(1));
    assert!(
        !worktrees::git_in(&scene, &clone, &["for-each-ref", &aside]).is_empty(),
        "the item's work is not kept in the clone"
    );
}

/// The texts of the session output events `sub` has received so far.
fn output_lines(sub: &mut crate::events::Subscriber) -> Vec<String> {
    match sub.take(10_000) {
        crate::events::Batch::Events(events) => events
            .iter()
            .filter_map(|e| {
                let v: serde_json::Value = serde_json::from_slice(&e.line).unwrap();
                v["params"]["body"]["line"].as_str().map(str::to_owned)
            })
            .collect(),
        crate::events::Batch::Lagged(_) => panic!("the subscription lagged"),
    }
}

/// #8 (E18): the restarted daemon reads the session from the offsets its row holds: the line
/// the session printed before the kill is not published again, the line after the release is
/// (`mutant_loop_reattach_from_zero` reads it all again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattached_session_is_read_from_its_stored_offsets() {
    let scene = shimmed_session().await;
    let record = kill_under_shim(&scene).await;
    let filter = bosun_api::EventFilter {
        kinds: Some(vec!["session_output".to_owned()]),
        tail: Some(bosun_api::Tail::default()),
    };
    let mut sub = scene.events.subscribe("test", None, None, filter).1;
    let (task, watcher) = scene.start(None);
    reattached(&scene).await;
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    let lines = output_lines(&mut sub);
    assert!(
        lines.iter().any(|l| l == "after-the-release"),
        "the output after the release: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l == "before-the-kill"),
        "the output before the kill is read again: {lines:?}"
    );
}

/// #8: attempt 1's session commits `one.txt` and its verdict (`test -f two.txt`) fails;
/// attempt 2's session writes `started`, waits for `release` and commits `two.txt`. The daemon
/// is killed while attempt 2 waits; the test's loop re-attaches the session and the release
/// ends it. The commit range the store records for attempt 2's run starts at attempt 1's head
/// and holds exactly the `two.txt` commit, not attempt 1's (`mutant_loop_attach_range_from_base`
/// starts it at the item's base). The range's base is checked on the run row: a commit is
/// listed under the first run that recorded it, so the stored list alone cannot show it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattached_session_counts_only_its_own_commits() {
    let scene = Scene::with_worktrees(None).await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Own commits", "test -f two.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("once {} commit one.txt one", p("once-commit")),
            format!(
                "once {} replay {}",
                p("once-replay"),
                Scene::stream("normal-end.jsonl")
            ),
            format!("once {} exit 0", p("once-exit")),
            print_text("attempt-two"),
            format!("write {} x", p("started")),
            format!("wait {}", p("release")),
            "commit two.txt two".to_owned(),
        ],
    );
    let mut daemon = Spawned::start(&scene, Role::Daemon);
    wait_file(&scene.path("started")).await;
    let mut row = None;
    wait_for(
        "attempt 2's shim row and its offsets",
        Duration::from_secs(60),
        async || {
            row = shims(&scene, 1)
                .await
                .into_iter()
                .find(|r| r.attempt == 2 && r.pid != 0);
            row.as_ref().is_some_and(|r| r.stdout_offset > 0)
        },
    )
    .await;
    let record = bosun_proc::shim::Record::read(Path::new(&row.unwrap().record_path)).unwrap();
    daemon.kill();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let alive = bosun_proc::is_running(record.child);
    if !alive {
        end_agent(&record);
    }
    assert!(
        alive,
        "the agent died with its daemon: {}",
        child_log(&scene)
    );

    let (task, watcher) = scene.start(None);
    reattached(&scene).await;
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    task.abort();
    watcher.abort();
    end_agent(&record);
    assert_eq!(state, State::Done(Outcome::Pass), "{}", child_log(&scene));
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.attempts, 2, "{item:?}");

    // The range recorded on attempt 2's run: from where its session started (attempt 1's
    // head), so it lists the `two.txt` commit alone.
    let runs = scene.runs(1).await;
    let first = runs.iter().find(|r| r.attempt == 1).unwrap();
    let second = runs.iter().find(|r| r.attempt == 2).unwrap();
    let (base, head) = (
        second.base_sha.clone().unwrap(),
        second.head_sha.clone().unwrap(),
    );
    assert_eq!(Some(&base), first.head_sha.as_ref(), "{runs:?}");
    assert_eq!(second.start_sha.as_ref(), Some(&base), "{runs:?}");
    let listed = scene.git(&["log", "--format=%s", &format!("{base}..{head}")]);
    assert_eq!(listed, "two", "{runs:?}");

    let subjects_of = stored_commits(&scene).await;
    let ranges = scene.store.reader().run_commits(0, i64::MAX).await.unwrap();
    let subjects = |attempt: u32| -> Vec<&str> {
        ranges
            .iter()
            .filter(|r| r.item == id(1) && r.attempt == attempt)
            .map(|r| subjects_of[&r.commit.sha].as_str())
            .collect()
    };
    assert_eq!(subjects(2), ["two"], "{ranges:?}");
    assert_eq!(subjects(1), ["one"], "{ranges:?}");
}

/// Stores every commit of the scene's repository, as the ingester would, so that the store's
/// reader of commit ranges lists them with their sessions. Returns their subjects by sha.
async fn stored_commits(scene: &Scene) -> BTreeMap<String, String> {
    let config = scene.config();
    let (repo, _) = lookup(&config, "demo").unwrap();
    let log = scene.git(&["log", "--all", "--format=%H %ct %s"]);
    let mut subjects = BTreeMap::new();
    let writes: Vec<Write> = log
        .lines()
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            let sha = parts.next().unwrap().to_owned();
            let ts: i64 = parts.next().unwrap().parse().unwrap();
            subjects.insert(sha.clone(), parts.next().unwrap_or_default().to_owned());
            Write::Commit {
                repo: repo.key.clone(),
                commit: bosun_core::Commit {
                    sha,
                    ts_ms: ts * 1000,
                    author: "Synthetic Agent <agent@example.com>".to_owned(),
                    lines_added: 1,
                    lines_removed: 0,
                    trailers: BTreeMap::new(),
                },
            }
        })
        .collect();
    scene.store.writer().write(writes).await.unwrap();
    subjects
}
