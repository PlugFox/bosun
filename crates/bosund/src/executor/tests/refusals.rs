//! A claim refusal in the log (#186): the loop refuses the claim at every poll while its cause
//! lasts, and logs the refusal (one `board_sync_error` each) when it starts and when its text
//! changes, not at every poll.

use super::worktrees::{subscribe_trouble, trouble};
use super::*;
use crate::events::Subscriber;

/// The refusal lines among `lines`.
fn refusals(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|l| l.contains("refused before spawn (#2)"))
        .collect()
}

/// Waits until `seen` (fed from `sub`) holds `count` refusals, then 1.5 s more (about thirty
/// polls at the scene's 50 ms) for any repeat to show.
async fn refused(sub: &mut Subscriber, seen: &mut Vec<String>, count: usize) {
    wait_for(
        &format!("{count} refusals"),
        Duration::from_secs(30),
        async || {
            seen.extend(trouble(sub));
            refusals(seen).len() >= count
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    seen.extend(trouble(sub));
}

/// #186: the identity check refuses for another login, then for an unreadable remote, over
/// about thirty polls each with the check run again every 100 ms: one line for each text.
/// Fixed, the card is claimed and ends Done. `mutant_loop_claim_refusal_every_poll` logs one
/// line per poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_claim_refusal_is_logged_once_until_its_text_changes() {
    let scene = Scene::new().await;
    std::fs::write(scene.path("login"), "someone-else").unwrap();
    scene.write_board(&[card(1, "Refused first", "test -f one.txt", "Item one.")]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let mut sub = subscribe_trouble(&scene);

    let (tx, rx) = watch::channel(Arc::new(scene.config()));
    let probe: Arc<dyn DiskProbe + Send + Sync> = Arc::new(SystemProbe::new());
    let watcher = config_watch::spawn(
        scene.config_path.clone(),
        tx,
        probe,
        Duration::from_millis(50),
        config_watch::Reported::default(),
    );
    let (mut options, deps) = scene.options_and_deps(None, scene.queue());
    options.recheck = Duration::from_millis(100);
    let task = tokio::spawn(Loop::new(options, deps, rx).run());

    let mut seen = Vec::new();
    refused(&mut sub, &mut seen, 1).await;
    let first = refusals(&seen);
    assert_eq!(first.len(), 1, "{seen:#?}");
    assert!(first[0].contains("another login"), "{first:?}");

    scene.git(&["remote", "remove", "origin"]);
    refused(&mut sub, &mut seen, 2).await;
    let both = refusals(&seen);
    assert_eq!(both.len(), 2, "{seen:#?}");
    assert!(both[1].contains("remote URL could not be read"), "{both:?}");

    scene.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/synthetic-owner/demo.git",
    ]);
    std::fs::write(scene.path("login"), LOGIN).unwrap();
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    seen.extend(trouble(&mut sub));
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    assert_eq!(refusals(&seen).len(), 2, "{seen:#?}");
}
