//! The automatic pause in worktree mode (#7, #8 E5): the items of a repository with
//! `repo.demo.worktree` work in the daemon's clone, not in the user's checkout, so an
//! interactive session there does not pause the repository unless `repo.demo.auto_pause = true`
//! asks for it.

use bosun_core::Hold;

use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(120);

/// A scene in worktree mode with the global `auto_pause` at its default (true), the checkout
/// registered as a repository root, and two cards: card 1 writes `flag1` and waits for
/// `release1`, card 2 commits at once. `repo_keys` goes into `[repo.demo]`.
async fn opened(repo_keys: &str) -> Scene {
    let scene = Scene::with_worktrees(None).await;
    let text = scene.config_text("'docs/frozen'").replace(
        "[repo.demo.policy]",
        &format!("{repo_keys}[repo.demo.policy]"),
    );
    std::fs::write(&scene.config_path, text).unwrap();
    scene.register_root().await;
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            format!("wait {}", p("release1")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(2, &["commit two.txt two".to_owned()]);
    scene
}

/// #7, #8: in worktree mode an interactive session in the user's checkout does not pause the
/// repository by default (`mutant_control_auto_pause_global_only` reads `daemon.auto_pause`
/// alone and pauses it); with `repo.demo.auto_pause = true` it does, and the next card waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_repository_is_not_paused_by_interactive_work_unless_it_opts_in() {
    // (a) The default: no pause.
    let scene = opened("").await;
    assert!(scene.config().daemon.auto_pause, "the global key is on");
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    scene.usage(1, "interactive-session", 10).await;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let now = scene.clock.wall_ms();
        assert_eq!(
            scene.control.hold("test", "demo", now),
            None,
            "interactive work in the checkout paused a repository in worktree mode"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::write(scene.path("release1"), "").unwrap();
    let one = ended(&scene, 1, LIMIT).await;
    let two = ended(&scene, 2, LIMIT).await;
    task.abort();
    watcher.abort();
    assert_eq!(one, State::Done(Outcome::Pass));
    assert_eq!(two, State::Done(Outcome::Pass));

    // (b) The repository opts in: the same work pauses it.
    let scene = opened("auto_pause = true\n").await;
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    scene.usage(1, "interactive-session", 10).await;
    let now = scene.clock.wall_ms();
    wait_for("the automatic pause", Duration::from_secs(10), async || {
        scene.control.hold("test", "demo", now) == Some(Hold::Pause)
    })
    .await;
    std::fs::write(scene.path("release1"), "").unwrap();
    assert_eq!(
        ended(&scene, 1, LIMIT).await,
        State::Done(Outcome::Pass),
        "the attempt in flight finishes"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        scene.control.hold("test", "demo", scene.clock.wall_ms()),
        Some(Hold::Pause)
    );
    assert_eq!(scene.state(2).await.unwrap_or(State::Ready), State::Ready);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);

    // Idle for the default 30 minutes: the pause lifts by itself.
    scene.clock.advance(Duration::from_mins(31));
    let two = ended(&scene, 2, LIMIT).await;
    task.abort();
    watcher.abort();
    assert_eq!(two, State::Done(Outcome::Pass));
}
