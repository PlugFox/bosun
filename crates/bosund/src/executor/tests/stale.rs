//! A stale transition costs one card, not the repository (#132): the store refuses an arrow the
//! loop computed from a view of the item older than its stored history, and the item goes to
//! Needs you with the refusal while the rest of the board keeps running.

use super::*;

/// The text of the synthetic refusal of the scenes.
const REFUSAL: &str = "synthetic: demo#1/t1 is stored as another arrow";

/// A failpoint that refuses, once, the first transition of `demo#1` that `matches`, as stale.
fn refuse_once(
    matches: impl Fn(&bosun_core::Transition) -> bool + Send + Sync + 'static,
) -> Failpoint {
    let refused = AtomicBool::new(false);
    Arc::new(move |t: &bosun_core::Transition| {
        (t.key.item == ItemId::new("demo#1") && matches(t) && !refused.swap(true, Ordering::SeqCst))
            .then(|| bosun_store::Error::Stale(REFUSAL.to_owned()))
    })
}

/// Card 1 fails lint at claim (its verdict passes on the base) and card 2 commits `two.txt`;
/// the store refuses card 1's `lint_failed` arrow once as stale. Card 1 goes to Needs you
/// (stale) with the refusal stored as its note and quoted in its one comment, card 2 ends Done,
/// and the loop is still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_bounce_sends_its_card_to_needs_you_and_the_loop_goes_on() {
    let scene = Scene::new().await;
    scene.write_board(&[
        card(1, "Passes on base", "true", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let failpoint = refuse_once(|t| t.cause == bosun_core::Event::LintFailed.name());
    let (task, watcher) = scene.start(Some(failpoint));
    assert_eq!(
        ended(&scene, 2, Duration::from_secs(120)).await,
        State::Done(Outcome::Pass)
    );
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(120)).await,
        State::NeedsYou(Reason::Stale)
    );
    let id = ItemId::new("demo#1");
    let note = scene
        .store
        .reader()
        .item_note(&id, outbox::NOTE_STALE)
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(note.contains(REFUSAL), "{note}");
    drained(&scene).await;
    let cards = scene.cards();
    let one = cards.iter().find(|c| c.id.0 == 1).unwrap();
    assert_eq!(one.column, Column::NeedsYou, "{one:#?}");
    // Besides the claim marker, one comment: the Needs-you one, which quotes the refusal.
    let needs_you: Vec<&str> = one
        .comments
        .iter()
        .filter(|c| !c.key.as_deref().is_some_and(|k| k.starts_with("claim:")))
        .map(|c| c.body.as_str())
        .collect();
    assert_eq!(needs_you.len(), 1, "{:#?}", one.comments);
    assert!(
        needs_you[0].contains("older than its stored history"),
        "{}",
        needs_you[0]
    );
    assert!(
        needs_you[0].contains("is stored as another arrow"),
        "{}",
        needs_you[0]
    );
    assert!(!task.is_finished(), "the loop stopped");
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
}

/// Cards 1 and 2 each commit a file; the store refuses, once and as stale, the transition of
/// item 1 that runs its verdict. Item 1 goes to Needs you (stale) after its one session with no
/// verdict run; the loop is free for card 2, claimed after it, which ends Done, and the loop is
/// still running (`mutant_loop_halt_on_stale` halts the loop: card 2 never runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_transition_of_the_item_in_flight_sends_it_to_needs_you_and_the_next_card_runs() {
    let scene = Scene::new().await;
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let failpoint = refuse_once(|t| {
        t.effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::RunVerdict { .. }))
    });
    let (task, watcher) = scene.start(Some(failpoint));
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(120)).await,
        State::NeedsYou(Reason::Stale)
    );
    assert_eq!(
        ended(&scene, 2, Duration::from_secs(120)).await,
        State::Done(Outcome::Pass)
    );
    let reader = scene.store.reader();
    let one = ItemId::new("demo#1");
    assert_eq!(scene.runs(1).await.len(), 1);
    assert_eq!(reader.verdict_runs(&one).await.unwrap().len(), 0);
    let stale_at = reader
        .transitions(&one)
        .await
        .unwrap()
        .iter()
        .find(|t| t.cause == "stale")
        .map(|t| t.ts_ms)
        .expect("item 1's stale arrow");
    let claimed_at = reader
        .transitions(&ItemId::new("demo#2"))
        .await
        .unwrap()
        .iter()
        .find(|t| t.cause == "claim")
        .map(|t| t.ts_ms)
        .expect("item 2's claim");
    assert!(claimed_at >= stale_at, "{claimed_at} < {stale_at}");
    assert!(!task.is_finished(), "the loop stopped");
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
}
