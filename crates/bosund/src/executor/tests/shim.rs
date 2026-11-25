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
