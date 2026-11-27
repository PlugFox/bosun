//! Git LFS in worktree mode (#8 "Git LFS objects follow a landed commit"; E5, E14, E2): the
//! daemon's clone cleans and smudges through Git LFS's filter, an item's worktree and its
//! verdict checkouts get the objects of the commit they check out from the user's repository,
//! and a landed commit's objects reach the user's repository before its checkout is
//! fast-forwarded.
//!
//! Each scene first runs `git lfs version` and is skipped (it prints `skipped: git-lfs is not
//! installed`) without it. The tests run with an empty global configuration, so each scene
//! sets Git LFS up in the user's repository itself: the filter in its own config, `*.bin`
//! routed through it by `.gitattributes`, and `lfs.url` at an empty local directory, so a
//! smudge that misses an object fails at once instead of asking the synthetic GitHub origin.

use super::landing::{pr_scene, pushed};
use super::worktrees::{closed, git_in, item};
use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(180);

/// Whether `git lfs version` succeeds; when it does not, the scene says so.
fn git_lfs() -> bool {
    let ok = without_session(&mut Command::new("git"))
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        println!("skipped: git-lfs is not installed");
    }
    ok
}

/// A `file://` URL of `path`.
fn file_url(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Sets Git LFS up in the scene's repository (see the module documentation) and commits its
/// `.gitattributes`.
fn lfs_repo(scene: &Scene) {
    for (key, value) in [
        ("filter.lfs.clean", "git-lfs clean -- %f"),
        ("filter.lfs.smudge", "git-lfs smudge -- %f"),
        ("filter.lfs.process", "git-lfs filter-process"),
        ("filter.lfs.required", "true"),
    ] {
        scene.git(&["config", key, value]);
    }
    let empty = scene.path("lfs-remote");
    std::fs::create_dir_all(&empty).unwrap();
    scene.git(&["config", "lfs.url", &file_url(&empty)]);
    std::fs::write(
        scene.repo.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    scene.git(&["add", ".gitattributes"]);
    scene.git(&["commit", "-q", "-m", "Route bin files through Git LFS"]);
}

/// The oid of the Git LFS pointer `text`.
fn oid_of(text: &str) -> String {
    assert!(
        text.starts_with("version https://git-lfs.github.com/spec/v1\n"),
        "not a pointer: {text:?}"
    );
    text.lines()
        .find_map(|l| l.strip_prefix("oid sha256:"))
        .unwrap_or_else(|| panic!("no oid: {text:?}"))
        .to_owned()
}

/// Worktree mode, `local` landing: the session commits `asset.bin` (`item-content`), which the
/// agent's git cleans through the clone's pinned filter; the verdict reads its content in the
/// verdict checkout. The item ends Done, the user's checkout has the content, its commit has a
/// pointer, and the object is in the user's store (`mutant_loop_lfs_not_carried_to_user`: the
/// fast-forward's smudge fails and the landing is refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_lfs_object_committed_in_a_worktree_reaches_the_users_checkout() {
    if !git_lfs() {
        return;
    }
    let scene = Scene::with_worktrees(None).await;
    lfs_repo(&scene);
    scene.write_board(&[card(
        1,
        "An LFS file",
        "grep -q item-content asset.bin",
        "Item one.",
    )]);
    scene.script(1, &["commit asset.bin item-content".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    let note = scene
        .store
        .reader()
        .item_note(&item(1), crate::executor::landing::NOTE_LANDING)
        .await
        .ok();
    assert!(matches!(state, State::Done(_)), "{state:?} {note:?}");
    let content = std::fs::read_to_string(scene.repo.join("asset.bin")).unwrap();
    assert!(content.contains("item-content"), "{content:?}");
    let pointer = scene.git(&["cat-file", "-p", "HEAD:asset.bin"]);
    let oid = oid_of(&format!("{pointer}\n"));
    let object = scene
        .repo
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    assert!(object.is_file(), "{} is missing", object.display());
}

/// The user's repository commits `base.bin` (`base-content`) through Git LFS before the
/// daemon starts: its object is in the user's store only. The repository's `setup` reads its
/// content (in the item's worktree and in every verdict checkout), the session commits
/// `one.txt`, and the verdict reads both. The item ends Done
/// (`mutant_loop_clone_without_lfs_filter`: `base.bin` is a pointer in the worktree and the
/// setup fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_and_its_verdict_checkout_have_the_lfs_content_of_the_base() {
    if !git_lfs() {
        return;
    }
    let scene = Scene::with_worktrees(Some("grep -q base-content base.bin")).await;
    lfs_repo(&scene);
    std::fs::write(scene.repo.join("base.bin"), "base-content\n").unwrap();
    scene.git(&["add", "base.bin"]);
    scene.git(&["commit", "-q", "-m", "Add base.bin"]);
    let pointer = scene.git(&["cat-file", "-p", "HEAD:base.bin"]);
    oid_of(&format!("{pointer}\n"));
    scene.write_board(&[card(
        1,
        "LFS content of the base",
        "grep -q base-content base.bin && test -f one.txt",
        "Item one.",
    )]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert!(scene.repo.join("one.txt").is_file());
}

/// #213, `landing = { mode = "pr" }`: the user's repository's `lfs.url` is the bare
/// `remote.git` the push goes to (git-lfs's standalone file transfer then writes the objects
/// into `remote.git/lfs/objects`), and the user's `main` with its `.gitattributes` is pushed
/// there again as the forge's target. The session commits `asset.bin` (`pr-content`). The item
/// ends Done with one open pull request, `remote.git` has `bosun/demo-1`, its `asset.bin` is a
/// pointer, and the pointer's object is in `remote.git`'s Git LFS store
/// (`mutant_loop_lfs_not_pushed`: it is not).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_request_push_carries_the_lfs_objects() {
    if !git_lfs() {
        return;
    }
    let (scene, fake, remote) = pr_scene("").await;
    lfs_repo(&scene);
    scene.git(&["config", "lfs.url", &file_url(&remote)]);
    scene.git(&[
        "push",
        "-q",
        &remote.to_string_lossy(),
        "main:refs/heads/main",
    ]);
    scene.write_board(&[card(
        1,
        "An LFS file in a pull request",
        "grep -q pr-content asset.bin",
        "Item one.",
    )]);
    scene.script(1, &["commit asset.bin pr-content".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    let note = scene
        .store
        .reader()
        .item_note(&item(1), crate::executor::landing::NOTE_LANDING)
        .await
        .ok();
    assert!(matches!(state, State::Done(_)), "{state:?} {note:?}");
    assert_eq!(fake.prs().len(), 1, "{:#?}", fake.prs());
    let branches = pushed(&scene, &remote);
    assert!(
        branches
            .iter()
            .any(|b| b.starts_with("refs/heads/bosun/demo-1 ")),
        "{branches:?}"
    );
    let pointer = git_in(
        &scene,
        &remote,
        &["cat-file", "-p", "bosun/demo-1:asset.bin"],
    );
    let oid = oid_of(&format!("{pointer}\n"));
    let object = remote
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    let content = std::fs::read_to_string(&object)
        .unwrap_or_else(|e| panic!("{} is missing: {e}", object.display()));
    assert!(content.contains("pr-content"), "{content:?}");
}
