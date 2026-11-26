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
pub(super) fn git_in(scene: &Scene, dir: &Path, args: &[&str]) -> String {
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

pub(super) fn item(card: u64) -> ItemId {
    ItemId::new(format!("demo#{card}"))
}

/// The item worktree rows of `card`.
pub(super) async fn rows(scene: &Scene, card: u64) -> Vec<bosun_store::WorktreeRow> {
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
pub(super) async fn closed(scene: &Scene, card: u64) {
    wait_for("the item's worktree to close", LIMIT, async || {
        let rows = rows(scene, card).await;
        !rows.is_empty() && rows.iter().all(|r| r.state != WorktreeState::Created)
    })
    .await;
}

/// The entries of the worktree root.
pub(super) fn root_entries(scene: &Scene) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(scene.worktree_root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    entries
}

/// The loop's trouble (`board_sync_error`) as it is published.
pub(super) fn subscribe_trouble(scene: &Scene) -> Subscriber {
    let filter = EventFilter {
        kinds: Some(vec!["board_sync_error".to_owned()]),
        tail: None,
    };
    scene.events.subscribe("test", None, None, filter).1
}

/// The trouble `sub` has received so far.
pub(super) fn trouble(sub: &mut Subscriber) -> Vec<String> {
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
pub(super) fn trailer_in(scene: &Scene, dir: &Path, rev: &str, key: &str) -> Vec<String> {
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
/// passes only after the commit. It ends Done, and the session never touched the user's
/// checkout: it is on its branch, clean, with one worktree and no other ref, and what reached
/// it is only the landed commit (the committed file, carrying the attempt's trailers, on top of
/// the head at claim), not the ignored file. A Done item's work is on the target, so no aside
/// ref is kept; the worktree is gone and its row is `removed`
/// (`mutant_loop_session_in_user_checkout` runs the session in the user's checkout).
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
    let after = Seen::of(&scene);
    assert_eq!(after.branch, before.branch);
    assert_eq!(after.status, before.status);
    assert_eq!(
        after.refs,
        format!("refs/heads/main {}", after.head),
        "{after:?}"
    );
    assert_eq!(scene.git(&["rev-parse", "HEAD~1"]), before.head);
    assert!(scene.repo.join("one.txt").is_file());
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

    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(trailer_in(&scene, &scene.repo, "HEAD", "Item"), ["demo#1"]);
    assert_eq!(trailer_in(&scene, &scene.repo, "HEAD", "Run"), ["a1s1"]);
    assert_eq!(
        trailer_in(&scene, &scene.repo, "HEAD", "Session"),
        [runs[0].session_key.clone()]
    );
    let clone = scene.worktree_root().join(workspace::CLONE);
    let aside = git::aside_ref(&item(1));
    assert_eq!(
        git_in(
            &scene,
            &clone,
            &["for-each-ref", "--format=%(refname)", &aside]
        ),
        ""
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
/// appears in the user's checkout: the change of the status is reported as a warning naming
/// the file, and the item goes on to its landing, which the untracked file defers until the
/// test removes it. The item ends Done on top of the user's commit
/// (`mutant_loop_strict_checkout_guard` refuses both).
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
    // The user's untracked file holds the landing (#8, E2) until it goes.
    let mut seen = Vec::new();
    wait_for("the landing to be deferred", LIMIT, async || {
        seen.extend(trouble(&mut sub));
        seen.iter().any(|t| t.contains("is deferred")) || terminal(scene.state(1).await)
    })
    .await;
    std::fs::remove_file(scene.repo.join("notes.txt")).unwrap();
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    // The user's commit is there, on the branch the user is on, below the item's.
    assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD~1"]), "user");
    assert_eq!(scene.git(&["rev-parse", "HEAD~2"]), head);
    assert_eq!(scene.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert!(seen.iter().any(|t| t.contains("notes.txt")), "{seen:#?}");
}

/// The session commits `a.txt` and leaves an untracked `left.txt`; the verdict never passes.
/// The item ends Needs you, and its work is kept in the daemon's clone under its aside ref: a
/// commit holding `left.txt` whose author and committer are the repository identity's (E26),
/// not a fixed one (`mutant_loop_aside_commit_fixed_identity` makes it as
/// `bosun <bosun@localhost>`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_set_aside_work_carries_the_repository_identity() {
    let scene = Scene::with_worktrees(None).await;
    scene.write_board(&[card(1, "Set aside", "test -f never", "Item one.")]);
    let marker = sh_path(&scene.path("session-1"));
    scene.script(
        1,
        &[
            format!("once {marker} commit a.txt a"),
            "write left.txt left".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::NeedsYou(_)), "{state:?}");
    let clone = scene.worktree_root().join(workspace::CLONE);
    let aside = git::aside_ref(&item(1));
    let files = git_in(&scene, &clone, &["ls-tree", "-r", "--name-only", &aside]);
    assert!(files.lines().any(|f| f == "left.txt"), "{files}");
    assert!(files.lines().any(|f| f == "a.txt"), "{files}");
    for format in ["%an <%ae>", "%cn <%ce>"] {
        assert_eq!(
            git_in(
                &scene,
                &clone,
                &["log", "-1", &format!("--format={format}"), &aside]
            ),
            "Synthetic Agent <agent@example.com>",
            "{format}"
        );
    }
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
pub(super) async fn verdict_rows(scene: &Scene, card: u64) -> Vec<bosun_store::WorktreeRow> {
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
/// needs `setup-ran` and the committed `a.txt` passes, and the item ends Done, with two verdict
/// checkouts: the base commit's (D7) and the attempt's
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
    assert_eq!(rows.len(), 2, "{rows:?}");
}

/// A verdict that fails its first run and passes when re-run: each run had its own checkout,
/// and both are gone once the item is Done, as is the checkout of the verdict on the base
/// commit. Three rows of kind `verdict`, all `removed`; the
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
    assert_eq!(rows.len(), 3, "{rows:?}");
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

/// With `setup = "echo ready > setup-ran"` the session stages and commits `setup-ran`, which
/// only `setup` made; the verdict asks git whether it is tracked. The verdict on the base
/// commit ran in a checkout of its own, so the item's worktree still has the file for the first
/// session: the item ends Done after that one session, and the worktree root holds only the
/// clone
/// (`mutant_loop_base_verdict_in_item_worktree` runs it in the item's worktree and restores that
/// after it: the file is gone and the item never passes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_setup_output_is_there_for_the_first_session() {
    let scene = Scene::with_worktrees(Some("echo ready > setup-ran")).await;
    scene.write_board(&[card(
        1,
        "Takes what setup made",
        "git ls-files --error-unmatch setup-ran",
        "Item one.",
    )]);
    scene.script(
        1,
        &[
            "git add setup-ran".to_owned(),
            "git commit -q -m took-setup".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    // The first session took it: no crash, no reset (whose setup would make it again).
    let runs = scene.runs(1).await;
    assert_eq!(modes(&runs), [(1, SpawnMode::Fresh)]);
    assert_eq!(state, State::Done(Outcome::Pass));
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
}

/// The same setup and verdict; the first session crashes, its resume crashes before it comes
/// up, so the attempt's next spawn is a fresh session after a reset of the worktree to its
/// head, which takes `setup-ran` with it. Only that session stages and commits the file: the
/// setup ran again after the reset, so the item ends Done, and one spawn was a `reset`
/// (`mutant_loop_reset_without_setup` starts the session without it: it never passes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_session_after_a_reset_finds_the_setup_output() {
    let scene = Scene::with_worktrees(Some("echo ready > setup-ran")).await;
    let m = |name: &str| sh_path(&scene.path(name));
    scene.write_board(&[card(
        1,
        "Takes what setup made after a reset",
        "git ls-files --error-unmatch setup-ran",
        "Item one.",
    )]);
    scene.script(
        1,
        &[
            // The first session crashes; its resume crashes before it comes up.
            format!("once {} exit 1", m("crashed")),
            format!("once {} exit 1", m("resume-failed")),
            "git add setup-ran".to_owned(),
            "git commit -q -m took-setup".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    let runs = scene.runs(1).await;
    assert!(
        modes(&runs).contains(&(1, SpawnMode::Reset)),
        "{:?}",
        modes(&runs)
    );
    assert_eq!(state, State::Done(Outcome::Pass), "{:?}", modes(&runs));
}

/// The setup marks its start outside the repository and sleeps for two minutes. Once it runs,
/// the card goes to Backlog: the item is Cancelled and its worktree closed within 20 seconds,
/// and no worktree directory is left (`mutant_loop_setup_ignores_kill` lets the setup run to
/// the verdict timeout first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_during_setup_ends_it_at_once() {
    let mut scene = Scene::with_worktrees(None).await;
    let started = scene.path("setup-started");
    scene.setup = Some(format!("echo x > {}; sleep 120", sh_path(&started)));
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    scene.write_board(&[card(1, "Cancelled in setup", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_file(&started).await;

    let mut cards = scene.cards();
    cards[0].column = Column::Backlog;
    scene.write_board(&cards);
    let mut state = None;
    wait_for(
        "the cancelled item's worktree to close",
        Duration::from_secs(20),
        async || {
            state = scene.state(1).await;
            let rows = rows(&scene, 1).await;
            state == Some(State::Cancelled)
                && !rows.is_empty()
                && rows.iter().all(|r| r.state != WorktreeState::Created)
        },
    )
    .await;
    task.abort();
    watcher.abort();

    assert_eq!(state, Some(State::Cancelled));
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
}

/// With a disk floor of 1 GB and plenty of space at claim, the session marks its start and
/// waits; the test takes the free space to nothing and lets it commit. The verdict's checkout
/// cannot be made: the item stays `verifying` for many polls, with one trouble line naming the
/// floor and no Needs you; once the space is back the item ends Done
/// (`mutant_loop_floor_is_setup_failure` sends it to Needs you as a setup failure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verdict_below_the_disk_floor_waits_instead_of_needing_the_user() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.worktree_more = ", disk_floor = '1GB'".to_owned();
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    let mut sub = subscribe_trouble(&scene);
    let (session, go) = (scene.path("session-started"), scene.path("go"));
    scene.write_board(&[card(1, "Waits for space", "test -f a.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", sh_path(&session)),
            format!("wait {}", sh_path(&go)),
            "commit a.txt a".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&session).await;
    scene.free_space.set(0);
    std::fs::write(&go, "").unwrap();

    let floor = |seen: &[String]| {
        seen.iter()
            .filter(|t| t.contains("below the disk floor"))
            .count()
    };
    let mut seen = Vec::new();
    wait_for("the wait below the floor", LIMIT, async || {
        seen.extend(trouble(&mut sub));
        floor(&seen) > 0 || terminal(scene.state(1).await)
    })
    .await;
    // Many polls (50 ms each) later it still waits.
    tokio::time::sleep(Duration::from_secs(1)).await;
    seen.extend(trouble(&mut sub));
    assert_eq!(scene.state(1).await, Some(State::Verifying), "{seen:#?}");
    assert_eq!(floor(&seen), 1, "{seen:#?}");
    let line = seen
        .iter()
        .find(|t| t.contains("below the disk floor"))
        .unwrap();
    assert!(line.contains("1000000000"), "{line}");

    scene.free_space.set(u64::MAX);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert_eq!(state, State::Done(Outcome::Pass));
    let transitions = scene.store.reader().transitions(&item(1)).await.unwrap();
    assert!(
        transitions.iter().all(|t| t.to != "needs_you"),
        "{transitions:?}"
    );
}

/// What an earlier daemon left: the daemon's clone, and the worktree of card `card` (an item
/// not on the board, so never in flight) made and recorded as the claim makes one. Returns the
/// workspace, the worktree and the loop state it was made with.
async fn plant(
    scene: &Scene,
    card: u64,
) -> (workspace::Workspace, bosun_git::worktree::Worktree, Shared) {
    let config = scene.config();
    let ws = workspace::Workspace::of(&config.repos["demo"]).unwrap();
    ws.ensure_clone().unwrap();
    ws.sync_target("main").unwrap();
    scene
        .store
        .writer()
        .write(vec![Write::Item(bosun_store::NewItem {
            item: item(card),
            // Not the repository's key: recovery never takes it for one of its items.
            name: "planted".to_owned(),
            repo: "planted".to_owned(),
            card_key: Some(card.to_string()),
            at_ms: 1,
        })])
        .await
        .unwrap();
    let shared = scene.shared();
    let made = ws
        .open(
            &shared,
            &item(card),
            &item(card).0,
            "refs/heads/main",
            WorktreeKind::Item,
            None,
        )
        .await
        .unwrap();
    (ws, made, shared)
}

/// Before the daemon starts, an earlier one's worktree of card 7 (not on the board) is there,
/// recorded, with an uncommitted `planted.txt`; then card 1 runs to Done. The planted worktree
/// is closed as no item's: its row is `removed`, its directory gone, its work (the file) kept in
/// the clone under card 7's aside ref, and the root holds only the clone
/// (`mutant_loop_no_worktree_sweep` leaves it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_no_item_owns_is_closed_at_the_next_claim() {
    let scene = Scene::with_worktrees(None).await;
    let (_, planted, _) = plant(&scene, 7).await;
    std::fs::write(planted.path.join("planted.txt"), "planted\n").unwrap();
    scene.write_board(&[card(
        1,
        "After the planted one",
        "test -f one.txt",
        "Item one.",
    )]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let rows = rows(&scene, 7).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].state, WorktreeState::Removed, "{rows:?}");
    assert!(!planted.path.exists(), "{}", planted.path.display());
    let clone = scene.worktree_root().join(workspace::CLONE);
    let aside = git::aside_ref(&item(7));
    let files = git_in(&scene, &clone, &["ls-tree", "-r", "--name-only", &aside]);
    assert!(files.lines().any(|f| f == "planted.txt"), "{files}");
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
    assert_eq!(git_in(&scene, &clone, &["branch", "--list", "bosun/*"]), "");
}

/// An earlier daemon's worktree of card 7 had a file held open when it was closed: it is
/// quarantined. The file is let go, and card 1 runs to Done: the quarantined worktree is
/// tried again and removed, its row `removed` and its directory and marker gone
/// (`mutant_loop_quarantine_not_retried` leaves it quarantined).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quarantined_worktree_is_removed_once_it_is_free() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let scene = Scene::with_worktrees(None).await;
    let (ws, planted, shared) = plant(&scene, 7).await;
    // Held as an editor holds it: no sharing at all, deletion included.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(planted.path.join("README.md"))
        .unwrap();
    let config = scene.config();
    let closed_as = ws
        .close(&shared, &config.identities["test"], &planted, None, None)
        .await;
    assert!(
        matches!(closed_as, workspace::Closed::Quarantined(_)),
        "{closed_as:?}"
    );
    assert_eq!(rows(&scene, 7).await[0].state, WorktreeState::Quarantined);
    drop(handle);

    scene.write_board(&[card(
        1,
        "After the quarantine",
        "test -f one.txt",
        "Item one.",
    )]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let rows = rows(&scene, 7).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].state, WorktreeState::Removed, "{rows:?}");
    assert!(!planted.path.exists(), "{}", planted.path.display());
    assert_eq!(root_entries(&scene), [workspace::CLONE]);
}

/// With `setup = "echo no; exit 3"` the item ends Needs you (`setup`), and its Needs-you
/// comment says what failed: that the worktree could not be made or set up, and how the command
/// ended (`mutant_loop_setup_comment_without_cause` stores no cause: the comment does not say).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_setup_says_what_failed_in_its_comment() {
    let scene = Scene::with_worktrees(Some("echo no; exit 3")).await;
    scene.write_board(&[card(1, "Set up fails", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    assert_eq!(state, State::NeedsYou(Reason::Setup));

    let mut body = String::new();
    wait_for("the Needs-you comment", LIMIT, async || {
        let card = scene.cards().into_iter().find(|c| c.id.0 == 1).unwrap();
        if let Some(c) = card
            .comments
            .iter()
            .find(|c| c.key.is_some() && c.body.contains("needs you"))
        {
            body.clone_from(&c.body);
        }
        !body.is_empty()
    })
    .await;
    task.abort();
    watcher.abort();

    assert!(body.contains("could not be made or set up"), "{body}");
    assert!(body.contains("exited 3"), "{body}");
}
