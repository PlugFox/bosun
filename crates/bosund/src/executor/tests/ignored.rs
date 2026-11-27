//! The new-ignored-files guard with a build below the repository root (#218): run 6's session
//! built from a crate directory with a relative target directory, and the guard's reason listed
//! every one of thousands of files. The reason is one short line with the places folded.

use super::*;

/// A card's session writes 60 build files under `crates/demo/target/` (ignored by `target/`,
/// but not the build directory `target` at the root) and commits its work: Needs you with the
/// guard, one reason of at most 300 characters naming the place, its count and the hint, no
/// file listed, and nothing deleted. `mutant_git_ignored_unfolded` lists all 60 files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_below_the_root_is_refused_with_a_short_folded_reason() {
    let scene = Scene::new().await;
    std::fs::write(scene.repo.join(".gitignore"), "target/\n").unwrap();
    scene.git(&["add", ".gitignore"]);
    scene.git(&["commit", "-q", "-m", "Ignore build output"]);
    scene.write_board(&[card(1, "Built below", "test -f one.txt", "Item one.")]);
    let mut lines: Vec<String> = (0..60)
        .map(|i| format!("write crates/demo/target/debug/f{i}.o x"))
        .collect();
    lines.push("commit one.txt one".to_owned());
    scene.script(1, &lines);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(120)).await;
    task.abort();
    watcher.abort();
    assert_eq!(
        state,
        State::NeedsYou(Reason::Guard(bosun_core::Guard::IgnoredFile))
    );

    let id = ItemId::new("demo#1");
    let reader = scene.store.reader();
    assert_eq!(reader.verdict_runs(&id).await.unwrap().len(), 0);
    let attempts = reader.attempts(&id).await.unwrap();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    let refusal = &attempts[0].refusal;
    assert_eq!(refusal.len(), 1, "{refusal:?}");
    let reason = &refusal[0];
    assert!(reason.chars().count() <= 300, "{reason}");
    for part in [
        "crates/demo/target/ (60 files)",
        "a build ran below the repository root",
    ] {
        assert!(reason.contains(part), "{part}: {reason}");
    }
    assert!(!reason.contains("f1.o"), "{reason}");
    for i in 0..60 {
        let file = scene.repo.join(format!("crates/demo/target/debug/f{i}.o"));
        assert!(file.exists(), "nothing is deleted: {}", file.display());
    }
}
