//! The invoker and the preconditions against throwaway repositories built with the system git
//! in temporary directories (#53).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::*;

/// Runs `git args` in `dir` as an agent would, with no user or system configuration and no
/// signing, but with the repository's own hooks and fsmonitor live. Returns the trimmed
/// output.
fn agent(dir: &Path, args: &[&str]) -> String {
    agent_with(dir, &[], args)
}

/// Removes from `command` every variable it would inherit from a bosun session (#134), and any
/// such variable already set on it: each `BOSUN_*` and the command-scope configuration
/// (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*`, `GIT_CONFIG_VALUE_*`) that points git at the
/// session's hooks. A test's git then behaves the same inside a session and outside one; what a
/// test sets after the call still applies.
pub(crate) fn without_session(command: &mut Command) -> &mut Command {
    let names: Vec<OsString> = std::env::vars_os()
        .map(|(name, _)| name)
        .chain(command.get_envs().map(|(name, _)| name.to_owned()))
        .filter(|name| {
            let name = name.to_string_lossy();
            name.starts_with("BOSUN_")
                || name == "GIT_CONFIG_COUNT"
                || name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
        })
        .collect();
    // Mutant (`mutant_git_test_env_inherited`): the session's variables reach the test's git.
    if !cfg!(mutant_git_test_env_inherited) {
        for name in names {
            command.env_remove(name);
        }
    }
    command
}

/// [`agent`] with the variables `env` added.
fn agent_with(dir: &Path, env: &[(String, String)], args: &[&str]) -> String {
    let out = without_session(&mut Command::new("git"))
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "tag.gpgSign=false"])
        .args(["-c", "user.name=Test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .envs(env.iter().map(|(k, v)| (k, v)))
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

// ---- #59: the ignored-file and configuration snapshots, protected paths, trailers ----

/// The environment of an agent session's git as `bosun-identity` builds it, reduced to what
/// the trailers need: its `credential.helper` reset at entry 0, the injected hooks after it,
/// and the trailer variables, with `checkout` as the item's checkout, when `trailers` is given.
fn session(hooks: &Hooks, checkout: &Path, trailers: Option<&Trailers>) -> Vec<(String, String)> {
    let mut entries = vec![("credential.helper".to_owned(), String::new())];
    entries.extend(hooks.git_config());
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), entries.len().to_string())];
    for (i, (key, value)) in entries.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), value));
    }
    if let Some(trailers) = trailers {
        env.extend(Hooks::env(trailers, &toplevel(checkout)));
    }
    env
}

/// The top level of the repository at `dir` as the daemon passes it ([`Hooks::checkout`]).
fn toplevel(dir: &Path) -> String {
    Hooks::checkout(&Git::new(dir)).unwrap()
}

fn trailers(session: &str) -> Trailers {
    Trailers {
        item: "owner/repo#59".to_owned(),
        run: "run-1".to_owned(),
        session: session.to_owned(),
    }
}

fn expected(trailers: &Trailers) -> Expected {
    Expected {
        item: [trailers.item.clone()].into(),
        run: [trailers.run.clone()].into(),
        session: [trailers.session.clone()].into(),
        co_authored_by: BTreeSet::new(),
    }
}

/// How many lines of the message of HEAD start with each of `Item:`, `Run:`, `Session:`.
fn trailer_counts(dir: &Path) -> [usize; 3] {
    let body = agent(dir, &["log", "-1", "--format=%B"]);
    ["Item: ", "Run: ", "Session: "].map(|key| body.lines().filter(|l| l.starts_with(key)).count())
}

fn sh(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[test]
fn the_injected_hooks_path_overrides_a_repository_local_one() {
    let repo = repository();
    let dir = repo.path();
    let outside = tempfile::tempdir().unwrap();
    let local_marker = outside.path().join("local.ran");
    let injected_marker = outside.path().join("injected.ran");
    std::fs::create_dir(dir.join(".githooks")).unwrap();
    plant(&dir.join(".githooks").join("pre-commit"), &local_marker);
    agent(dir, &["config", "core.hooksPath", ".githooks"]);
    let injected = outside.path().join("hooks");
    std::fs::create_dir(&injected).unwrap();
    plant(&injected.join("pre-commit"), &injected_marker);
    let env = vec![
        ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
        ("GIT_CONFIG_KEY_0".to_owned(), "core.hooksPath".to_owned()),
        ("GIT_CONFIG_VALUE_0".to_owned(), sh(&injected)),
    ];

    assert_eq!(
        agent_with(dir, &env, &["config", "--get", "core.hooksPath"]),
        sh(&injected)
    );
    agent_with(
        dir,
        &env,
        &["commit", "--quiet", "--allow-empty", "-m", "Probe"],
    );
    assert_eq!(
        markers(&[&local_marker, &injected_marker]),
        ["injected.ran"]
    );
    // The control: without the environment the repository's own hook runs.
    std::fs::remove_file(&injected_marker).unwrap();
    agent(dir, &["commit", "--quiet", "--allow-empty", "-m", "Probe"]);
    assert_eq!(markers(&[&local_marker, &injected_marker]), ["local.ran"]);
}

#[test]
fn a_hidden_file_behind_a_new_gitignore_line_is_refused_but_not_in_a_build_dir() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    std::fs::write(dir.join(".gitignore"), "old.log\n").unwrap();
    commit(dir, "b.txt", "Ignore old logs");
    std::fs::write(dir.join("old.log"), "already there").unwrap();
    let build: BTreeSet<String> = ["target".to_owned()].into();
    let before = Ignored::capture(&git).unwrap();
    assert_eq!(before.paths, ["old.log".to_owned()].into());

    // The session: a `.gitignore` line that hides the file making the verdict pass, and
    // build output in the declared build directory.
    std::fs::write(dir.join(".gitignore"), "old.log\nfixture.dat\ntarget/\n").unwrap();
    commit(dir, "c.txt", "Ignore more");
    std::fs::write(dir.join("fixture.dat"), "makes the verdict pass").unwrap();
    std::fs::create_dir_all(dir.join("target").join("debug")).unwrap();
    std::fs::write(dir.join("target").join("debug").join("out.o"), "build").unwrap();
    std::fs::write(dir.join("old.log"), "rewritten, still the old file").unwrap();

    assert_eq!(
        ignored(&git, &before, &build).unwrap(),
        [Refusal::NewIgnored {
            paths: vec!["fixture.dat".to_owned()],
            build_names: vec!["target".to_owned()],
        }]
    );
    assert!(dir.join("fixture.dat").exists(), "nothing is deleted");

    // The same file inside the build directory is expected.
    std::fs::rename(
        dir.join("fixture.dat"),
        dir.join("target").join("fixture.dat"),
    )
    .unwrap();
    assert_eq!(ignored(&git, &before, &build).unwrap(), []);
    // A new file in a directory that was ignored already counts too.
    std::fs::create_dir(dir.join("cache")).unwrap();
    std::fs::write(dir.join(".gitignore"), "old.log\ntarget/\ncache/\n").unwrap();
    let before = Ignored::capture(&git).unwrap();
    std::fs::write(dir.join("cache").join("x.bin"), "x").unwrap();
    assert_eq!(
        ignored(&git, &before, &build).unwrap(),
        [Refusal::NewIgnored {
            paths: vec!["cache/x.bin".to_owned()],
            build_names: vec!["target".to_owned()],
        }]
    );
}

/// The changed files of `refusals`, each as its last two path components.
fn changed(refusals: &[Refusal]) -> Vec<String> {
    refusals
        .iter()
        .map(|r| match r {
            Refusal::ConfigChanged { path, .. } => {
                let path = sh(path);
                let parts: Vec<&str> = path.rsplit('/').take(2).collect();
                format!("{}/{}", parts[1], parts[0])
            }
            other => format!("{other:?}"),
        })
        .collect()
}

/// One edit of git's configuration and the file it changes.
type Step<'a> = (&'a str, Box<dyn Fn() + 'a>);

#[test]
fn a_planted_hook_an_include_edit_and_an_exclude_edit_are_each_refused() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    let outside = tempfile::tempdir().unwrap();
    // An include target next to the configuration, one that does not exist yet, and a
    // repository-local hooks directory.
    std::fs::write(
        dir.join(".git").join("extra.inc"),
        "[alias]\n\tx = status\n",
    )
    .unwrap();
    agent(dir, &["config", "--add", "include.path", "extra.inc"]);
    agent(dir, &["config", "--add", "include.path", "later.inc"]);
    std::fs::create_dir(dir.join(".githooks")).unwrap();
    agent(dir, &["config", "core.hooksPath", ".githooks"]);
    let marker = outside.path().join("ran");

    let snapshot = GitConfig::capture(&git).unwrap();
    // Capturing and comparing do not themselves change anything.
    Base::capture(&git).unwrap();
    Ignored::capture(&git).unwrap();
    assert_eq!(config(&git, &snapshot).unwrap(), []);

    let info = dir.join(".git").join("info");
    let steps: [Step; 7] = [
        (
            "hooks/pre-commit",
            Box::new(|| plant(&dir.join(".git").join("hooks").join("pre-commit"), &marker)),
        ),
        (
            ".githooks/post-commit",
            Box::new(|| plant(&dir.join(".githooks").join("post-commit"), &marker)),
        ),
        (
            ".git/extra.inc",
            Box::new(|| {
                std::fs::write(
                    dir.join(".git").join("extra.inc"),
                    "[core]\n\tbare = false\n",
                )
                .unwrap();
            }),
        ),
        (
            ".git/later.inc",
            Box::new(|| std::fs::write(dir.join(".git").join("later.inc"), "").unwrap()),
        ),
        (
            "info/exclude",
            Box::new(|| std::fs::write(info.join("exclude"), "fixture.dat\n").unwrap()),
        ),
        (
            "info/attributes",
            Box::new(|| std::fs::write(info.join("attributes"), "* -diff\n").unwrap()),
        ),
        (
            ".git/config.worktree",
            Box::new(|| std::fs::write(dir.join(".git").join("config.worktree"), "").unwrap()),
        ),
    ];
    for (name, step) in steps {
        let snapshot = GitConfig::capture(&git).unwrap();
        step();
        assert_eq!(changed(&config(&git, &snapshot).unwrap()), [name], "{name}");
    }
    // An edit of `.git/config` itself.
    let snapshot = GitConfig::capture(&git).unwrap();
    agent(dir, &["config", "core.fsmonitor", "false"]);
    assert_eq!(changed(&config(&git, &snapshot).unwrap()), [".git/config"]);
}

#[test]
fn each_configuration_file_is_one_entry_whatever_its_spelling() {
    let repo = repository();
    let dir = repo.path();
    std::fs::create_dir(dir.join("sub")).unwrap();
    // The same file under three spellings: built from git's directories, an origin git prints,
    // and an include target written through `..`.
    std::fs::write(dir.join(".git").join("extra.inc"), "").unwrap();
    agent(dir, &["config", "include.path", "../.git/extra.inc"]);
    let git = Git::new(dir.join("sub").join(".."));
    let snapshot = GitConfig::capture(&git).unwrap();
    for name in ["config", "extra.inc"] {
        let count = snapshot
            .files
            .keys()
            .filter(|p| p.file_name().is_some_and(|n| n == name))
            .count();
        assert_eq!(count, 1, "{name}: {:?}", snapshot.files.keys());
    }

    // Spellings of one path, existing or not, are one canonical path.
    let a = crate::snapshot::canonical(&dir.join("sub").join("..").join("missing.inc"));
    let b = crate::snapshot::canonical(&dir.join("missing.inc"));
    assert_eq!(a, b);
    let a = crate::snapshot::canonical(&dir.join(".").join(".git").join("config"));
    let b = crate::snapshot::canonical(&dir.join(".git").join("config"));
    assert_eq!(a, b);

    // An edit of the file is reported once.
    agent(dir, &["config", "core.fsmonitor", "false"]);
    assert_eq!(changed(&config(&git, &snapshot).unwrap()), [".git/config"]);
}

#[test]
fn a_commit_touching_the_verdicts_test_is_refused_with_the_diff_stat() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    std::fs::create_dir(dir.join("tests")).unwrap();
    commit(dir, "tests/verdict.rs", "Add the verdict's test");
    let base = Base::capture(&git).unwrap();
    let protects = |path: &str| path.starts_with("tests/") || path.ends_with(".gitignore");

    commit(dir, "src.rs", "Change the code");
    assert_eq!(protected(&git, &base, protects, &[]).unwrap(), []);

    commit(dir, "tests/verdict.rs", "Weaken the test");
    let refusals = protected(&git, &base, protects, &[]).unwrap();
    let [Refusal::Protected { paths, stat }] = refusals.as_slice() else {
        panic!("{refusals:?}");
    };
    assert_eq!(paths, &["tests/verdict.rs"]);
    assert!(stat.contains("tests/verdict.rs"), "{stat}");
    assert!(stat.contains("1 file changed"), "{stat}");
    assert!(!stat.contains("src.rs"), "{stat}");

    // Listed as allowed in the snapshot, it is not refused; other entries do not cover it.
    let allowed = ["tests/verdict.rs".to_owned()];
    assert_eq!(protected(&git, &base, protects, &allowed).unwrap(), []);
    let allowed = ["tests/verdict.rs.bak".to_owned()];
    assert_eq!(protected(&git, &base, protects, &allowed).unwrap().len(), 1);

    // Changed and changed back is still an edit; the stat shows the commits.
    let base = Base::capture(&git).unwrap();
    std::fs::write(dir.join(".gitignore"), "x\n").unwrap();
    agent(dir, &["add", "--all"]);
    agent(dir, &["commit", "--quiet", "-m", "Ignore x"]);
    agent(dir, &["rm", "--quiet", ".gitignore"]);
    agent(dir, &["commit", "--quiet", "-m", "Unignore x"]);
    let refusals = protected(&git, &base, protects, &[]).unwrap();
    let [Refusal::Protected { paths, stat }] = refusals.as_slice() else {
        panic!("{refusals:?}");
    };
    assert_eq!(paths, &[".gitignore"]);
    assert!(
        stat.contains("Ignore x") && stat.contains("Unignore x"),
        "{stat}"
    );
}

/// Plants the repository's own `pre-commit` and `prepare-commit-msg` hooks, writing markers
/// into `outside`: in `.git/hooks` or, with `hooks_path`, in a directory `core.hooksPath`
/// names.
fn with_own_hooks(dir: &Path, outside: &Path, hooks_path: bool) -> [PathBuf; 2] {
    let hooks = if hooks_path {
        agent(dir, &["config", "core.hooksPath", ".githooks"]);
        dir.join(".githooks")
    } else {
        dir.join(".git").join("hooks")
    };
    std::fs::create_dir_all(&hooks).unwrap();
    let markers = [
        outside.join("pre-commit.ran"),
        outside.join("prepare-commit-msg.ran"),
    ];
    plant(&hooks.join("pre-commit"), &markers[0]);
    plant(&hooks.join("prepare-commit-msg"), &markers[1]);
    markers
}

#[test]
fn an_agent_commit_carries_the_three_trailers_once_also_after_amend() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    let outside = tempfile::tempdir().unwrap();
    let own = with_own_hooks(dir, outside.path(), false);
    let hooks = Hooks::install(outside.path().join("bosun-hooks")).unwrap();
    let values = trailers("session-1");
    let env = session(&hooks, dir, Some(&values));
    let base = Base::capture(&git).unwrap();

    std::fs::write(dir.join("a.txt"), "changed").unwrap();
    agent_with(dir, &env, &["commit", "--quiet", "--all", "-m", "Change a"]);
    assert_eq!(trailer_counts(dir), [1, 1, 1]);
    let body = agent(dir, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Item: owner/repo#59\nRun: run-1\nSession: session-1"),
        "{body}"
    );
    // The repository's own hooks ran, chained.
    assert_eq!(
        markers(&[&own[0], &own[1]]),
        ["pre-commit.ran", "prepare-commit-msg.ran"]
    );

    agent_with(dir, &env, &["commit", "--quiet", "--amend", "--no-edit"]);
    assert_eq!(trailer_counts(dir), [1, 1, 1]);
    agent_with(
        dir,
        &env,
        &["commit", "--quiet", "--amend", "-m", "Change a\n\nMore."],
    );
    assert_eq!(trailer_counts(dir), [1, 1, 1]);
    // `--no-verify` skips pre-commit but not the trailers.
    agent_with(
        dir,
        &env,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "-m",
            "Empty",
        ],
    );
    assert_eq!(trailer_counts(dir), [1, 1, 1]);

    let expected = expected(&values);
    assert_eq!(messages(&git, &base, &expected).unwrap(), []);
    assert_eq!(missing(&git, &base, &expected).unwrap(), []);
}

#[test]
fn a_commit_without_the_injection_carries_none_and_its_hooks_run_as_before() {
    for hooks_path in [false, true] {
        let repo = repository();
        let dir = repo.path();
        let git = Git::new(dir);
        let outside = tempfile::tempdir().unwrap();
        let own = with_own_hooks(dir, outside.path(), hooks_path);
        let hooks = Hooks::install(outside.path().join("bosun-hooks")).unwrap();
        let base = Base::capture(&git).unwrap();

        // The user's own commit: no injection at all.
        agent(
            dir,
            &["commit", "--quiet", "--allow-empty", "-m", "User commit"],
        );
        assert_eq!(trailer_counts(dir), [0, 0, 0]);
        assert_eq!(
            markers(&[&own[0], &own[1]]),
            ["pre-commit.ran", "prepare-commit-msg.ran"]
        );
        for marker in &own {
            std::fs::remove_file(marker).unwrap();
        }

        // The injected hooks without the marker (the values left over in a shell, say): they
        // only chain.
        let mut env = session(&hooks, dir, None);
        env.extend(
            Hooks::env(&trailers("session-1"), &toplevel(dir))
                .into_iter()
                .filter(|(k, _)| k != MARKER_VAR),
        );
        agent_with(
            dir,
            &env,
            &["commit", "--quiet", "--allow-empty", "-m", "Also user"],
        );
        assert_eq!(trailer_counts(dir), [0, 0, 0], "hooks_path {hooks_path}");
        assert_eq!(
            markers(&[&own[0], &own[1]]),
            ["pre-commit.ran", "prepare-commit-msg.ran"]
        );

        // Both commits lack the trailers: a report line, not a refusal.
        let found = missing(&git, &base, &expected(&trailers("session-1"))).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|m| m.keys == ["Item", "Run", "Session"]));
    }
}

#[test]
fn a_commit_with_a_wrong_session_is_refused_as_forged() {
    let repo = repository();
    let dir = repo.path();
    let git = Git::new(dir);
    let outside = tempfile::tempdir().unwrap();
    let hooks = Hooks::install(outside.path().join("bosun-hooks")).unwrap();
    let base = Base::capture(&git).unwrap();
    let env = session(&hooks, dir, Some(&trailers("someone-elses-session")));
    agent_with(
        dir,
        &env,
        &["commit", "--quiet", "--allow-empty", "-m", "Forged"],
    );

    let sha = agent(dir, &["rev-parse", "HEAD"]);
    let right = expected(&trailers("session-1"));
    assert_eq!(
        messages(&git, &base, &right).unwrap(),
        [Reason::ForgedTrailer {
            sha: sha.clone(),
            key: "Session".to_owned(),
            value: "someone-elses-session".to_owned(),
        }]
    );
    assert_eq!(
        missing(&git, &base, &right).unwrap(),
        [Missing {
            sha,
            keys: vec!["Session"]
        }]
    );
}

#[test]
fn a_commit_in_another_repository_carries_no_trailer() {
    let a = repository();
    let b = repository();
    let outside = tempfile::tempdir().unwrap();
    let hooks = Hooks::install(outside.path().join("bosun-hooks")).unwrap();
    // The environment a session gets, with repository A as the item's checkout.
    let env = session(&hooks, a.path(), Some(&trailers("session-1")));
    assert!(
        env.iter()
            .any(|(k, v)| k == CHECKOUT_VAR && *v == toplevel(a.path())),
        "{env:?}"
    );

    agent_with(
        a.path(),
        &env,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "In the checkout",
        ],
    );
    assert_eq!(trailer_counts(a.path()), [1, 1, 1]);
    agent_with(
        b.path(),
        &env,
        &["commit", "--quiet", "--allow-empty", "-m", "Elsewhere"],
    );
    assert_eq!(trailer_counts(b.path()), [0, 0, 0]);

    // An empty checkout matches nothing, A included.
    let mut empty = env.clone();
    for (k, v) in &mut empty {
        if k == CHECKOUT_VAR {
            v.clear();
        }
    }
    agent_with(
        a.path(),
        &empty,
        &["commit", "--quiet", "--allow-empty", "-m", "No checkout"],
    );
    assert_eq!(trailer_counts(a.path()), [0, 0, 0]);
}

#[test]
fn the_test_git_drops_the_session_variables() {
    let session = [
        "BOSUN_ITEM",
        "BOSUN_TRAILERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ];
    let mut command = Command::new("git");
    for name in session {
        command.env(name, "1");
    }
    command.env("UNRELATED_TO_THE_SESSION", "kept");

    without_session(&mut command);
    let envs: Vec<(String, Option<String>)> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for name in session {
        assert!(
            envs.iter().any(|(k, v)| k == name && v.is_none()),
            "{name} is not removed: {envs:?}"
        );
    }
    assert!(
        envs.iter()
            .any(|(k, v)| k == "UNRELATED_TO_THE_SESSION" && v.as_deref() == Some("kept")),
        "{envs:?}"
    );
}
