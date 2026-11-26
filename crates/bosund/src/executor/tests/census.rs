//! The census of a repository's tests (#7): after a passing verdict, a kept counter that
//! dropped between the item's base and the judged commit sends the item to Needs you with the
//! counts, unless the card's `allow-census` names it.

use super::*;

/// The census of the scenes: `tests/**`, assertions and tests kept, switched-off tests capped.
const CENSUS: &str = "[repo.demo.policy.census]\n\
                      glob = \"tests/**\"\n\
                      keep = { tests = \"fn test_\", assertions = \"assert\" }\n\
                      cap = { ignored = '#\\[ignore' }\n";

/// A scene with [`CENSUS`] and `tests/a.rs` committed with two `assert` lines, and card 1,
/// whose session empties `tests/a.rs` (it then holds the word `emptied`) and whose verdict
/// passes only after that. `allow` is the card's `allow-census`, if any.
async fn scene_with(allow: Option<&str>) -> Scene {
    let mut scene = Scene::new().await;
    scene.config_more = CENSUS.to_owned();
    let text = scene.config_text("'docs/frozen'");
    assert!(text.contains("[repo.demo.policy.census]"), "{text}");
    std::fs::write(&scene.config_path, text).unwrap();
    std::fs::create_dir_all(scene.repo.join("tests")).unwrap();
    std::fs::write(
        scene.repo.join("tests/a.rs"),
        "assert!(true);\nassert_eq!(1, 1);\n",
    )
    .unwrap();
    scene.git(&["add", "tests/a.rs"]);
    scene.git(&["commit", "-q", "-m", "Add the tests"]);
    let mut one = card(
        1,
        "Empties a test",
        "grep -q emptied tests/a.rs",
        "Item one.",
    );
    if let Some(allow) = allow {
        one.body = one
            .body
            .replace("verdict = ", &format!("allow-census = {allow}\nverdict = "));
        assert!(one.body.contains("allow-census"), "{}", one.body);
    }
    scene.write_board(&[one]);
    scene.script(1, &["commit tests/a.rs emptied".to_owned()]);
    scene
}

/// Card 1 deletes both assertions of `tests/a.rs`; its verdict passes. The item ends Needs you
/// (`census_drop`) after one attempt, none counted failed, and its Needs-you comment gives
/// `assertions 2 -> 0`. `mutant_loop_skip_census` lets it reach Done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_that_deletes_an_assertion_goes_to_needs_you_with_the_counts() {
    let scene = scene_with(None).await;
    let (task, watcher) = scene.start(None);
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(120)).await,
        State::NeedsYou(Reason::CensusDrop)
    );
    let id = ItemId::new("demo#1");
    let stored = scene.store.reader().item(&id).await.unwrap().unwrap();
    assert_eq!(stored.item.failed, 0, "{:?}", stored.item);
    let attempts = scene.store.reader().attempts(&id).await.unwrap();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    drained(&scene).await;
    let cards = scene.cards();
    let one = cards.iter().find(|c| c.id.0 == 1).unwrap();
    assert_eq!(one.column, Column::NeedsYou, "{one:#?}");
    let comment = one
        .comments
        .iter()
        .map(|c| c.body.as_str())
        .find(|b| b.contains("needs you"))
        .unwrap_or_else(|| panic!("no Needs-you comment: {:#?}", one.comments));
    assert!(comment.contains("assertions 2 -> 0"), "{comment}");
    assert!(comment.contains("allow-census"), "{comment}");
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
}

/// The same card with `allow-census = ["assertions"]` ends Done.
/// `mutant_loop_census_ignores_allowance` sends it to Needs you.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_that_allows_the_drop_lands() {
    let scene = scene_with(Some("[\"assertions\"]")).await;
    let (task, watcher) = scene.start(None);
    assert_eq!(
        ended(&scene, 1, Duration::from_secs(120)).await,
        State::Done(Outcome::Pass)
    );
    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
}
