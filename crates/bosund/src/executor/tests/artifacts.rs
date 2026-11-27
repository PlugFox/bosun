//! A device's state is not the item's until the item put it there (#44 C), on the loop: a
//! resource declares an artifact (a local file the verdict builds, its path on the device, a
//! `digest` command), and after each verdict run that leases it the daemon asks the device for
//! its copy's digest. The device here is a directory of the scene and the commands are shell
//! one-liners; no real device or device tool anywhere.

use sha2::{Digest, Sha256};

use super::leases::card_needing;
use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(180);

/// A worktree scene with the fake device `device` of the scene, holding `app` with `old`, and
/// `[resource.rig]` declaring the artifact `app` (`out/app`, the device's `app`, `sha256sum` as
/// the digest), and card 1 that needs `rig`, whose session commits `app.txt` with `new`, with
/// `verdict` (`{device}` stands for the device's directory). Returns the scene and the device.
async fn scene(verdict: &str) -> (Scene, PathBuf) {
    let mut scene = Scene::with_worktrees(None).await;
    let device = scene.path("device");
    std::fs::create_dir_all(&device).unwrap();
    std::fs::write(device.join("app"), "old\n").unwrap();
    let on_device = sh_path(&device);
    scene.config_more = format!(
        "[resource.rig]\n\
         kind = \"device\"\n\
         [resource.rig.artifact.app]\n\
         local = \"out/app\"\n\
         device = '{on_device}/app'\n\
         digest = 'sha256sum \"$BOSUN_ARTIFACT_DEVICE\" 2>/dev/null | cut -d\" \" -f1'\n"
    );
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    let verdict = verdict.replace("{device}", &on_device);
    scene.write_board(&[card_needing(1, &verdict, &["rig"])]);
    scene.script(1, &["commit app.txt new".to_owned()]);
    (scene, device)
}

/// The SHA-256 of `bytes`, its first 12 hex digits.
fn prefix(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..6]
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The verdict runs of item 1: their results.
async fn verdicts(scene: &Scene) -> Vec<Option<bosun_core::RunResult>> {
    scene
        .store
        .reader()
        .verdict_runs(&ItemId::new("demo#1"))
        .await
        .unwrap()
        .iter()
        .map(|v| v.result)
        .collect()
}

/// Item 1's `unready` note, if any.
async fn unready_note(scene: &Scene) -> Option<String> {
    scene
        .store
        .reader()
        .item_note(&ItemId::new("demo#1"), outbox::NOTE_UNREADY)
        .await
        .unwrap()
}

/// The comments on card 1 that say its item needs the user.
fn needs_you_comments(scene: &Scene) -> Vec<String> {
    scene
        .cards()
        .into_iter()
        .find(|c| c.id.0 == 1)
        .expect("the card is on the board")
        .comments
        .into_iter()
        .filter(|c| c.key.is_some() && c.body.contains("needs you."))
        .map(|c| c.body)
        .collect()
}

/// #44 C's case: the verdict builds `out/app` and "runs on the device" without pushing, so it
/// passes against the copy another build left there. The pass does not stand: Needs you
/// (`resource_unready`) at once, after one session and one verdict run (recorded `unready`),
/// with no failed attempt; the device keeps `old` (the daemon never pushes); the `unready` note
/// names `rig`, `app` and the digests' prefixes of `new` (the run's build) and `old` (the
/// device's), and the one Needs-you comment quotes it with no path in it
/// (`mutant_loop_skip_artifact_check`: Done).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_against_another_build_on_the_device_is_needs_you_and_counts_nothing() {
    use bosun_core::RunResult;
    let (scene, device) =
        scene("mkdir -p out && cp app.txt out/app && test -f '{device}/app'").await;
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::ResourceUnready));
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "{item:?}");
    assert_eq!(scene.runs(1).await.len(), 1, "one session");
    assert_eq!(verdicts(&scene).await, [Some(RunResult::Unready)]);
    assert_eq!(std::fs::read(device.join("app")).unwrap(), b"old\n");

    let note = unready_note(&scene).await.expect("the unready note");
    let (new, old) = (prefix(b"new\n"), prefix(b"old\n"));
    for part in ["`rig`", "`app`", "the device's copy differs", &new, &old] {
        assert!(note.contains(part), "{part:?} is not in {note:?}");
    }
    let comments = needs_you_comments(&scene);
    assert_eq!(comments.len(), 1, "{comments:#?}");
    let comment = &comments[0];
    assert!(
        comment.contains("A resource the verdict needs was not ready"),
        "{comment}"
    );
    for part in ["rig", &new, &old] {
        assert!(comment.contains(part), "{part:?} is not in {comment}");
    }
    let dir = scene.path("");
    for path in [
        "out/app".to_owned(),
        sh_path(&dir).trim_end_matches('/').to_owned(),
        dir.display()
            .to_string()
            .trim_end_matches(['/', '\\'])
            .to_owned(),
    ] {
        assert!(!comment.contains(&path), "{path:?} is in {comment}");
    }
}

/// The verdict pushes its own build to the device and runs against it: the run stands, Done
/// (pass) after one session, the device's `app` is the committed `app.txt`, and no `unready`
/// note (`mutant_loop_artifact_local_in_item_worktree` looks for `out/app` in the item's
/// worktree, where the verdict did not build it: Needs you).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verdict_that_puts_its_own_build_on_the_device_stands() {
    use bosun_core::RunResult;
    let (scene, device) = scene(
        "mkdir -p out && cp app.txt out/app && cp out/app '{device}/app' && grep -qx new \
         '{device}/app'",
    )
    .await;
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    drained(&scene).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::Done(Outcome::Pass));
    let item = scene.item(1).await.unwrap();
    assert_eq!(item.failed, 0, "{item:?}");
    assert_eq!(scene.runs(1).await.len(), 1, "one session");
    assert_eq!(verdicts(&scene).await, [Some(RunResult::Pass)]);
    // What the session committed (`commit` writes the text and a newline).
    assert_eq!(std::fs::read(device.join("app")).unwrap(), b"new\n");
    assert_eq!(unready_note(&scene).await, None);
}
