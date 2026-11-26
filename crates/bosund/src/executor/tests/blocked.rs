//! The cards that wait on an open blocker (#5, #7), on the loop of the scenario suite: the
//! loop publishes them after each plan, gives each a store row, and the blocker's Needs-you
//! comment names them.

use super::*;

/// Card `n` as the board holds it.
fn card_now(scene: &Scene, n: u64) -> Card {
    scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == n)
        .expect("the card is on the board")
}

/// The waiting cards the loop published for the scene's repository, as `(item, [blocker])`.
fn waiting(scene: &Scene) -> Vec<(String, Vec<String>)> {
    scene
        .blockers
        .of("demo")
        .into_iter()
        .map(|b| {
            (
                b.item.to_string(),
                b.open.iter().map(ToString::to_string).collect(),
            )
        })
        .collect()
}

/// #5: card 2 is blocked by card 1, whose verdict never passes. Card 1 ends Needs you; card 2
/// waits in Ready, listed with its blocker, with a store row and no run, and card 1's Needs-you
/// comment names it (`mutant_loop_blockers_unpublished`: nothing is listed;
/// `mutant_outbox_comment_without_dependants`: the comment does not name it). The user moves
/// card 1 to Done by hand: card 2 is claimed, ends Done, and the list no longer names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_waiting_on_a_needs_you_blocker_is_listed_and_named_in_its_comment() {
    let scene = Scene::new().await;
    let mut two = card(2, "Two", "test -f two.txt", "Item two.");
    // The local file keeps no backend blockers: the contract's `blocked-by` names card 1.
    two.body = two.body.replace(
        "verdict = ",
        "blocked-by = [1]
verdict = ",
    );
    scene.write_board(&[card(1, "One", "false", "Item one."), two]);
    // Committed once: a later session of card 1 has nothing to commit and ends clean.
    let marker = sh_path(&scene.path("session-1"));
    scene.script(1, &[format!("once {marker} commit one.txt one")]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);

    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    assert_eq!(state, State::NeedsYou(Reason::AttemptsExhausted));
    drained(&scene).await;
    assert_eq!(
        waiting(&scene),
        [("demo#2".to_owned(), vec!["demo#1".to_owned()])]
    );
    assert_eq!(
        scene.state(2).await,
        Some(State::Ready),
        "a waiting card has its row"
    );
    assert!(scene.runs(2).await.is_empty(), "a waiting card is not run");
    let comments: Vec<String> = card_now(&scene, 1)
        .comments
        .into_iter()
        .map(|c| c.body)
        .filter(|b| b.contains("needs you."))
        .collect();
    assert!(
        comments
            .iter()
            .any(|b| b.contains("Waiting on this item: demo#2. They are claimed once it is Done.")),
        "{comments:?}"
    );

    let mut cards = scene.cards();
    cards.iter_mut().find(|c| c.id.0 == 1).unwrap().column = Column::Done;
    scene.write_board(&cards);
    let state = ended(&scene, 2, Duration::from_secs(120)).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    assert!(
        waiting(&scene).iter().all(|(item, _)| item != "demo#2"),
        "{:?}",
        waiting(&scene)
    );
}
