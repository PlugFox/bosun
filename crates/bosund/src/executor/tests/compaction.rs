//! The compaction instructions of a session (#7): the session is started with `--settings` of
//! a file in its directory whose `PreCompact` hook prints `compact.md`, and that command, run
//! through `sh` as Claude Code's hook shell runs it, prints the item's instructions.

use super::*;

/// While the session of `demo#1` waits, its hook command is read from the settings file its
/// argv names and run through `sh`: it prints the item, its verdict, its protected path and the
/// paragraph that no approval may be attributed to the user. `mutant_runner_compaction_hook_dropped`
/// leaves the settings without a hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pre_compact_hook_of_a_session_prints_its_compaction_instructions() {
    let scene = Scene::new().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "Compacts", "test -f file1.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("argv {}", p("argv1")),
            format!("wait {}", p("go1")),
            "commit file1.txt x".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    // The argv is written in one go; wait until the whole of it can be read (its last line is
    // the agent's last flag's value, so a line after `--settings` is enough).
    let mut argv = String::new();
    wait_for("the session's argv", Duration::from_secs(120), async || {
        argv = std::fs::read_to_string(scene.path("argv1")).unwrap_or_default();
        argv.lines()
            .skip_while(|a| *a != "--settings")
            .nth(1)
            .is_some()
            || argv.lines().any(|a| a == "--permission-mode")
    })
    .await;
    let argv: Vec<&str> = argv.lines().collect();
    let at = argv
        .iter()
        .position(|a| *a == "--settings")
        .unwrap_or_else(|| panic!("no `--settings` in {argv:?}"));
    let settings = std::fs::read_to_string(argv[at + 1]).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings).unwrap();
    let command = settings
        .pointer("/hooks/PreCompact/0/hooks/0/command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no PreCompact hook in {settings}"))
        .to_owned();

    let sh = bosun_queue::lint::verdict_shell(None).expect("a POSIX shell");
    let out = std::process::Command::new(sh)
        .arg("-c")
        .arg(&command)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{command}: {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    for part in [
        "demo#1",
        "test -f file1.txt",
        "docs/frozen",
        "Do not attribute any approval",
    ] {
        assert!(
            stdout.contains(part),
            "the hook's output lacks {part:?}: {stdout}"
        );
    }

    std::fs::write(scene.path("go1"), "").unwrap();
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    drained(&scene).await;
    task.abort();
    watcher.abort();
}
