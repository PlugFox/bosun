//! Worktree mode in the loop (#8, E19): with `repo.demo.worktree` an item's session, guards
//! and preconditions run in its worktree off the daemon's clone, each verdict run in a set-up
//! checkout of the worktree's head (only what was committed), and the user's
//! checkout is guarded by E4 as amended by E21: a switched HEAD is Needs you before any
//! verdict runs, the user's own commits and edits are not (edits are reported). A failing
//! `setup` is Needs you with no session started.

use std::collections::BTreeSet;

use bosun_api::EventFilter;
use bosun_store::{WorktreeKind, WorktreeState};

use super::*;
use crate::events::{Batch, Subscriber};

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(120);

/// The user's checkout as the guard of E21 sees it, and as the user would.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    head: String,
    branch: String,
    refs: String,
    status: String,
}

impl Seen {
    fn of(scene: &Scene) -> Self {
        Self {
            head: scene.git(&["rev-parse", "HEAD"]),
            branch: scene.git(&["symbolic-ref", "HEAD"]),
            refs: scene.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            status: scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        }
    }
}

/// Runs `git args` in `dir` the way [`Scene::git`] does in the scene's repository.
fn git_in(scene: &Scene, dir: &Path, args: &[&str]) -> String {
    let out = without_session(&mut Command::new("git"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &scene.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn item(card: u64) -> ItemId {
    ItemId::new(format!("demo#{card}"))
}

/// The item worktree rows of `card`.
async fn rows(scene: &Scene, card: u64) -> Vec<bosun_store::WorktreeRow> {
    scene
        .store
        .reader()
        .worktrees(&item(card))
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == WorktreeKind::Item)
        .collect()
}

/// Waits until every item worktree of `card` has ended (the loop closes it after the item's
/// last transition).
async fn closed(scene: &Scene, card: u64) {
    wait_for("the item's worktree to close", LIMIT, async || {
        let rows = rows(scene, card).await;
        !rows.is_empty() && rows.iter().all(|r| r.state != WorktreeState::Created)
    })
    .await;
}

/// The entries of the worktree root.
fn root_entries(scene: &Scene) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(scene.worktree_root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    entries
}

/// The loop's trouble (`board_sync_error`) as it is published.
fn subscribe_trouble(scene: &Scene) -> Subscriber {
    let filter = EventFilter {
        kinds: Some(vec!["board_sync_error".to_owned()]),
        tail: None,
    };
    scene.events.subscribe("test", None, None, filter).1
}

/// The trouble `sub` has received so far.
fn trouble(sub: &mut Subscriber) -> Vec<String> {
    match sub.take(1_000) {
        Batch::Events(events) => events
            .iter()
            .map(|e| {
                let v: serde_json::Value = serde_json::from_slice(&e.line).unwrap();
                v["params"]["body"]["error"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect(),
        Batch::Lagged(_) => panic!("lagged"),
    }
}

/// The values of trailer `key` on commit `rev` of the repository at `dir`.
fn trailer_in(scene: &Scene, dir: &Path, rev: &str, key: &str) -> Vec<String> {
    git_in(
        scene,
        dir,
        &[
            "log",
            "-1",
            &format!("--format=%(trailers:key={key},valueonly)"),
            rev,
        ],
    )
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect()
}

/// The item commits a file and leaves an ignored one in the build directory; its verdict
/// passes only after the commit. It ends Done, and the user's checkout (HEAD, branch, refs,
/// status, worktree list) is as it was: neither file is there. The work is in the daemon's
/// clone under the item's aside ref, its commit carrying the attempt's trailers; the worktree
/// is gone and its row is `removed` (`mutant_loop_session_in_user_checkout` runs the session in
/// the user's checkout).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_runs_in_its_worktree_and_the_users_checkout_is_untouched() {
    let scene = Scene::with_worktrees(None).await;
    std::fs::write(scene.repo.join(".gitignore"), "target/\n").unwrap();
    scene.git(&["add", ".gitignore"]);
    scene.git(&["commit", "-q", "-m", "Ignore the build directory"]);
    let before = Seen::of(&scene);
    scene.write_board(&[card(1, "In its worktree", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit one.txt one".to_owned(),
            "write target/scratch.txt x".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(Seen::of(&scene), before);
    assert!(!scene.repo.join("one.txt").exists());
    assert!(!scene.repo.join("target").exists());
    let listed = scene.git(&["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "{listed}"
    );

    let clone = scene.worktree_root().join(workspace::CLONE);
    let aside = git::aside_ref(&item(1));
    git_in(
        &scene,
        &clone,
        &["rev-parse", "--verify", "--quiet", &aside],
    );
    let files = git_in(&scene, &clone, &["ls-tree", "-r", "--name-only", &aside]);
    assert!(files.lines().any(|f| f == "one.txt"), "{files}");
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(trailer_in(&scene, &clone, &aside, "Item"), ["demo#1"]);
    assert_eq!(trailer_in(&scene, &clone, &aside, "Run"), ["a1s1"]);
    assert_eq!(
        trailer_in(&scene, &clone, &aside, "Session"),
        [runs[0].session_key.clone()]
    );

    let rows = rows(&scene, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].state, WorktreeState::Removed);
    assert!(!Path::new(&rows[0].path).exists(), "{}", rows[0].path);
    assert!(rows[0].user_base_json.is_some());
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
}

/// The session commits in its worktree and also switches the user's checkout to a new branch:
/// the item ends Needs you (`guard_checkout`) and no verdict run of the attempt is recorded
/// (`mutant_loop_skip_checkout_guard` lets it reach Done).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_switches_the_users_branch_needs_the_user() {
    let scene = Scene::with_worktrees(None).await;
    scene.write_board(&[card(1, "Escapes", "test -f two.txt", "Item two.")]);
    scene.script(
        1,
        &[
            "commit two.txt two".to_owned(),
            format!("git -C {} checkout -q -b escaped", sh_path(&scene.repo)),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert_eq!(
        state,
        State::NeedsYou(Reason::Guard(bosun_core::Guard::Checkout))
    );
    let runs = scene.store.reader().verdict_runs(&item(1)).await.unwrap();
    assert_eq!(runs.len(), 0, "{runs:?}");
}

/// While the session runs, the user's target branch gains a commit and an untracked file
/// appears in the user's checkout: the item ends Done, and the change of the status is
/// reported as a warning naming the file (`mutant_loop_strict_checkout_guard` refuses both).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_commit_and_a_dirty_file_during_the_run_do_not_stop_the_item() {
    let scene = Scene::with_worktrees(None).await;
    let mut sub = subscribe_trouble(&scene);
    scene.write_board(&[card(
        1,
        "Alongside the user",
        "test -f three.txt",
        "Item three.",
    )]);
    let user = sh_path(&scene.repo);
    scene.script(
        1,
        &[
            "commit three.txt three".to_owned(),
            // The test standing in for the user, working in their checkout meanwhile.
            format!("git -C {user} commit --allow-empty -q -m user"),
            format!("write {user}/notes.txt x"),
        ],
    );
    let head = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    // The user's commit is there, on the branch the user is on.
    assert_eq!(scene.git(&["rev-parse", "HEAD~1"]), head);
    assert_eq!(scene.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
    let trouble = trouble(&mut sub);
    assert!(
        trouble.iter().any(|t| t.contains("notes.txt")),
        "{trouble:#?}"
    );
}

/// With `setup = "exit 3"` the item ends Needs you (`setup`), no session is started and no
/// worktree directory is left (`mutant_loop_setup_failure_is_claim_failure` sends the item back
/// to Ready: it never reaches Needs you).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_setup_needs_the_user_and_starts_no_session() {
    let scene = Scene::with_worktrees(Some("exit 3")).await;
    scene.write_board(&[card(1, "Set up", "test -f four.txt", "Item four.")]);
    scene.script(1, &["commit four.txt four".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert_eq!(state, State::NeedsYou(Reason::Setup));
    assert_eq!(scene.runs(1).await.len(), 0);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
}

/// The verdict rows of `card`: the throwaway checkouts its verdict runs ran in.
async fn verdict_rows(scene: &Scene, card: u64) -> Vec<bosun_store::WorktreeRow> {
    scene
        .store
        .reader()
        .worktrees(&item(card))
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == WorktreeKind::Verdict)
        .collect()
}

/// The session commits `a.txt`, changes it again without committing and leaves an untracked
/// `b.txt`: the verdict, run in a checkout of the commit, sees the committed `a.txt` and no
/// `b.txt`. The item ends Done in one attempt, and its `uncommitted` note names both files
/// (`mutant_loop_verdict_in_item_worktree` runs the verdict among them: it fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verdict_sees_only_what_was_committed() {
    let scene = Scene::with_worktrees(None).await;
    scene.write_board(&[card(
        1,
        "Committed only",
        "test -f a.txt && test ! -f b.txt && grep -q committed-text a.txt",
        "Item one.",
    )]);
    scene.script(
        1,
        &[
            "commit a.txt committed-text".to_owned(),
            "write a.txt changed-later".to_owned(),
            "write b.txt untracked".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let attempts: BTreeSet<u32> = scene
        .store
        .reader()
        .verdict_runs(&item(1))
        .await
        .unwrap()
        .iter()
        .map(|r| r.attempt)
        .collect();
    assert_eq!(attempts, BTreeSet::from([1]));
    let note = scene
        .store
        .reader()
        .item_note(&item(1), outbox::NOTE_UNCOMMITTED)
        .await
        .unwrap()
        .expect("an uncommitted note");
    let paths: Vec<String> = serde_json::from_str(&note).unwrap();
    assert!(paths.iter().any(|p| p == "a.txt"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "b.txt"), "{paths:?}");
}

/// With `setup = "echo ready > setup-ran"` the verdict checkout is set up too: a verdict that
/// needs `setup-ran` and the committed `a.txt` passes, and the item ends Done
/// (`mutant_loop_verdict_checkout_without_setup` skips the setup there: it fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_setup_command_runs_in_the_verdict_checkout() {
    let scene = Scene::with_worktrees(Some("echo ready > setup-ran")).await;
    scene.write_board(&[card(
        1,
        "Set up for the verdict",
        "test -f setup-ran && test -f a.txt",
        "Item one.",
    )]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let rows = verdict_rows(&scene, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
}

/// A verdict that fails its first run and passes when re-run: each run had its own checkout,
/// and both are gone once the item is Done. Two rows of kind `verdict`, both `removed`; the
/// worktree root holds only the clone, and the clone no `bosun/*-v*` branch
/// (`mutant_loop_keep_verdict_checkout` leaves them behind).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verdict_checkout_is_removed_after_each_run() {
    let scene = Scene::with_worktrees(None).await;
    // Outside the repository: the run on the base commit is the first, the attempt's first run
    // the second (it fails), its re-run the third.
    let count = sh_path(&scene.path("count1"));
    scene.write_board(&[card(
        1,
        "Fails once",
        &format!(
            "n=$(cat {count} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count}; \
             if [ $n -eq 2 ]; then exit 1; fi; [ $n -ge 2 ] && test -f a.txt"
        ),
        "Item one.",
    )]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let verdicts = scene.store.reader().verdict_runs(&item(1)).await.unwrap();
    assert_eq!(verdicts.len(), 2, "{verdicts:?}");
    let rows = verdict_rows(&scene, 1).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(row.state, WorktreeState::Removed, "{row:?}");
        assert!(!Path::new(&row.path).exists(), "{}", row.path);
    }
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
    let clone = scene.worktree_root().join(workspace::CLONE);
    assert_eq!(
        git_in(&scene, &clone, &["branch", "--list", "bosun/*-v*"]),
        ""
    );
}
