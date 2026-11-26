//! A repository whose checkout moved (#132): the configuration's `path` changes, its name stays,
//! and the store keeps one repository of that name, so the loop still finds its items.

use super::*;

/// The first loop bounces card 1 at claim (its verdict passes on the base: Needs you, lint)
/// and lands card 2. The checkout then moves and the configuration follows it; the stored
/// repository still has the old path when the second loop starts (nothing registered the move
/// yet). Card 1, back in Ready with a verdict that needs work, is claimed with its history
/// (no stale transition), and card 3, blocked by the Done card 2, runs: both end Done, and the
/// one repository row (named `demo`) has the moved path (`mutant_store_repo_by_path` matches
/// items by path: the second loop sees none of them and its claim of card 1 is refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn a_moved_repository_keeps_its_items_and_their_history() {
    let mut scene = Scene::new().await;
    scene.write_board(&[
        card(1, "Passes on base", "true", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(120)).await,
        State::NeedsYou(Reason::Lint)
    );
    assert_eq!(
        ended(&scene, 2, Duration::from_secs(120)).await,
        State::Done(Outcome::Pass)
    );
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
    let id = |n: u64| ItemId::new(format!("demo#{n}"));
    let reader = scene.store.reader();
    let before = reader.transitions(&id(1)).await.unwrap();
    assert_eq!(
        before.last().map(|t| t.cause.as_str()),
        Some("lint_failed"),
        "{before:#?}"
    );

    // The move: every process of the first loop is gone, or Windows refuses the rename.
    let moved = scene.path("moved");
    let deadline = Instant::now() + scaled(Duration::from_secs(30));
    while let Err(e) = std::fs::rename(&scene.repo, &moved) {
        assert!(
            Instant::now() < deadline,
            "the checkout cannot be moved: {e}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    scene.repo = moved;
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    let key = scene.config().repos["demo"].key.clone();
    let stale = reader.repos().await.unwrap();
    assert!(
        stale.iter().all(|r| r.path != key),
        "the store already has the moved path: {stale:?}"
    );

    let mut cards = scene.cards();
    let one = cards.iter_mut().find(|c| c.id.0 == 1).unwrap();
    *one = card(1, "Needs work now", "test -f one.txt", "Item one, edited.");
    let mut three = card(3, "After two", "test -f three.txt", "Item three.");
    three.body = "Item three.\n\n```bosun\nverdict = \"test -f three.txt\"\n\
                  blocked-by = [\"#2\"]\n```\n"
        .to_owned();
    cards.push(three);
    scene.write_board(&cards);
    scene.script(3, &["commit three.txt three".to_owned()]);
    let (task, watcher) = scene.start(None);
    // Item 1 is terminal already (its bounce): it ends again past its stored transitions.
    let seq = before.last().unwrap().seq;
    let mut one = None;
    wait_for(
        "item 1 to end again",
        Duration::from_secs(120),
        async || {
            let item = scene.item(1).await;
            one = item.as_ref().map(|i| i.state);
            item.is_some_and(|i| i.seq > seq) && terminal(one)
        },
    )
    .await;
    let one = one.unwrap();
    let three = ended(&scene, 3, Duration::from_secs(120)).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
    assert_eq!(one, State::Done(Outcome::Pass), "item 1");
    assert_eq!(three, State::Done(Outcome::Pass), "item 3");

    // Item 1 was claimed with its history: its transitions start with the stored bounce.
    let after = reader.transitions(&id(1)).await.unwrap();
    assert!(after.len() > before.len(), "{after:#?}");
    assert_eq!(after[..before.len()], before[..], "{after:#?}");
    assert!(
        after.iter().any(|t| t.cause == "claim" && t.seq > seq),
        "{after:#?}"
    );
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.attempts, 1, "{item:?}");

    // One repository, named `demo`, at the moved path; every item is on it.
    assert_eq!(
        reader.repos().await.unwrap(),
        [bosun_store::RepoRow {
            name: Some("demo".to_owned()),
            path: key.clone(),
        }]
    );
    let items = reader.items(false).await.unwrap();
    assert_eq!(items.len(), 3, "{items:#?}");
    for stored in &items {
        assert_eq!(stored.repo, key, "{}", stored.item.id);
    }
    for file in ["one.txt", "two.txt", "three.txt"] {
        assert!(scene.repo.join(file).exists(), "{file}");
    }
}
