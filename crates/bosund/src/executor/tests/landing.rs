//! Landing in worktree mode (#8 "Landing"; E2, E14, E20): a verified item is rebased onto the
//! target's head in the user's repository, verified again on the rebased commit, and the
//! target is fast-forwarded there, its clean checkout with it; a dirty one defers the landing
//! and holds the repository's queue.
//!
//! The scenes move the user's target (or dirty the user's checkout) at a fixed moment: after
//! the checkout guard and before the landing, from the repository's `setup` command, which runs
//! in every verdict checkout just before the verdict. The script acts once, and only in a
//! checkout that has the session's `a.txt` (the item's worktree at claim has not).

use std::collections::BTreeSet;

use bosun_core::RunResult;

use super::worktrees::{closed, git_in, item, rows, subscribe_trouble, trailer_in, trouble};
use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(180);

/// Makes the repository's `setup` a script that runs `then` (shell lines) once, in the first
/// checkout that has `a.txt`, and rewrites the configuration.
pub(super) fn setup_once(scene: &mut Scene, then: &str) {
    let script = scene.path("setup.sh");
    let marker = sh_path(&scene.path("setup-done"));
    std::fs::write(
        &script,
        format!("if [ -f a.txt ] && [ ! -f '{marker}' ]; then\n  touch '{marker}'\n  {then}\nfi\n"),
    )
    .unwrap();
    scene.setup = Some(format!("sh \"{}\"", sh_path(&script)));
    configure(scene);
}

/// Rewrites the scene's configuration after its fields changed.
fn configure(scene: &Scene) {
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
}

/// Shell lines that commit `file` on the user's target, in the user's repository.
fn commit_on_target(scene: &Scene, file: &str, subject: &str) -> String {
    format!(
        "cd '{}' && echo {file} > {file} && git add {file} && \
         git -c commit.gpgSign=false commit -q -m '{subject}'",
        sh_path(&scene.repo)
    )
}

/// The verdict runs of `card` that passed.
async fn passed(scene: &Scene, card: u64) -> usize {
    scene
        .store
        .reader()
        .verdict_runs(&item(card))
        .await
        .unwrap()
        .iter()
        .filter(|r| r.result == Some(RunResult::Pass))
        .count()
}

/// The daemon's clone.
fn clone_of(scene: &Scene) -> PathBuf {
    scene.worktree_root().join(workspace::CLONE)
}

/// Whether `reference` exists in the repository at `dir`.
fn has_ref(scene: &Scene, dir: &Path, reference: &str) -> bool {
    without_session(&mut Command::new("git"))
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .env("GIT_CONFIG_GLOBAL", &scene.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success()
}

/// The files of commit `rev` of the user's repository.
pub(super) fn files_at(scene: &Scene, rev: &str) -> BTreeSet<String> {
    scene
        .git(&["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The session commits `a.txt`; the setup script commits `b.txt` on the user's target once,
/// before the first verdict. The item ends Done in one attempt: the target's head is the item's
/// commit, rebased onto the `b.txt` commit (directly below it on the first-parent chain), with
/// both files and the three trailers; the user's checkout is clean with both files on disk; two
/// passing verdict runs (before and after the rebase); the worktree is gone and no aside ref
/// was made (`mutant_loop_land_ref_only` moves only the ref: the checkout is neither clean nor
/// has the files).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_that_moved_is_rebased_onto_and_verified_again() {
    let mut scene = Scene::with_worktrees(None).await;
    let then = commit_on_target(&scene, "b.txt", "Add b");
    setup_once(&mut scene, &then);
    scene.write_board(&[card(1, "Moved target", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let start = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let attempt = scene.item(1).await.unwrap().attempt.unwrap();
    assert_eq!(attempt.number, 1);
    let chain = scene.git(&["rev-list", "--first-parent", "HEAD"]);
    let chain: Vec<&str> = chain.lines().collect();
    assert_eq!(chain.len(), 3, "{chain:?}");
    assert_eq!(chain[2], start);
    assert_eq!(scene.git(&["log", "-1", "--format=%s", chain[1]]), "Add b");
    assert_eq!(scene.git(&["log", "-1", "--format=%s", chain[0]]), "a");
    assert_eq!(scene.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
    let files = files_at(&scene, "HEAD");
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files:?}"
    );
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(trailer_in(&scene, &scene.repo, "HEAD", "Item"), ["demo#1"]);
    assert_eq!(trailer_in(&scene, &scene.repo, "HEAD", "Run"), ["a1s1"]);
    assert_eq!(
        trailer_in(&scene, &scene.repo, "HEAD", "Session"),
        [runs[0].session_key.clone()]
    );
    assert_eq!(
        scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert!(scene.repo.join("a.txt").is_file());
    assert!(scene.repo.join("b.txt").is_file());
    assert_eq!(passed(&scene, 1).await, 2);

    let rows = rows(&scene, 1).await;
    assert!(
        rows.iter().all(|r| !Path::new(&r.path).exists()),
        "{rows:?}"
    );
    let clone = clone_of(&scene);
    assert!(!has_ref(&scene, &clone, &git::aside_ref(&item(1))));
    assert!(!has_ref(&scene, &clone, &git::landing_ref(&item(1))));
}

/// The user's repository has `core.autocrlf=true`; the session commits `a.txt` (`a` and a
/// newline). The item ends Done and the user's checkout is what the user's own git would have
/// checked out: clean, with `a.txt` on disk as `a\r\n` (E2: the checkout is untouched apart
/// from a clean fast-forward, whatever its line-ending policy;
/// `mutant_git_ff_ignores_eol` fast-forwards with `core.autocrlf=false`: `a.txt` lands as
/// `a\n`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkout_with_autocrlf_lands_clean_with_its_line_endings() {
    let scene = Scene::with_worktrees(None).await;
    scene.git(&["config", "core.autocrlf", "true"]);
    scene.write_board(&[card(1, "Line endings", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD"]), "a");
    assert_eq!(
        scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert_eq!(std::fs::read(scene.repo.join("a.txt")).unwrap(), b"a\r\n");
}

/// The setup script commits `breaker` on the user's target once; the verdict needs `a.txt`
/// and no `breaker`. The first attempt passes before the rebase and fails after it; the second
/// attempt has verdict runs of its own (its preconditions passed: the daemon's own moves of the
/// target were not taken for tampering) and fails too. The item ends Needs you
/// (`attempts_exhausted`), and the user's target is still the `breaker` commit, without `a.txt`
/// (`mutant_loop_accept_pre_rebase_verdict` lands it next to `breaker`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebase_that_breaks_the_verdict_does_not_land() {
    let mut scene = Scene::with_worktrees(None).await;
    let then = commit_on_target(&scene, "breaker", "Add breaker");
    setup_once(&mut scene, &then);
    scene.write_board(&[card(
        1,
        "Broken by the target",
        "test -f a.txt && test ! -f breaker",
        "Item one.",
    )]);
    let marker = sh_path(&scene.path("session-1"));
    scene.script(1, &[format!("once {marker} commit a.txt a")]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert_eq!(state, State::NeedsYou(Reason::AttemptsExhausted));
    let attempt = scene.item(1).await.unwrap().attempt.unwrap();
    assert_eq!(attempt.number, 2);
    let verdicts = scene.store.reader().verdict_runs(&item(1)).await.unwrap();
    let attempts: BTreeSet<u32> = verdicts.iter().map(|r| r.attempt).collect();
    assert_eq!(attempts, BTreeSet::from([1, 2]), "{verdicts:?}");
    assert!(
        verdicts
            .iter()
            .any(|r| r.attempt == 1 && r.result == Some(RunResult::Pass)),
        "{verdicts:?}"
    );
    assert_eq!(
        scene.git(&["log", "-1", "--format=%s", "HEAD"]),
        "Add breaker"
    );
    let files = files_at(&scene, "HEAD");
    assert!(!files.contains("a.txt"), "{files:?}");
    assert!(!scene.repo.join("a.txt").exists());
}

/// One scene per merge method (`landing = { mode = "merge", merge = { method = ... } }`); the
/// session makes two commits and submits a summary. In each the user's target ends with the
/// tree of the item's head: `ff` is the item's head; `squash` one commit on the old target head
/// whose subject is the card's title and whose message ends with the `Item:`, `Run:` and
/// `Session:` trailers; `no-ff` a merge of the old target head and the item's head with the
/// trailers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_merge_method_lands_the_verified_tree_with_the_trailers() {
    for method in ["ff", "squash", "no-ff"] {
        let mut scene = Scene::with_worktrees(None).await;
        scene.config_more =
            format!("[repo.demo.landing]\nmode = \"merge\"\nmerge = {{ method = \"{method}\" }}\n");
        configure(&scene);
        let title = format!("Land by {method}");
        scene.write_board(&[card(
            1,
            &title,
            "test -f a.txt && test -f c.txt",
            "Item one.",
        )]);
        scene.script(
            1,
            &[
                "commit a.txt a".to_owned(),
                "commit c.txt c".to_owned(),
                r#"mcp bosun submit_summary {"body": "Added a.txt and c.txt."}"#.to_owned(),
            ],
        );
        let old = scene.git(&["rev-parse", "HEAD"]);
        let (task, watcher) = scene.start(None);
        let state = ended(&scene, 1, LIMIT).await;
        closed(&scene, 1).await;
        task.abort();
        watcher.abort();

        assert!(matches!(state, State::Done(_)), "{method}: {state:?}");
        let runs = scene.runs(1).await;
        let item_head = runs
            .iter()
            .rev()
            .find_map(|r| r.head_sha.clone())
            .expect("the session's head");
        let clone = clone_of(&scene);
        let head = scene.git(&["rev-parse", "HEAD"]);
        assert_eq!(
            scene.git(&["rev-parse", "HEAD^{tree}"]),
            git_in(
                &scene,
                &clone,
                &["rev-parse", &format!("{item_head}^{{tree}}")]
            ),
            "{method}"
        );
        assert_eq!(
            scene.git(&["status", "--porcelain", "--untracked-files=all"]),
            "",
            "{method}"
        );
        let parents: Vec<String> = scene
            .git(&["rev-list", "--parents", "-n1", "HEAD"])
            .split(' ')
            .skip(1)
            .map(str::to_owned)
            .collect();
        match method {
            "ff" => {
                assert_eq!(head, item_head, "ff");
                assert_eq!(trailer_in(&scene, &scene.repo, "HEAD", "Item"), ["demo#1"]);
            }
            "squash" => {
                assert_eq!(parents, std::slice::from_ref(&old), "squash");
                assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD"]), title);
                let body = scene.git(&["log", "-1", "--format=%B", "HEAD"]);
                let last: Vec<&str> = body
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .rev()
                    .take(3)
                    .collect();
                assert_eq!(
                    last.iter()
                        .rev()
                        .map(|l| l.split(':').next().unwrap())
                        .collect::<Vec<_>>(),
                    ["Item", "Run", "Session"],
                    "{body}"
                );
            }
            _ => {
                assert_eq!(parents, [old.clone(), item_head.clone()], "no-ff");
            }
        }
        assert_eq!(
            trailer_in(&scene, &scene.repo, "HEAD", "Item"),
            ["demo#1"],
            "{method}"
        );
        assert_eq!(
            trailer_in(&scene, &scene.repo, "HEAD", "Run"),
            ["a1s1"],
            "{method}"
        );
        assert_eq!(
            trailer_in(&scene, &scene.repo, "HEAD", "Session"),
            [runs[0].session_key.clone()],
            "{method}"
        );
    }
}

/// A sequence editor for `git rebase -i` that stops the rebase after its first pick: it inserts
/// `break` after the first line of the todo, with shell builtins only (no `sed` on the path).
const BREAK_AFTER_FIRST: &str = "#!/bin/sh
out=
first=1
while IFS= read -r line; do
  if [ -n \"$first\" ]; then out=\"$line
break\"; first=; else out=\"$out
$line\"; fi
done < \"$1\"
printf '%s\n' \"$out\" > \"$1\"
";

/// The session commits `a.txt` (c1), then `b.txt` (c2), then leaves its worktree mid-rebase:
/// `git rebase -i HEAD~2` with a `break` after the first pick stops with exit 0, its head
/// detached at c1, `rebase-merge` present, the item's branch still at c2. The verdict would pass
/// on c1 already. The item ends Done: the rebase is aborted (reported once) before its head is
/// checked, verified or landed, so the user's target head is c2 with both files
/// (`mutant_loop_land_over_stale_rebase` leaves the rebase: c1 is never what lands).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebase_left_in_progress_is_aborted_before_landing() {
    let scene = Scene::with_worktrees(None).await;
    let editor = scene.path("break.sh");
    // Git executes the editor itself (its path has no shell metacharacters), so on Unix it
    // must be executable; Git for Windows runs it through `sh` either way.
    write_executable(&editor, BREAK_AFTER_FIRST);
    let mut sub = subscribe_trouble(&scene);
    scene.write_board(&[card(1, "Left mid-rebase", "test -f a.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit a.txt a".to_owned(),
            "commit b.txt b".to_owned(),
            format!(
                "git -c sequence.editor={} -c commit.gpgSign=false rebase -i HEAD~2",
                sh_path(&editor)
            ),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let files = files_at(&scene, "HEAD");
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files:?}"
    );
    assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD"]), "b");
    assert_eq!(
        scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    let seen = trouble(&mut sub);
    assert_eq!(
        seen.iter()
            .filter(|t| t.contains("a rebase was left in progress"))
            .count(),
        1,
        "{seen:#?}"
    );
}

/// The identity file's author, which every commit the daemon makes carries (E26).
const IDENTITY: &str = "Synthetic Agent <agent@example.com>";

/// `landing = { mode = "merge", merge = { method = "no-ff" } }`; the session commits `a.txt`
/// and the setup script commits `b.txt` on the user's target once, so the item is rebased. The
/// item ends Done; on the user's target the merge commit's author and committer are the
/// repository identity's, and so is the committer of its second parent, the rebased item
/// commit. The daemon's clone has no identity of its own and the tests run with no global one
/// (E26), so a commit made without the identity's author is refused, or carries whatever global
/// identity the process has (`mutant_loop_ambient_commit_identity`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemons_commits_carry_the_repository_identity() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more =
        "[repo.demo.landing]\nmode = \"merge\"\nmerge = { method = \"no-ff\" }\n".to_owned();
    let then = commit_on_target(&scene, "b.txt", "Add b");
    setup_once(&mut scene, &then);
    scene.write_board(&[card(1, "Identity", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD^1"]), "Add b");
    assert_eq!(scene.git(&["log", "-1", "--format=%s", "HEAD^2"]), "a");
    let ident =
        |format: &str, rev: &str| scene.git(&["log", "-1", &format!("--format={format}"), rev]);
    assert_eq!(ident("%an <%ae>", "HEAD"), IDENTITY, "the merge's author");
    assert_eq!(
        ident("%cn <%ce>", "HEAD"),
        IDENTITY,
        "the merge's committer"
    );
    assert_eq!(
        ident("%cn <%ce>", "HEAD^2"),
        IDENTITY,
        "the rebased commit's committer"
    );
}

/// The setup script writes an untracked file into the user's checkout once, before the first
/// item's verdict. With two cards in Ready, the first reaches Landing and stays there: the
/// user's target and files are unchanged, a trouble event says the fast-forward is deferred,
/// and the second card is not claimed. The test deletes the file: the first item ends Done with
/// its commit on the target, then the second card is claimed and ends Done
/// (`mutant_loop_land_while_deferred` reports the deferred landing as landed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dirty_checkout_of_the_target_defers_the_landing_and_holds_the_queue() {
    let mut scene = Scene::with_worktrees(None).await;
    let scratch = scene.repo.join("scratch.txt");
    let then = format!("echo mine > '{}'", sh_path(&scratch));
    setup_once(&mut scene, &then);
    let mut sub = subscribe_trouble(&scene);
    scene.write_board(&[
        card(1, "First", "test -f a.txt", "Item one."),
        card(2, "Second", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let start = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);

    let mut seen = Vec::new();
    wait_for("the deferred fast-forward", LIMIT, async || {
        seen.extend(trouble(&mut sub));
        seen.iter().any(|t| t.contains("is deferred")) || terminal(scene.state(1).await)
    })
    .await;
    assert!(seen.iter().any(|t| t.contains("is deferred")), "{seen:#?}");
    // Well past a few retries of the fast-forward.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(scene.state(1).await, Some(State::Landing));
    let second = scene.state(2).await;
    assert!(
        second.is_none() || second == Some(State::Ready),
        "{second:?}"
    );
    assert_eq!(scene.runs(2).await, []);
    assert_eq!(scene.git(&["rev-parse", "HEAD"]), start);
    assert_eq!(scene.git(&["rev-parse", "refs/heads/main"]), start);
    assert_eq!(
        scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        "?? scratch.txt"
    );
    assert!(!scene.repo.join("a.txt").exists());

    std::fs::remove_file(&scratch).unwrap();
    let first = ended(&scene, 1, LIMIT).await;
    assert!(matches!(first, State::Done(_)), "{first:?}");
    assert_eq!(scene.git(&["rev-parse", "HEAD~1"]), start);
    assert!(files_at(&scene, "HEAD").contains("a.txt"));
    assert!(scene.repo.join("a.txt").is_file());
    let second = ended(&scene, 2, LIMIT).await;
    closed(&scene, 2).await;
    task.abort();
    watcher.abort();
    assert!(matches!(second, State::Done(_)), "{second:?}");
    let files = files_at(&scene, "HEAD");
    assert!(
        files.contains("a.txt") && files.contains("two.txt"),
        "{files:?}"
    );
}

/// The first session saves its prompt and commits `a.txt` with one content; the setup script
/// commits `a.txt` with another content on the user's target once, before the first verdict.
/// The rebase at landing conflicts, and the machine starts one more session of the attempt: it
/// saves its prompt, resets its branch onto the target (the worktree shares the clone's refs)
/// and commits `a.txt` with a merged content. The item ends Done in one attempt with two
/// sessions and one conflict; the second prompt begins with the daemon's conflict section,
/// naming `a.txt` and the target's commit, the first has none; the user's target head has the
/// merged content and the target's own commit in its history (`mutant_loop_conflict_note_unread`
/// never passes the note on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflict_gets_one_session_with_the_conflict_as_its_prompt_and_then_lands() {
    let mut scene = Scene::with_worktrees(None).await;
    let then = commit_on_target(&scene, "a.txt", "Add a on the target");
    setup_once(&mut scene, &then);
    scene.write_board(&[card(1, "Conflicting", "test -f a.txt", "Item one.")]);
    let p = |name: &str| sh_path(&scene.path(name));
    scene.script(
        1,
        &[
            format!("once {} stdin {}", p("m1"), p("prompt-1.txt")),
            format!("once {} commit a.txt a", p("m2")),
            format!(
                "once {} replay {}",
                p("m3"),
                Scene::stream("normal-end.jsonl")
            ),
            format!("once {} exit 0", p("m4")),
            format!("stdin {}", p("prompt-2.txt")),
            "git reset -q --hard main".to_owned(),
            "commit a.txt merged".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let it = scene.item(1).await.unwrap();
    assert_eq!(it.conflicts, 1, "{it:?}");
    assert_eq!(it.attempt.as_ref().unwrap().number, 1);
    let runs = scene.runs(1).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs.iter().all(|r| r.attempt == 1), "{runs:?}");

    // The target's own commit: the parent of the item's commit on the target.
    let onto = scene.git(&["rev-parse", "HEAD~1"]);
    assert_eq!(
        scene.git(&["log", "-1", "--format=%s", &onto]),
        "Add a on the target"
    );
    assert_eq!(scene.git(&["show", "HEAD:a.txt"]), "merged");
    assert_eq!(
        scene.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );

    let first = std::fs::read_to_string(scene.path("prompt-1.txt")).unwrap();
    assert!(first.starts_with("# Task\n\n"), "{first}");
    assert!(!first.contains("rebase conflict"), "{first}");
    let second = std::fs::read_to_string(scene.path("prompt-2.txt")).unwrap();
    assert!(
        second.starts_with("# A rebase conflict (written by bosun)"),
        "{second}"
    );
    let (section, rest) = second.split_once("# Task\n\n").expect("the card's text");
    assert!(section.contains("- `a.txt`"), "{section}");
    assert!(
        section.contains(&format!("`git rebase {onto}`")),
        "{section}"
    );
    assert!(section.contains("the target branch `main`"), "{section}");
    assert!(rest.contains("Item one."), "{rest}");
}

/// The setup script writes an untracked file into the user's checkout once, before the
/// verdict, so the item's fast-forward is deferred and the clone offers the landed commit under
/// its landing ref. When the deferral is reported the user moves the card to Backlog. The item
/// ends Cancelled, the clone has no `refs/bosun/landing/` ref left, and the user's target is
/// unchanged (`mutant_loop_keep_landing_ref` keeps the offer, with its objects, for ever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_deferred_landing_leaves_no_landing_ref() {
    let mut scene = Scene::with_worktrees(None).await;
    let scratch = scene.repo.join("scratch.txt");
    let then = format!("echo mine > '{}'", sh_path(&scratch));
    setup_once(&mut scene, &then);
    let mut sub = subscribe_trouble(&scene);
    scene.write_board(&[card(1, "Cancelled", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let start = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);

    let mut seen = Vec::new();
    wait_for("the deferred fast-forward", LIMIT, async || {
        seen.extend(trouble(&mut sub));
        seen.iter().any(|t| t.contains("is deferred")) || terminal(scene.state(1).await)
    })
    .await;
    assert!(seen.iter().any(|t| t.contains("is deferred")), "{seen:#?}");
    let clone = clone_of(&scene);
    assert!(has_ref(&scene, &clone, &git::landing_ref(&item(1))));

    let mut cards = scene.cards();
    cards[0].column = Column::Backlog;
    scene.write_board(&cards);
    let mut state = None;
    wait_for("item 1 to settle", LIMIT, async || {
        state = scene.state(1).await;
        state.is_some_and(|s| !s.is_active())
    })
    .await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert_eq!(state, Some(State::Cancelled));
    assert_eq!(
        git_in(
            &scene,
            &clone,
            &["for-each-ref", "--format=%(refname)", "refs/bosun/landing/"]
        ),
        ""
    );
    assert_eq!(scene.git(&["rev-parse", "refs/heads/main"]), start);
    assert!(!scene.repo.join("a.txt").exists());
}

/// Before the run the user's repository has a `FETCH_HEAD` of its own. A `local` item lands;
/// the `FETCH_HEAD` is byte for byte what it was: landing fetches objects only (E14)
/// (`mutant_loop_landing_fetch_writes_fetch_head` lets the fetch write it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landing_leaves_the_users_fetch_head_alone() {
    let scene = Scene::with_worktrees(None).await;
    let fetch_head = PathBuf::from(scene.git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "FETCH_HEAD",
    ]));
    let start = scene.git(&["rev-parse", "HEAD"]);
    let mine = format!("{start}\t\tbranch 'topic' of ../elsewhere\n");
    std::fs::write(&fetch_head, &mine).unwrap();
    scene.write_board(&[card(1, "Landed", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(scene.git(&["rev-parse", "HEAD~1"]), start);
    assert_eq!(std::fs::read(&fetch_head).unwrap(), mine.as_bytes());
}

/// One pull request of the [`FakeForge`].
#[derive(Debug, Clone)]
pub(super) struct FakePr {
    pub(super) number: u64,
    pub(super) new: forge::NewPullRequest,
    /// The body now: the one it was opened with, or what was set or edited since.
    pub(super) body: String,
}

/// The forge of the `pr` scenes: pull requests in memory, and the answers a scene asks for.
#[derive(Debug, Default)]
pub(super) struct FakeForge {
    prs: std::sync::Mutex<Vec<FakePr>>,
    /// Every call is refused with this.
    refuse: std::sync::Mutex<Option<String>>,
    /// While set, every call goes unanswered ([`forge::ForgeError::Unreachable`]).
    down: AtomicBool,
    /// The calls answered unreachable.
    unanswered: AtomicUsize,
    /// The calls of `create`.
    creates: AtomicUsize,
    /// The next `create` stops after it opened the pull request ([`FakeForge::paused`]) and
    /// returns only once [`FakeForge::release`] is called.
    pause_after_create: AtomicBool,
    paused: AtomicBool,
    released: AtomicBool,
}

impl FakeForge {
    fn prs(&self) -> Vec<FakePr> {
        self.prs.lock().unwrap().clone()
    }

    /// The answer every call gets before it is served, if not served.
    fn answer(&self) -> Result<(), forge::ForgeError> {
        if let Some(why) = self.refuse.lock().unwrap().clone() {
            return Err(forge::ForgeError::Refused(why));
        }
        if self.down.load(Ordering::SeqCst) {
            self.unanswered.fetch_add(1, Ordering::SeqCst);
            return Err(forge::ForgeError::Unreachable(
                "Could not resolve host: github.com".to_owned(),
            ));
        }
        Ok(())
    }

    /// A person edits the body of pull request `number`.
    fn edit(&self, number: u64, edit: impl FnOnce(&str) -> String) {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs.iter_mut().find(|p| p.number == number).unwrap();
        pr.body = edit(&pr.body);
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl forge::Forge for FakeForge {
    fn open_pull_request(
        &self,
        _at: &forge::At,
        head_branch: &str,
    ) -> Result<Option<forge::PullRequest>, forge::ForgeError> {
        self.answer()?;
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.new.head == head_branch)
            .map(|p| forge::PullRequest {
                number: p.number,
                url: url_of(p.number),
                body: p.body.clone(),
            }))
    }

    fn create(
        &self,
        _at: &forge::At,
        new: &forge::NewPullRequest,
    ) -> Result<forge::PullRequest, forge::ForgeError> {
        self.answer()?;
        self.creates.fetch_add(1, Ordering::SeqCst);
        let number = {
            let mut prs = self.prs.lock().unwrap();
            let number = prs.len() as u64 + 1;
            prs.push(FakePr {
                number,
                new: new.clone(),
                body: new.body.clone(),
            });
            number
        };
        if self.pause_after_create.swap(false, Ordering::SeqCst) {
            self.paused.store(true, Ordering::SeqCst);
            while !self.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(forge::PullRequest {
            number,
            url: url_of(number),
            body: new.body.clone(),
        })
    }

    fn set_body(&self, _at: &forge::At, number: u64, body: &str) -> Result<(), forge::ForgeError> {
        self.answer()?;
        self.edit(number, |_| body.to_owned());
        Ok(())
    }
}

fn url_of(number: u64) -> String {
    format!("https://github.com/synthetic-owner/demo/pull/{number}")
}

/// The scene's `origin`, which the identity checks read.
const ORIGIN: &str = "https://github.com/synthetic-owner/demo.git";

/// A scene in worktree mode with `landing = { mode = "pr" }` and `more` configuration, a
/// [`FakeForge`], and a bare repository in the scene's directory that pushes to `origin` go to
/// instead (`pushInsteadOf` in the identity's git configuration file: `git remote get-url`
/// still prints the synthetic URL). Nothing is pushed anywhere else.
async fn pr_scene(more: &str) -> (Scene, Arc<FakeForge>, PathBuf) {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = format!("[repo.demo.landing]\nmode = \"pr\"\n{more}");
    configure(&scene);
    let remote = scene.path("remote.git");
    let out = without_session(&mut Command::new("git"))
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(&remote)
        .env("GIT_CONFIG_GLOBAL", &scene.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let path = remote.to_string_lossy().replace('\\', "/");
    let url = if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    };
    let mut config = std::fs::read_to_string(&scene.gitconfig).unwrap();
    let _ = write!(config, "[url \"{url}\"]\n\tpushInsteadOf = {ORIGIN}\n");
    std::fs::write(&scene.gitconfig, config).unwrap();
    let fake = Arc::new(FakeForge::default());
    scene.forge = Some(fake.clone());
    (scene, fake, remote)
}

/// The branches under `refs/heads/bosun/` of the bare remote, with their commits.
fn pushed(scene: &Scene, remote: &Path) -> Vec<String> {
    git_in(
        scene,
        remote,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/bosun/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// The daemon's region of a pull request body is there.
fn has_region(body: &str) -> bool {
    body.contains(&bosun_core::publish::marker("pr:facts:start"))
        && body.contains(&bosun_core::publish::marker("pr:facts:end"))
}

/// `landing = { mode = "pr" }`: the session commits `a.txt` and submits a summary. The item
/// ends Done; the bare remote has `refs/heads/bosun/demo-1` at the item's verified head; the
/// forge holds one draft pull request of that branch into `main`, titled as the card, whose
/// body has the summary, the daemon's region (the verified head in it) and `Closes #1`; the
/// landing row has its number and `open`. The user's target did not move, and neither the
/// user's repository nor the clone has a ref under `refs/bosun/landing/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr_mode_ends_done_with_one_open_draft_pull_request() {
    let (scene, fake, remote) = pr_scene("").await;
    assert_eq!(scene.git(&["remote", "get-url", "origin"]), ORIGIN);
    scene.write_board(&[card(1, "Proposed", "test -f a.txt", "Item one.")]);
    scene.script(
        1,
        &[
            "commit a.txt a".to_owned(),
            r#"mcp bosun submit_summary {"body": "Added a.txt for the proposal."}"#.to_owned(),
        ],
    );
    let start = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let row = scene
        .store
        .reader()
        .landing(&item(1))
        .await
        .unwrap()
        .expect("the landing row");
    let head = row.head_sha.clone().expect("the verified head");
    assert_eq!(row.landed_sha.as_deref(), Some(head.as_str()), "{row:?}");
    assert_eq!(row.pr_number, Some(1), "{row:?}");
    assert_eq!(row.pr_state, Some(bosun_store::PrState::Open), "{row:?}");
    assert_eq!(row.pr_url.as_deref(), Some(url_of(1).as_str()), "{row:?}");
    assert_eq!(
        pushed(&scene, &remote),
        [format!("refs/heads/bosun/demo-1 {head}")]
    );

    let prs = fake.prs();
    assert_eq!(prs.len(), 1, "{prs:#?}");
    let pr = &prs[0];
    assert!(pr.new.draft, "{pr:#?}");
    assert_eq!(pr.new.head, "bosun/demo-1");
    assert_eq!(pr.new.base, "main");
    assert_eq!(pr.new.title, "Proposed");
    assert!(
        pr.new.reviewers.is_empty() && pr.new.labels.is_empty(),
        "{pr:#?}"
    );
    assert!(
        pr.body.starts_with("Added a.txt for the proposal."),
        "{}",
        pr.body
    );
    assert!(has_region(&pr.body), "{}", pr.body);
    assert!(pr.body.contains(&head), "{}", pr.body);
    assert!(pr.body.trim_end().ends_with("Closes #1"), "{}", pr.body);

    assert_eq!(scene.git(&["rev-parse", "refs/heads/main"]), start);
    assert_eq!(scene.git(&["rev-parse", "HEAD"]), start);
    assert_eq!(
        scene.git(&["for-each-ref", "--format=%(refname)", "refs/bosun/landing/"]),
        ""
    );
    assert_eq!(
        git_in(
            &scene,
            &clone_of(&scene),
            &["for-each-ref", "--format=%(refname)", "refs/bosun/landing/"]
        ),
        ""
    );
}

/// The fake forge opens the pull request and then holds the landing task before the row is
/// stored; a person edits its body meanwhile (text above and below the daemon's region), and
/// the test stops the loop and starts it again: recovery redoes the landing. The item ends
/// Done; the forge holds one pull request and was asked to create one once; the remote has one
/// `bosun/` branch; the body keeps the person's text and the region
/// (`mutant_loop_pr_create_without_lookup` opens a second one; `mutant_loop_pr_body_overwritten`
/// writes the body afresh).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_landing_redone_after_the_pull_request_opened_opens_no_second_one() {
    let (scene, fake, remote) = pr_scene("").await;
    fake.pause_after_create.store(true, Ordering::SeqCst);
    scene.write_board(&[card(1, "Redone", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_for("the pull request to be opened", LIMIT, async || {
        fake.paused.load(Ordering::SeqCst) || terminal(scene.state(1).await)
    })
    .await;
    assert!(fake.paused.load(Ordering::SeqCst));
    assert_eq!(scene.state(1).await, Some(State::Landing));
    fake.edit(1, |body| {
        format!("A person wrote this above.\n\n{body}\n\nAnd this below.\n")
    });
    task.abort();
    watcher.abort();
    let _ = task.await;

    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();
    // The first loop's landing task, held in the fake, may go now.
    fake.release();

    assert!(matches!(state, State::Done(_)), "{state:?}");
    let prs = fake.prs();
    assert_eq!(prs.len(), 1, "{prs:#?}");
    assert_eq!(fake.creates.load(Ordering::SeqCst), 1);
    assert_eq!(pushed(&scene, &remote).len(), 1);
    let body = &prs[0].body;
    assert!(body.starts_with("A person wrote this above.\n\n"), "{body}");
    assert!(body.ends_with("\n\nAnd this below.\n"), "{body}");
    assert!(has_region(body), "{body}");
    assert!(body.contains("Closes #1"), "{body}");
    let row = scene.store.reader().landing(&item(1)).await.unwrap();
    let row = row.expect("the landing row");
    assert_eq!(
        (row.pr_number, row.pr_state),
        (Some(1), Some(bosun_store::PrState::Open)),
        "{row:?}"
    );
}

/// A lease a card may need in these scenes.
const RIG: &str = "[resource.rig]\nkind = \"device\"\ncapacity = 1\nscope = \"machine\"\n";

/// With the forge refusing (`branch protection`) the item ends Needs you (`landing_refused`)
/// in one attempt, none failed, the error in its `landing` note. With the forge unanswered
/// until the test lets it answer, a card that needs `rig` stays in Landing holding no lease
/// while the forge is asked again, and ends Done with one pull request
/// (`mutant_loop_pr_refusal_is_retried` waits the refusal out: the item never needs the user).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_pull_request_needs_the_user_and_an_unreachable_forge_keeps_landing() {
    let (scene, fake, _remote) = pr_scene("").await;
    *fake.refuse.lock().unwrap() = Some("branch protection".to_owned());
    scene.write_board(&[card(1, "Refused", "test -f a.txt", "Item one.")]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    // Bounded: a refusal waited out never ends.
    let mut state = None;
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        state = scene.state(1).await;
        if terminal(state) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    task.abort();
    watcher.abort();
    assert_eq!(state, Some(State::NeedsYou(Reason::LandingRefused)));
    let row = scene.item(1).await.unwrap();
    assert_eq!((row.attempts, row.failed), (1, 0), "{row:?}");
    let note = scene
        .store
        .reader()
        .item_note(&item(1), "landing")
        .await
        .unwrap()
        .expect("the landing note");
    assert!(note.contains("branch protection"), "{note}");
    assert!(fake.prs().is_empty());

    let (mut scene, fake, remote) = pr_scene(RIG).await;
    let leases = Arc::new(
        crate::leases::Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    fake.down.store(true, Ordering::SeqCst);
    scene.write_board(&[Card {
        body: "Item one.\n\n```bosun\nverdict = 'test -f a.txt'\nneeds = [\"rig\"]\n```\n"
            .to_owned(),
        ..card(1, "Unreachable", "true", "")
    }]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_for("the forge to be asked twice", LIMIT, async || {
        fake.unanswered.load(Ordering::SeqCst) >= 2 || terminal(scene.state(1).await)
    })
    .await;
    assert_eq!(scene.state(1).await, Some(State::Landing));
    let holder = crate::leases::Holder::Item(item(1));
    assert_eq!(
        leases.holds(&holder),
        Vec::<String>::new(),
        "held while waiting"
    );
    fake.down.store(false, Ordering::SeqCst);
    let state = ended(&scene, 1, LIMIT).await;
    closed(&scene, 1).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(fake.prs().len(), 1);
    assert_eq!(pushed(&scene, &remote).len(), 1);
}
