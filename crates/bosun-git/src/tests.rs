//! The invoker and the preconditions against throwaway repositories built with the system git
//! in temporary directories (#53).

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::*;

/// Runs `git args` in `dir` as an agent would, with no user or system configuration and no
/// signing, but with the repository's own hooks and fsmonitor live. Returns the trimmed
/// output.
fn agent(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "tag.gpgSign=false"])
        .args(["-c", "user.name=Test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository on `main` with one commit.
fn repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    agent(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
    commit(dir.path(), "a.txt", "Initial commit");
    dir
}

/// Writes `file` (its name as its content plus a counter, so each call changes it) and commits
/// everything with `message`. Returns the sha.
fn commit(dir: &Path, file: &str, message: &str) -> String {
    let path = dir.join(file);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, format!("{old}{file}\n")).unwrap();
    agent(dir, &["add", "--all"]);
    agent(dir, &["commit", "--quiet", "-m", message]);
    agent(dir, &["rev-parse", "HEAD"])
}

fn capture(dir: &Path) -> Base {
    Base::capture(&Git::new(dir)).unwrap()
}

#[test]
fn the_base_is_head_the_branch_every_ref_and_the_stash() {
    let repo = repository();
    let dir = repo.path();
    agent(dir, &["tag", "v1"]);
    std::fs::write(dir.join("a.txt"), "changed").unwrap();
    agent(dir, &["stash", "push", "--quiet"]);
    let head = agent(dir, &["rev-parse", "HEAD"]);
    let stash = agent(dir, &["rev-parse", "refs/stash"]);

    let base = capture(dir);
    assert_eq!(base.head, head);
    assert_eq!(base.branch.as_deref(), Some("refs/heads/main"));
    assert_eq!(
        base.refs.keys().map(String::as_str).collect::<Vec<_>>(),
        ["refs/heads/main", "refs/stash", "refs/tags/v1"]
    );
    assert_eq!(base.refs["refs/heads/main"], head);
    assert_eq!(base.stash, [stash]);

    agent(dir, &["checkout", "--quiet", "--detach"]);
    assert_eq!(capture(dir).branch, None);
}

#[test]
fn an_untracked_file_fails_clean_tree() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
    commit(dir, "b.txt", "Ignore logs");
    // The repository's configuration cannot hide untracked files from the check.
    agent(dir, &["config", "status.showUntrackedFiles", "no"]);

    // An ignored file is #59's business, not this check's.
    std::fs::write(dir.join("build.log"), "output").unwrap();
    assert_eq!(clean_tree(&git).unwrap(), []);

    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("new.txt"), "new").unwrap();
    assert_eq!(
        clean_tree(&git).unwrap(),
        [Reason::Dirty {
            entries: vec!["?? sub/new.txt".to_owned()]
        }]
    );
}

#[test]
fn uncommitted_changes_fail_clean_tree() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    std::fs::write(dir.join("a.txt"), "edited").unwrap();
    assert_eq!(
        clean_tree(&git).unwrap(),
        [Reason::Dirty {
            entries: vec![" M a.txt".to_owned()]
        }]
    );
    agent(dir, &["checkout", "--quiet", "--", "a.txt"]);
    assert_eq!(clean_tree(&git).unwrap(), []);
    agent(dir, &["mv", "a.txt", "renamed.txt"]);
    assert_eq!(
        clean_tree(&git).unwrap(),
        [Reason::Dirty {
            entries: vec!["D  a.txt".to_owned(), "A  renamed.txt".to_owned()]
        }]
    );
}

#[test]
fn zero_commits_fail_ancestry() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    let base = capture(dir);
    assert_eq!(ancestry(&git, &base).unwrap(), [Reason::NoCommits]);
    commit(dir, "a.txt", "Work");
    assert_eq!(ancestry(&git, &base).unwrap(), []);
}

#[test]
fn a_reset_below_base_and_a_recommit_fails_ancestry() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    commit(dir, "a.txt", "Second");
    let base = capture(dir);
    agent(dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let head = commit(dir, "c.txt", "Rewritten");
    assert_eq!(
        ancestry(&git, &base).unwrap(),
        [Reason::NotDescendant {
            base: base.head.clone(),
            head
        }]
    );
}

#[test]
fn a_new_tag_a_moved_branch_and_a_stash_entry_each_fail_refs() {
    // Commits on the claim branch are what the item is for.
    let repo = repository();
    let dir = repo.path();
    agent(dir, &["branch", "other"]);
    let base = capture(dir);
    commit(dir, "a.txt", "Work");
    assert_eq!(refs(&Git::new(dir), &base).unwrap(), []);

    let fresh = || {
        let repo = repository();
        agent(repo.path(), &["branch", "other"]);
        let base = capture(repo.path());
        (repo, base)
    };

    let (repo, base) = fresh();
    agent(repo.path(), &["tag", "v1"]);
    assert_eq!(
        refs(&Git::new(repo.path()), &base).unwrap(),
        [Reason::RefMoved {
            name: "refs/tags/v1".to_owned(),
            before: None,
            after: Some(base.head.clone()),
        }]
    );

    let (repo, base) = fresh();
    let dir = repo.path();
    agent(dir, &["checkout", "--quiet", "other"]);
    let moved = commit(dir, "a.txt", "Elsewhere");
    agent(dir, &["checkout", "--quiet", "main"]);
    assert_eq!(
        refs(&Git::new(dir), &base).unwrap(),
        [Reason::RefMoved {
            name: "refs/heads/other".to_owned(),
            before: Some(base.head.clone()),
            after: Some(moved),
        }]
    );

    let (repo, base) = fresh();
    let dir = repo.path();
    std::fs::write(dir.join("a.txt"), "stashed").unwrap();
    agent(dir, &["stash", "push", "--quiet"]);
    let found = refs(&Git::new(dir), &base).unwrap();
    assert!(
        matches!(&found[..], [Reason::RefMoved { name, .. }, Reason::StashChanged { before, after }]
            if name == "refs/stash" && before.is_empty() && after.len() == 1),
        "{found:?}"
    );
}

#[test]
fn leaving_the_claim_branch_fails_refs() {
    let repo = repository();
    let dir = repo.path();
    let base = capture(dir);
    agent(dir, &["checkout", "--quiet", "-b", "side"]);
    let found = refs(&Git::new(dir), &base).unwrap();
    assert_eq!(
        found,
        [
            Reason::BranchChanged {
                expected: Some("refs/heads/main".to_owned()),
                found: Some("refs/heads/side".to_owned()),
            },
            Reason::RefMoved {
                name: "refs/heads/side".to_owned(),
                before: None,
                after: Some(base.head.clone()),
            },
        ]
    );
}

#[test]
fn closing_keywords_skip_ci_and_forged_trailers_each_fail_messages() {
    let expected = Expected {
        item: ["owner/repo#53".to_owned()].into(),
        ..Expected::default()
    };
    // A message before the base is not the item's.
    let repo = repository();
    let dir = repo.path();
    commit(dir, "a.txt", "Old work\n\nCloses #9");
    let base = capture(dir);
    commit(dir, "a.txt", "Work\n\nItem: owner/repo#53");
    assert_eq!(messages(&Git::new(dir), &base, &expected).unwrap(), []);

    for message in [
        "Work\n\nCloses #1",
        "Work\n\ncloses #1",
        "Work [SKIP CI]",
        "Work [skip ci]",
        "Work\n\nItem: other/repo#1",
        "Work\n\nITEM: other/repo#1",
    ] {
        let repo = repository();
        let dir = repo.path();
        let base = capture(dir);
        commit(dir, "a.txt", "Clean work");
        let sha = commit(dir, "a.txt", message);
        let found = messages(&Git::new(dir), &base, &expected).unwrap();
        assert_eq!(found.len(), 1, "{message:?}: {found:?}");
        let flagged = match &found[0] {
            Reason::ClosingKeyword { sha, .. }
            | Reason::SkipCi { sha, .. }
            | Reason::ForgedTrailer { sha, .. } => sha,
            other => panic!("{message:?}: {other:?}"),
        };
        assert_eq!(flagged, &sha, "{message:?}");
    }
}

#[test]
fn a_clean_item_passes_every_precondition() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    let base = capture(dir);
    commit(dir, "a.txt", "Work\n\nItem: owner/repo#53\nRun: r1");
    let expected = Expected {
        item: ["owner/repo#53".to_owned()].into(),
        run: ["r1".to_owned()].into(),
        ..Expected::default()
    };
    assert_eq!(check(&git, &base, &expected).unwrap(), []);

    std::fs::write(dir.join("left.txt"), "behind").unwrap();
    agent(dir, &["tag", "v1"]);
    let found = check(&git, &base, &Expected::default()).unwrap();
    let names: Vec<_> = found
        .iter()
        .map(|r| match r {
            Reason::Dirty { .. } => "dirty",
            Reason::RefMoved { .. } => "ref",
            Reason::ForgedTrailer { .. } => "trailer",
            _ => "other",
        })
        .collect();
    assert_eq!(names, ["dirty", "ref", "trailer", "trailer"], "{found:?}");
}

/// A hook script that writes `marker` when it runs.
fn plant(script: &Path, marker: &Path) {
    // Forward slashes: Git for Windows runs hooks through its `sh`.
    let marker = marker.to_string_lossy().replace('\\', "/");
    std::fs::write(script, format!("#!/bin/sh\necho ran > '{marker}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn markers(markers: &[&PathBuf]) -> Vec<String> {
    markers
        .iter()
        .filter(|m| m.exists())
        .map(|m| m.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn planted_hooks_never_run_under_the_invoker() {
    let repo = repository();
    let dir = repo.path();
    // Outside the repository, so that neither the scripts nor the markers dirty the tree.
    let outside = tempfile::tempdir().unwrap();
    let checkout_marker = outside.path().join("post-checkout.ran");
    let fsmonitor_marker = outside.path().join("fsmonitor.ran");
    plant(
        &dir.join(".git").join("hooks").join("post-checkout"),
        &checkout_marker,
    );
    let fsmonitor = outside.path().join("fsmonitor.sh");
    plant(&fsmonitor, &fsmonitor_marker);
    agent(
        dir,
        &[
            "config",
            "core.fsmonitor",
            &fsmonitor.to_string_lossy().replace('\\', "/"),
        ],
    );
    let all = [&checkout_marker, &fsmonitor_marker];

    // The planted hooks are live: plain git runs both.
    agent(dir, &["update-index", "--fsmonitor"]);
    agent(dir, &["status", "--porcelain"]);
    agent(dir, &["checkout", "--quiet", "-b", "probe"]);
    agent(dir, &["checkout", "--quiet", "main"]);
    assert_eq!(markers(&all), ["post-checkout.ran", "fsmonitor.ran"]);
    for marker in all {
        std::fs::remove_file(marker).unwrap();
    }

    // Not one of them runs under the invoker, whatever it is asked to do.
    let git = Git::new(dir);
    let base = Base::capture(&git).unwrap();
    git.run(&["status", "--porcelain"], None).unwrap();
    git.run(&["checkout", "--quiet", "-b", "other"], None)
        .unwrap();
    git.run(&["checkout", "--quiet", "main"], None).unwrap();
    git.run(&["checkout", "--quiet", "--", "a.txt"], None)
        .unwrap();
    git.optional(&["symbolic-ref", "--quiet", "HEAD"]).unwrap();
    check(&git, &base, &Expected::default()).unwrap();
    assert_eq!(markers(&all), Vec::<String>::new());
}
