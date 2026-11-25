//! The process sweep in the loop (#8, "Process containment: the sweep"; E12): with
//! `repo.<name>.sweep.processes`, a listed process that started during an attempt is killed
//! after the attempt's session, reported and noted on the item; one of the same name that
//! started before the attempt is not touched.
//!
//! The only name these scenes sweep is the unique random name of a copy of `fake-agent` the
//! scene itself made in its temporary directory; the only processes they kill are their own
//! children, by their handles.

use std::process::{Child, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::executor::sweep::{NOTE_SWEEP, Swept};

/// How long the item of the scene may take.
const LIMIT: Duration = Duration::from_secs(180);

/// A unique name of at most 15 characters: `bsw` and 12 hex digits of the clock and the pid,
/// mixed.
fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    let mixed =
        (nanos ^ (u128::from(std::process::id()) << 64)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    #[expect(clippy::cast_possible_truncation, reason = "12 hex digits are kept")]
    let low = (mixed >> 40) as u64 & 0xffff_ffff_ffff;
    format!("bsw{low:012x}")
}

/// The test's own children, killed by their handles when the test ends, however it ends.
#[derive(Default)]
struct Strays(Vec<Child>);

impl Drop for Strays {
    fn drop(&mut self) {
        for child in &mut self.0 {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

/// Copies the executable `from` to `to`. On Unix through a child `cp`, never a descriptor of
/// this process, for the reason [`write_executable`] gives (`ETXTBSY`).
fn copy_executable(from: &Path, to: &Path) {
    #[cfg(unix)]
    {
        let status = Command::new("cp").arg(from).arg(to).status().unwrap();
        assert!(status.success(), "copying to {}: {status}", to.display());
    }
    #[cfg(not(unix))]
    std::fs::copy(from, to).unwrap();
}

/// Starts the stray at `program` with nothing to read or write; it sleeps its script.
fn start_stray(program: &Path) -> Child {
    without_session(&mut Command::new(program))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start a stray")
}

/// Whether `child` exits within `limit`.
fn exits_within(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A copy of `fake-agent` under a unique name sleeps its script (`<name>.script` next to it).
/// The repository's sweep lists the name. One copy starts before the card is put in Ready, a
/// second once the session has started (between its start file and its release). After the
/// item ends Done the second is gone within 5 s, the first still runs, and the item's `sweep`
/// note names the second's pid only (`mutant_loop_sweep_ignores_start_time` kills the first,
/// `mutant_loop_sweep_unlogged` leaves no note).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_stray_started_during_the_attempt_is_killed_and_an_older_one_is_not() {
    let mut scene = Scene::new().await;
    let name = unique_name();
    let strays_dir = scene.path("strays");
    std::fs::create_dir_all(&strays_dir).unwrap();
    let stray = strays_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    copy_executable(fake_agent(), &stray);
    std::fs::write(strays_dir.join(format!("{name}.script")), "sleep 600000\n").unwrap();
    scene.config_more = format!("[repo.demo.sweep]\nprocesses = ['{name}']\n");
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();

    let mut strays = Strays::default();
    strays.0.push(start_stray(&stray));
    let older = strays.0[0].id();
    // The attempt starts later than this on the wall clock, by more than its rounding.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (started, release) = (scene.path("started"), scene.path("release"));
    scene.write_board(&[card(1, "Swept", "test -f done.txt", "Leave a stray.")]);
    scene.script(
        1,
        &[
            format!("write {} x", started.display()),
            format!("wait {}", release.display()),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_for("the session to start", LIMIT, async || {
        started.exists() || terminal(scene.state(1).await)
    })
    .await;
    strays.0.push(start_stray(&stray));
    let newer = strays.0[1].id();
    std::fs::write(&release, "x").unwrap();

    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    let newer_gone = exits_within(&mut strays.0[1], Duration::from_secs(5));
    let older_alive = strays.0[0].try_wait().unwrap().is_none();
    let note = scene
        .store
        .reader()
        .item_note(&ItemId::new("demo#1"), NOTE_SWEEP)
        .await
        .unwrap();
    // The test ends the older copy itself, by its own handle.
    drop(strays);

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert!(
        newer_gone,
        "the stray started during the attempt (pid {newer}) still runs"
    );
    assert!(
        older_alive,
        "the stray started before the attempt (pid {older}) was killed"
    );
    let note: Vec<Swept> = serde_json::from_str(&note.expect("a sweep note")).unwrap();
    assert_eq!(note, [Swept { name, pid: newer }]);
}
