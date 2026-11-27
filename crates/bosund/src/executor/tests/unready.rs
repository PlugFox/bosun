//! A verdict exit code that means "not ready", not "red" (#204), on the loop: the repository's
//! `verdict.unready_exit` parks the item for `unready_backoff` and wakes it into the verdict
//! again, with no session, no new attempt and no counted failure; past `unready_cap` the item
//! is Needs you (`resource_unready`), its comment quoting the `unready` note.

use super::*;

/// A stage 1 scene whose repository has the not-ready code 75, a backoff of 5 minutes and a cap
/// of 2, and card 1: its session commits `one.txt`, its verdict is not ready (exit 75) until
/// `ready.flag` (outside the repository) exists, and then checks `one.txt`.
async fn scene() -> Scene {
    let scene = Scene::new().await;
    let plain = "verdict = { timeout = '60s' }";
    let text = scene.config_text("'docs/frozen'");
    assert!(text.contains(plain), "{text}");
    let text = text.replace(
        plain,
        "verdict = { timeout = '60s', unready_exit = 75, unready_backoff = '5m', \
         unready_cap = 2 }",
    );
    std::fs::write(&scene.config_path, text).unwrap();
    let flag = sh_path(&scene.path("ready.flag"));
    let verdict = format!("test -f '{flag}' || exit 75; test -f one.txt");
    scene.write_board(&[Card {
        body: format!("Item one.\n\n```bosun\nverdict = '''{verdict}'''\n```\n"),
        ..card(1, "Waits for a resource", "true", "")
    }]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene
}

/// Waits until item 1 is parked, and returns its park.
async fn parked(scene: &Scene) -> bosun_core::Park {
    let mut park = None;
    wait_for("item 1 to park", Duration::from_secs(60), async || {
        park = match scene.state(1).await {
            Some(State::Parked(park)) => Some(park),
            _ => None,
        };
        park.is_some()
    })
    .await;
    park.unwrap()
}

/// The verdict runs of item 1: their results and exit codes.
async fn verdicts(scene: &Scene) -> Vec<(Option<bosun_core::RunResult>, Option<i64>)> {
    scene
        .store
        .reader()
        .verdict_runs(&ItemId::new("demo#1"))
        .await
        .unwrap()
        .iter()
        .map(|v| (v.result, v.exit_code))
        .collect()
}

/// The daemon's comments on card 1 that are neither the claim marker nor the sticky report.
fn needs_you_comments(scene: &Scene) -> Vec<String> {
    scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == 1)
        .expect("the card is on the board")
        .comments
        .into_iter()
        .filter(|c| {
            !c.key
                .as_deref()
                .is_some_and(|k| k.starts_with("claim:") || k.starts_with("report:"))
        })
        .map(|c| c.body)
        .collect()
}

/// The verdict exits 75 while the flag is missing: the item parks with `unready` for about the
/// backoff and stays parked while the clock does not move; with the flag made and the clock past
/// the backoff the verdict runs again, with no session in between, and passes: Done (pass) on
/// one attempt, nothing failed, nothing uncounted, and the sticky report counts the not-ready
/// run apart from the verdict (`mutant_loop_unready_counts` maps the code to a failed run: the
/// item never parks).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_not_ready_verdict_waits_and_runs_again_without_a_session_or_a_counted_attempt() {
    use bosun_core::RunResult;
    let scene = scene().await;
    let (task, watcher) = scene.start(None);

    let park = parked(&scene).await;
    assert_eq!(park.reason, ParkReason::Unready, "{park:?}");
    let until = park.until_ms.expect("a not-ready run parks until a time");
    let now = scene.clock.wall_ms();
    assert!(
        until > now + 4 * 60_000 && until <= now + 5 * 60_000,
        "the backoff of 5 minutes: {park:?} at {now}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        matches!(scene.state(1).await, Some(State::Parked(_))),
        "parked until the backoff ends"
    );
    assert_eq!(
        verdicts(&scene).await.len(),
        1,
        "no verdict before the park ends"
    );

    std::fs::write(scene.path("ready.flag"), "").unwrap();
    scene.clock.advance(Duration::from_mins(6));
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!(
        (item.attempts, item.failed, item.uncounted),
        (1, 0, 0),
        "{item:?}"
    );
    assert_eq!(scene.runs(1).await.len(), 1, "one session");
    assert_eq!(
        verdicts(&scene).await,
        [
            (Some(RunResult::Unready), Some(75)),
            (Some(RunResult::Pass), Some(0))
        ]
    );
    let report = scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == 1)
        .unwrap()
        .comments
        .into_iter()
        .find(|c| c.key.as_deref() == Some("report:demo#1"))
        .map(|c| c.body)
        .expect("the sticky report");
    assert!(report.contains("after 1 not-ready run"), "{report}");
}

/// The flag never comes: each time the item parks the clock moves past the backoff. The third
/// not-ready run is past the cap of 2: Needs you (`resource_unready`) after one session, with no
/// failed attempt, its `unready` note naming the code and its key, quoted by its one Needs-you
/// comment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verdict_that_stays_not_ready_goes_to_needs_you_at_the_cap() {
    use bosun_core::RunResult;
    let scene = scene().await;
    let (task, watcher) = scene.start(None);

    let mut state = None;
    wait_for("item 1 to end", Duration::from_secs(120), async || {
        state = scene.state(1).await;
        if matches!(state, Some(State::Parked(_))) {
            scene.clock.advance(Duration::from_mins(6));
        }
        terminal(state)
    })
    .await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, Some(State::NeedsYou(Reason::ResourceUnready)));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "{item:?}");
    assert_eq!(scene.runs(1).await.len(), 1, "one session");
    assert_eq!(
        verdicts(&scene).await,
        [(Some(RunResult::Unready), Some(75)); 3]
    );
    let note = scene
        .store
        .reader()
        .item_note(&ItemId::new("demo#1"), outbox::NOTE_UNREADY)
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(note.contains("exited 75"), "{note}");
    assert!(note.contains("repo.demo.verdict.unready_exit"), "{note}");
    assert!(note.contains("3 of unready_cap 2"), "{note}");
    let comments = needs_you_comments(&scene);
    assert_eq!(comments.len(), 1, "{comments:#?}");
    assert!(
        comments[0].contains("A resource the verdict needs was not ready: run 3 of the verdict"),
        "{}",
        comments[0]
    );
    // The comment is Markdown: the note's underscores are escaped.
    assert!(
        comments[0].contains(r"exited 75 (repo.demo.verdict.unready\_exit)"),
        "{}",
        comments[0]
    );
}
