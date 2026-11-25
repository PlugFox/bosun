//! Worktree-mode sessions under the session shim (#8, "Process containment"; E3, E9, E16,
//! E17, E18, E19): the agent runs under a copy of `bosun-shim` named by the binary's digest,
//! gets its prompt through the shim's stdin file, is recorded in `process` with its record,
//! offsets and credential digest, and outlives the loop that started it.

use bosun_proc::shim::Record;
use bosun_store::ProcessRow;

use super::worktrees::item;
use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(120);

/// Every `process` row of `card`, gone or not.
async fn processes(scene: &Scene, card: u64) -> Vec<ProcessRow> {
    scene
        .store
        .reader()
        .processes_of(&item(card))
        .await
        .unwrap()
}

/// The script saves its standard input, waits until the test has seen the session's child and
/// its shim, commits a file and ends normally. The item ends Done; the prompt reached the agent
/// through the shim's stdin file; one `process` row was written for the session, with its
/// record under the session's directory, the shim's pid (not the agent's, which the attempt
/// recorded), the credential's digest, offsets past the output it read, and marked gone; and
/// the shim that ran is a byte-identical copy under `<daemon directory>/shim/<digest>/`
/// (`mutant_loop_shim_in_place` runs it where it was found, `mutant_loop_drop_shim_offsets`
/// never writes the offsets).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_in_worktree_mode_runs_under_a_copied_shim_and_is_recorded() {
    let scene = Scene::with_worktrees(None).await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Under the shim", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("stdin {}", p("prompt.txt")),
            format!("wait {}", p("go")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    // While the session waits: the agent as the attempt records it, and the shim's row.
    let mut agent = None;
    wait_for("the session's child and its shim", LIMIT, async || {
        let children = scene.store.reader().children().await.unwrap();
        agent = children
            .iter()
            .find(|c| c.item == item(1) && c.verdict_run.is_none())
            .map(|c| c.pid);
        agent.is_some() && !processes(&scene, 1).await.is_empty()
    })
    .await;
    std::fs::write(scene.path("go"), "").unwrap();
    let state = ended(&scene, 1, LIMIT).await;
    super::worktrees::closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let prompt = std::fs::read(scene.path("prompt.txt")).unwrap();
    assert!(!prompt.is_empty(), "the prompt did not reach the agent");
    assert_eq!(scene.spawner.shimmed.load(Ordering::SeqCst), 1);

    let rows = processes(&scene, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(row.session_key, runs[0].session_key);
    let session_dir = scene
        .path("work")
        .join("sessions")
        .join(&runs[0].session_key);
    assert!(
        Path::new(&row.record_path).starts_with(&session_dir),
        "{} is not under {}",
        row.record_path,
        session_dir.display()
    );
    assert_ne!(
        Some(row.pid),
        agent,
        "the row names the agent, not its shim"
    );
    assert!(row.stdout_offset > 0, "{row:?}");
    assert!(row.gone_ms.is_some(), "{row:?}");
    assert_eq!(row.credential_digest.as_ref().map(Vec::len), Some(32));

    let copies: Vec<PathBuf> = std::fs::read_dir(scene.path("state").join("shim"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy = copies[0].join(format!("bosun-shim{}", std::env::consts::EXE_SUFFIX));
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        std::fs::read(bosun_shim()).unwrap(),
        "{} is not a copy of the built shim",
        copy.display()
    );
}

/// The script writes a start file and waits for a release file that never comes. Once it has
/// started, the test reads the agent's key from the shim's record and aborts the loop's tasks,
/// as a dying daemon would drop them: every `Spawned` the loop held is dropped. Two seconds
/// later the agent still runs; the test then ends it by its recorded pid and start time, and
/// the shim exits after it (`mutant_loop_sessions_without_shim` makes the agent a plain child,
/// which dies with the dropped tree).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_outlives_the_loop_that_started_it() {
    let scene = Scene::with_worktrees(None).await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Outlives", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} started", p("started")),
            format!("wait {}", p("release")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("started")).await;
    let mut row = None;
    wait_for("the shim's row", LIMIT, async || {
        row = processes(&scene, 1).await.into_iter().next();
        row.is_some()
    })
    .await;
    let row = row.unwrap();
    // Read before the loop goes: its dropped session task may remove the session's directory.
    let record = Record::read(Path::new(&row.record_path)).unwrap();

    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let alive = bosun_proc::is_running(record.child);
    // Ended either way, so that nothing of the scene outlives it.
    let _ = bosun_proc::reap(record.child);
    assert!(alive, "the agent died with the loop that started it");
    wait_for("the shim to exit", LIMIT, async || {
        !bosun_proc::is_running(record.shim)
    })
    .await;
}

/// Reaps the agent of a record when dropped, so that nothing of a scene outlives it.
struct Reap(bosun_proc::ProcessKey);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = bosun_proc::reap(self.0);
    }
}

/// The causes of `card`'s transitions, in order.
async fn causes(scene: &Scene, card: u64) -> Vec<String> {
    scene
        .store
        .reader()
        .transitions(&item(card))
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.cause)
        .collect()
}

/// E22: the script writes a start file, waits for a release file, commits and ends. Once it
/// has started, the test stops the loop gracefully: it aborts only the configuration watcher,
/// which drops the configuration's sender, and the loop runs its end. Then the agent still
/// runs (its key from the shim's record, which the `process` row names), the row is not gone,
/// the run has no exit and the session's directory holds the record. A new loop on the same
/// scene re-attaches the session (`reattached`), the release ends it, and the item ends Done
/// with one attempt and one run (`mutant_loop_stop_kills_shim_sessions` kills the agent at
/// the stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graceful_stop_leaves_the_session_running_and_the_next_loop_reattaches() {
    let scene = Scene::with_worktrees(None).await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Detached", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} started", p("started")),
            format!("wait {}", p("release")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("started")).await;
    let mut row = None;
    wait_for("the shim's row with its pid", LIMIT, async || {
        row = processes(&scene, 1).await.into_iter().next();
        row.as_ref().is_some_and(|r| r.pid != 0)
    })
    .await;
    let record = Record::read(Path::new(&row.unwrap().record_path)).unwrap();
    let _reap = Reap(record.child);

    // The graceful stop: the configuration's sender goes, the loop leaves its `while`.
    watcher.abort();
    let _ = watcher.await;
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("the loop ends at the stop")
        .unwrap();

    assert!(
        bosun_proc::is_running(record.child),
        "the agent was killed at the stop"
    );
    let rows = processes(&scene, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].gone_ms, None, "{rows:?}");
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].exit, None, "{runs:?}");
    assert!(
        Path::new(&rows[0].record_path).is_file(),
        "the session's directory lost the shim's record"
    );

    let (task, watcher) = scene.start(None);
    wait_for("the re-attach", LIMIT, async || {
        scene.state(1).await == Some(State::Running)
            && causes(&scene, 1).await.iter().any(|c| c == "reattached")
    })
    .await;
    std::fs::write(scene.path("release"), "").unwrap();
    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.attempts, 1, "{item:?}");
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].exit, Some(bosun_core::Exit::Ok), "{runs:?}");
}

/// The scene's spawns through the shim, checking before each that the store already holds the
/// session's `process` row, and noting the shim each one started.
struct RowFirst {
    inner: Arc<Counting>,
    reader: bosun_store::Reader,
    runtime: tokio::runtime::Handle,
    /// Per spawn: whether the row was there, and the shim's key from its record.
    seen: std::sync::Mutex<Vec<(bool, Option<bosun_proc::ProcessKey>)>>,
}

impl ShimSessions for RowFirst {
    fn spawn_in(
        &self,
        shim: &bosun_proc::shim::ShimSpawner,
        spec: &ChildSpec,
        dir: &Path,
    ) -> std::io::Result<Spawned> {
        let record = bosun_proc::shim::session_record(dir);
        let path = record.to_string_lossy().into_owned();
        let rows = self
            .runtime
            .block_on(self.reader.processes())
            .unwrap_or_default();
        let before = rows.iter().any(|r| r.record_path == path);
        let spawned = self.inner.spawn_in(shim, spec, dir);
        let key = Record::read(&record).ok().map(|r| r.shim);
        self.seen.lock().unwrap().push((before, key));
        spawned
    }
}

/// The session's `process` row is in the store before its shim is spawned (`spawn_in` sees
/// it), and once the shim runs the row has its pid and start time
/// (`mutant_loop_process_row_after_spawn` writes the row after the spawn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_process_row_is_written_before_the_shim_starts() {
    let mut scene = Scene::with_worktrees(None).await;
    let sessions = Arc::new(RowFirst {
        inner: scene.spawner.clone(),
        reader: scene.store.reader().clone(),
        runtime: tokio::runtime::Handle::current(),
        seen: std::sync::Mutex::default(),
    });
    scene.sessions = Some(sessions.clone());
    scene.write_board(&[card(1, "Row first", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");

    let seen = sessions.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (before, shim) = seen[0];
    assert!(before, "the shim was spawned before its row was written");
    let shim = shim.expect("the shim's record");
    let rows = processes(&scene, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].pid, rows[0].pid_started_ms),
        (shim.pid, shim.start.raw().cast_signed()),
        "{rows:?}"
    );
}
