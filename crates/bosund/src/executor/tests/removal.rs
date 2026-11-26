//! A repository removed from the configuration while its loop runs (#186): the loop is not
//! stopped (that waits for the next daemon start), but it claims nothing new, and its item in
//! flight goes to Needs you as a configuration change.

use super::*;

/// How long the item may take.
const LIMIT: Duration = Duration::from_secs(180);

/// Two Ready cards; once card 1's session runs, `[repo.demo]` leaves the configuration (it is
/// renamed `alpha`, which has no loop). Card 1 ends Needs you (`config_changed`) with its
/// session killed before it was let go on; card 2 is never claimed over many polls, and the
/// loop still runs (`mutant_loop_removed_repo_keeps_item` takes the removal for no change, so
/// card 1 runs on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_repository_claims_nothing_new_and_its_item_in_flight_needs_you() {
    let mut scene = Scene::new().await;
    let (started, go) = (scene.path("started"), scene.path("go"));
    scene.write_board(&[
        card(1, "First", "test -f done.txt", "Item 1."),
        card(2, "Second", "true", "Item 2."),
    ]);
    scene.script(
        1,
        &[
            format!("write {} x", started.display()),
            format!("wait {}", go.display()),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);

    wait_for("card 1's session to start", LIMIT, async || {
        started.exists() || terminal(scene.state(1).await)
    })
    .await;
    assert!(started.exists(), "{:?}", scene.state(1).await);

    // The configuration without `[repo.demo]`: the same repository named otherwise.
    scene.name = "alpha".to_owned();
    let without = scene.config_text("'docs/frozen'");
    scene.name = "demo".to_owned();
    std::fs::write(&scene.config_path, without).unwrap();

    // Not `ended`: the mutant keeps the item waiting for `go` forever.
    wait_for("card 1 to end", LIMIT, async || {
        terminal(scene.state(1).await)
    })
    .await;
    assert_eq!(
        scene.state(1).await,
        Some(State::NeedsYou(Reason::ConfigChanged))
    );
    assert!(!go.exists(), "the session was never let go on");

    // At least five of the loop's polls (50 ms each), with room to spare.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let claims: Vec<_> = scene
            .store
            .reader()
            .transitions(&ItemId::new("demo#2"))
            .await
            .unwrap()
            .into_iter()
            .filter(|t| t.cause == "claim")
            .collect();
        assert!(claims.is_empty(), "card 2 was claimed: {claims:?}");
        assert!(!task.is_finished(), "the loop ended");
    }
    task.abort();
    watcher.abort();
}
