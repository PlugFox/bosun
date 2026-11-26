//! The effort level of a session (#7, #9): the card's `effort`, else `repo.<name>.effort`,
//! passed as `--effort <level>` and recorded on the attempt; with neither, no flag and nothing
//! recorded.

use super::*;

fn id(n: u64) -> ItemId {
    ItemId::new(format!("demo#{n}"))
}

/// The argument after `--effort` in the argv the fake agent of item `n` wrote.
fn effort_of(scene: &Scene, n: u64) -> Option<String> {
    let argv = std::fs::read_to_string(scene.path(&format!("argv{n}"))).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert!(
        argv.iter().filter(|a| **a == "--effort").count() <= 1,
        "one `--effort` at most: {argv:?}"
    );
    argv.iter()
        .position(|a| *a == "--effort")
        .map(|i| argv[i + 1].to_owned())
}

/// The sticky report of item `n`.
fn report_of(scene: &Scene, n: u64) -> String {
    let cards = scene.cards();
    let card = cards.iter().find(|c| c.id.0 == n).unwrap();
    card.comments
        .iter()
        .find(|c| c.key.as_deref() == Some(&*format!("report:demo#{n}")))
        .map(|c| c.body.clone())
        .expect("the sticky report")
}

/// Runs the scene's cards `numbers` to their end, each script writing its argv and committing.
async fn run_to_done(scene: &Scene, numbers: &[u64]) {
    let p = |name: &str| scene.path(name).display().to_string();
    for &n in numbers {
        scene.script(
            n,
            &[
                format!("argv {}", p(&format!("argv{n}"))),
                format!("commit file{n}.txt x"),
            ],
        );
    }
    let (task, watcher) = scene.start(None);
    for &n in numbers {
        let state = ended(scene, n, Duration::from_secs(60)).await;
        assert_eq!(state, State::Done(Outcome::Pass), "item {n}");
    }
    drained(scene).await;
    task.abort();
    watcher.abort();
}

/// The repository asks for `high`; card 1 names no effort and gets it, card 2 names `low` and
/// gets that. `mutant_runner_card_effort_ignored` gives card 2 `high`;
/// `mutant_store_attempt_effort_dropped` leaves both attempts' effort empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_card_effort_overrides_the_repository_effort_and_is_recorded() {
    let scene = Scene::new().await;
    let text = scene
        .config_text("'docs/frozen'")
        .replace("model = 'opus'\n", "model = 'opus'\neffort = 'high'\n");
    assert!(text.contains("effort = 'high'"), "{text}");
    std::fs::write(&scene.config_path, text).unwrap();
    let mut low = card(2, "Overrides", "test -f file2.txt", "Item two.");
    low.body = low
        .body
        .replace("verdict = ", "effort = \"low\"\nverdict = ");
    scene.write_board(&[card(1, "Defaults", "test -f file1.txt", "Item one."), low]);
    run_to_done(&scene, &[1, 2]).await;

    for (n, want) in [(1, "high"), (2, "low")] {
        assert_eq!(effort_of(&scene, n).as_deref(), Some(want), "item {n}");
        let attempts = scene.store.reader().attempts(&id(n)).await.unwrap();
        let first = attempts.iter().find(|a| a.attempt.number == 1).unwrap();
        assert_eq!(first.effort.as_deref(), Some(want), "item {n}'s attempt");
        let report = report_of(&scene, n);
        let line = format!("; effort `{want}`");
        assert!(report.contains(&line), "{line:?} missing from {report}");
    }
}

/// Neither the card nor the repository names an effort: no flag, nothing recorded, nothing
/// reported. `mutant_runner_effort_default_medium` passes `--effort medium`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_an_effort_no_flag_is_passed_and_none_is_recorded() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Defaults", "test -f file1.txt", "Item one.")]);
    run_to_done(&scene, &[1]).await;

    assert_eq!(effort_of(&scene, 1), None, "no `--effort`");
    let attempts = scene.store.reader().attempts(&id(1)).await.unwrap();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    for attempt in &attempts {
        assert_eq!(attempt.effort, None, "attempt {}", attempt.attempt.number);
    }
    let report = report_of(&scene, 1);
    assert!(!report.contains("effort"), "{report}");
}
