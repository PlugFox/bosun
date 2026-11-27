//! The loop's notices (#9): a card that goes to Needs you, a Ready column the loop drained and
//! a repository the fast-death breaker paused each queue one notice in the identity's
//! notification outbox. No drainer runs in these scenes, so the rows stay pending.

use std::collections::BTreeMap;

use bosun_store::{NewChannel, NoticeRow};

use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(120);

/// Writes the channel `ops` for `events` to the scene's store.
async fn channel(scene: &Scene, events: &[&str]) {
    scene
        .store
        .writer()
        .write(vec![Write::ChannelAdded(NewChannel {
            name: "ops".to_owned(),
            kind: "fake".to_owned(),
            params: BTreeMap::from([("sink".to_owned(), "executor-tests-notify".to_owned())]),
            events: events.iter().map(|e| (*e).to_owned()).collect(),
            min_gap_ms: 0,
            secret_entry: "notify/test/ops".to_owned(),
            at_ms: 1,
        })])
        .await
        .unwrap();
}

async fn pending(scene: &Scene) -> Vec<NoticeRow> {
    scene.store.reader().pending_notices("ops").await.unwrap()
}

fn of<'a>(rows: &'a [NoticeRow], event: &str) -> Vec<&'a NoticeRow> {
    rows.iter().filter(|r| r.event == event).collect()
}

/// Card 1 passes on its base (Needs you, lint, at its claim) and card 2 commits `two.txt`
/// (Done): one `needs_you` notice for card 1, and one `ready_drained` notice after card 2, not
/// one more for the idle polls after it (`mutant_loop_no_drained_notice`: none).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_needs_you_card_and_a_drained_ready_column_queue_one_notification_each() {
    let scene = Scene::new().await;
    channel(&scene, &["needs_you", "ready_drained"]).await;
    scene.write_board(&[
        card(1, "Passes on base", "true", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    assert_eq!(ended(&scene, 1, LIMIT).await, State::NeedsYou(Reason::Lint));
    assert_eq!(ended(&scene, 2, LIMIT).await, State::Done(Outcome::Pass));
    wait_for("the ready_drained notice", LIMIT, async || {
        !of(&pending(&scene).await, "ready_drained").is_empty()
    })
    .await;
    // Several more polls of an idle board.
    tokio::time::sleep(Duration::from_secs(3)).await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;

    let rows = pending(&scene).await;
    let needs_you = of(&rows, "needs_you");
    assert_eq!(needs_you.len(), 1, "{rows:#?}");
    assert_eq!(needs_you[0].item.as_deref(), Some("demo#1"));
    assert_eq!(needs_you[0].facts["reason"], "lint");
    assert!(
        needs_you[0].key.starts_with("needs_you:demo#1/t"),
        "{rows:#?}"
    );
    let drained = of(&rows, "ready_drained");
    assert_eq!(drained.len(), 1, "{rows:#?}");
    assert_eq!(drained[0].item.as_deref(), Some("demo#2"));
    assert!(
        drained[0].key.starts_with("ready_drained:demo/demo#2/"),
        "{rows:#?}"
    );
    assert_eq!(drained[0].repo, "demo");
    assert_eq!(rows.len(), 2, "{rows:#?}");
}

/// Card 1 passes on its base (Needs you, lint, at its claim) and the column is drained; the
/// user puts it back in Ready, it is claimed again and lands (Done), and the column is drained
/// again: two `ready_drained` notices of the same last item, one per claim, the text the same
/// (`mutant_loop_drained_key_per_item`: the second has the first's key and is dropped).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_drain_after_a_reclaim_queues_a_second_notice() {
    let scene = Scene::new().await;
    channel(&scene, &["ready_drained"]).await;
    scene.write_board(&[card(1, "Passes on base", "true", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    assert_eq!(ended(&scene, 1, LIMIT).await, State::NeedsYou(Reason::Lint));
    wait_for("the first ready_drained notice", LIMIT, async || {
        !of(&pending(&scene).await, "ready_drained").is_empty()
    })
    .await;
    let seq = scene.item(1).await.unwrap().seq;
    // The board's own outbox has moved card 1 to Needs you: no later write of it undoes the
    // move back below.
    drained(&scene).await;

    // Back in Ready, with a verdict the agent's commit meets.
    let mut cards = scene.cards();
    let one = cards.iter_mut().find(|c| c.id.0 == 1).unwrap();
    *one = card(1, "Needs work now", "test -f one.txt", "Item one, edited.");
    scene.write_board(&cards);
    wait_for("item 1 to end again", LIMIT, async || {
        scene
            .item(1)
            .await
            .is_some_and(|i| i.seq > seq && terminal(Some(i.state)))
    })
    .await;
    assert_eq!(scene.state(1).await, Some(State::Done(Outcome::Pass)));
    wait_for("the second ready_drained notice", LIMIT, async || {
        of(&pending(&scene).await, "ready_drained").len() >= 2
    })
    .await;
    // Several more polls of an idle board.
    tokio::time::sleep(Duration::from_secs(3)).await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;

    let rows = pending(&scene).await;
    let drained = of(&rows, "ready_drained");
    assert_eq!(drained.len(), 2, "{rows:#?}");
    assert_ne!(drained[0].key, drained[1].key, "{rows:#?}");
    for row in &drained {
        assert!(
            row.key.starts_with("ready_drained:demo/demo#1/"),
            "{rows:#?}"
        );
        assert_eq!(row.item.as_deref(), Some("demo#1"));
        assert_eq!(row.facts, drained[0].facts, "the same text: {rows:#?}");
    }
}

/// An agent that dies at once in every session trips the breaker (as in `fast_death`): one
/// `repo_paused` notice for `demo`, with the breaker's figures
/// (`mutant_loop_no_breaker_notice`: none).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_breaker_pause_queues_one_repo_paused_notification() {
    let scene = Scene::new().await;
    channel(&scene, &["repo_paused"]).await;
    scene.write_board(&[
        card(1, "Dies at once", "test -f one.txt", "Item one."),
        card(2, "Waits", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["exit 1".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_for("the breaker to trip", LIMIT, async || {
        super::fast_death::tripped(&scene).is_some()
            || matches!(scene.state(1).await, Some(State::NeedsYou(_)))
    })
    .await;
    // Several more polls, the repository paused.
    tokio::time::sleep(Duration::from_secs(2)).await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;

    let rows = pending(&scene).await;
    let paused = of(&rows, "repo_paused");
    assert_eq!(paused.len(), 1, "{rows:#?}");
    assert_eq!(paused[0].repo, "demo");
    assert!(paused[0].key.starts_with("repo_paused:demo/"), "{rows:#?}");
    assert_eq!(paused[0].facts["sessions"], "3");
    assert_eq!(paused[0].facts["within_s"], "60");
    assert_eq!(paused[0].item.as_deref(), Some("demo#1"));
    assert_eq!(rows.len(), 1, "{rows:#?}");
}
