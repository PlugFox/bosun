//! The commit reader against real repositories built in temporary directories, and the link
//! query of `bosun-store` over what it reads (#23). Sessions are synthetic rows.

use std::path::Path;
use std::process::Command;

use bosun_core::{PathStyle, canonical_path};
use bosun_git::NO_HOOKS;
use bosun_store::{CommitLink, Options, Reader, Repo, Store, Write};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use tempfile::TempDir;

use super::*;

/// Runs `git args` in `dir` with no user or system configuration, no hooks and no signing, the
/// commit dates set to `at` (Unix seconds) when given. Returns the trimmed output.
fn git_at(dir: &Path, at: Option<i64>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", &format!("core.hooksPath={NO_HOOKS}")])
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "user.name=Test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(at) = at {
        let date = format!("@{at} +0000");
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let out = command.output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, None, args)
}

fn repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
    dir
}

/// Writes `files`, commits everything at `at` with `message`, and returns the sha.
fn commit(dir: &Path, at: i64, files: &[(&str, &[u8])], message: &str) -> String {
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    git(dir, &["add", "--all"]);
    git_at(
        dir,
        Some(at),
        &["commit", "--quiet", "--allow-empty", "-m", message],
    );
    git(dir, &["rev-parse", "HEAD"])
}

fn shas(scan: &Scan) -> Vec<&str> {
    scan.commits.iter().map(|c| c.sha.as_str()).collect()
}

#[test]
fn commits_are_read_with_their_counts_and_trailers() {
    let repo = repository();
    let dir = repo.path();
    let first = commit(dir, 1_700_000_000, &[("a.txt", b"1\n2\n3\n")], "First");
    let second = commit(
        dir,
        1_700_000_100,
        &[("a.txt", b"1\nX\n3\nY\n"), ("blob.bin", b"\0\x01\x02\0")],
        "Second\n\nBody.\n\nItem: #7\nSession: one\nSession: two\n",
    );

    let scan = scan(dir, &Cursor::new()).unwrap();
    assert_eq!(shas(&scan), [second.as_str(), first.as_str()]);
    let [newest, oldest] = &scan.commits[..] else {
        unreachable!()
    };
    assert_eq!(oldest.ts_ms, 1_700_000_000_000);
    assert_eq!(oldest.author, "Test <test@example.invalid>");
    assert_eq!((oldest.lines_added, oldest.lines_removed), (3, 0));
    assert!(oldest.trailers.is_empty());
    assert_eq!(newest.ts_ms, 1_700_000_100_000);
    // The binary file counts no lines.
    assert_eq!((newest.lines_added, newest.lines_removed), (2, 1));
    assert_eq!(
        newest.trailers,
        BTreeMap::from([
            ("Item".to_owned(), vec!["#7".to_owned()]),
            (
                "Session".to_owned(),
                vec!["one".to_owned(), "two".to_owned()]
            ),
        ])
    );
    assert_eq!(scan.cursor, Cursor::from_shas([second]));
}

#[test]
fn an_empty_repository_has_no_commits() {
    let repo = repository();
    let scan = scan(repo.path(), &Cursor::new()).unwrap();
    assert_eq!(shas(&scan), Vec::<&str>::new());
    assert_eq!(scan.cursor, Cursor::new());
    assert_eq!(scan.default_branch, None);
}

#[test]
fn a_directory_that_is_not_a_repository_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = scan(dir.path(), &Cursor::new()).unwrap_err();
    assert!(matches!(err, Error::Git { .. }), "{err}");
}

#[test]
fn the_origin_and_the_default_branch() {
    let repo = repository();
    let dir = repo.path();
    commit(dir, 1_700_000_000, &[("a.txt", b"a\n")], "First");
    let found = scan(dir, &Cursor::new()).unwrap();
    assert_eq!(found.remote, None);
    assert_eq!(found.default_branch.as_deref(), Some("main"));

    git(
        dir,
        &[
            "remote",
            "add",
            "origin",
            "https://someone:secret@example.invalid/r.git",
        ],
    );
    git(
        dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    let found = scan(dir, &Cursor::new()).unwrap();
    assert_eq!(
        found.remote.as_deref(),
        Some("https://example.invalid/r.git")
    );
    assert_eq!(found.default_branch.as_deref(), Some("trunk"));
}

#[test]
fn credentials_are_dropped_from_urls() {
    assert_eq!(
        scrub("https://u:p@example.invalid/r.git"),
        "https://example.invalid/r.git"
    );
    assert_eq!(
        scrub("https://example.invalid/a@b.git"),
        "https://example.invalid/a@b.git"
    );
    assert_eq!(
        scrub("git@example.invalid:o/r.git"),
        "git@example.invalid:o/r.git"
    );
}

#[test]
fn remote_tracking_branches_are_read() {
    let repo = repository();
    let dir = repo.path();
    commit(dir, 1_700_000_000, &[("a.txt", b"a\n")], "First");
    git(dir, &["checkout", "--quiet", "-b", "feature"]);
    let pushed = commit(dir, 1_700_000_100, &[("b.txt", b"b\n")], "Pushed");
    git(dir, &["update-ref", "refs/remotes/origin/feature", "HEAD"]);
    git(dir, &["checkout", "--quiet", "main"]);
    git(dir, &["branch", "--quiet", "-D", "feature"]);
    let found = scan(dir, &Cursor::new()).unwrap();
    assert!(found.commits.iter().any(|c| c.sha == pushed));
    assert_eq!(found.commits.len(), 2);
}

async fn open_store(dir: &TempDir) -> Store {
    Store::open(dir.path().join("bosun.db"), Options::default())
        .await
        .unwrap()
}

fn key(dir: &Path) -> String {
    canonical_path(dir.to_str().unwrap(), PathStyle::host())
}

/// Stores what `scan` found in `dir`, as the daemon will.
async fn store_scan(store: &Store, dir: &Path, scan: Scan) {
    let repo = key(dir);
    let mut batch = vec![Write::Repo(Repo {
        name: None,
        path: repo.clone(),
        remote: scan.remote,
        default_branch: scan.default_branch,
    })];
    batch.extend(scan.commits.into_iter().map(|commit| Write::Commit {
        repo: repo.clone(),
        commit,
    }));
    store.writer().write(batch).await.unwrap();
}

#[tokio::test]
async fn reading_is_incremental_and_survives_a_rewrite() {
    let db = tempfile::tempdir().unwrap();
    let store = open_store(&db).await;
    let repo = repository();
    let dir = repo.path();
    let a = commit(dir, 1_700_000_000, &[("a.txt", b"a\n")], "A");
    let b = commit(dir, 1_700_000_100, &[("a.txt", b"b\n")], "B");

    let first = scan(dir, &Cursor::new()).unwrap();
    assert_eq!(shas(&first), [b.as_str(), a.as_str()]);
    let cursor = first.cursor.clone();
    store_scan(&store, dir, first).await;
    assert_eq!(shas(&scan(dir, &cursor).unwrap()), Vec::<&str>::new());

    let c = commit(dir, 1_700_000_200, &[("a.txt", b"c\n")], "C");
    let next = scan(dir, &cursor).unwrap();
    assert_eq!(shas(&next), [c.as_str()]);
    let cursor = next.cursor.clone();
    store_scan(&store, dir, next).await;

    // Rewrite C: only the rewritten commit is new.
    let amend = |at: i64, message: &str| {
        git_at(
            dir,
            Some(at),
            &["commit", "--quiet", "--amend", "-m", message],
        );
        git(dir, &["rev-parse", "HEAD"])
    };
    let c2 = amend(1_700_000_300, "C, rewritten");
    let after = scan(dir, &cursor).unwrap();
    assert_eq!(shas(&after), [c2.as_str()]);
    let cursor = after.cursor.clone();
    store_scan(&store, dir, after).await;

    // Rewrite it again and prune both earlier versions: the cursor now names a commit that no
    // longer exists. The scan does not fail; the history the lost commit covered is read
    // again, and storing it again changes nothing.
    let c3 = amend(1_700_000_400, "C, rewritten twice");
    git(dir, &["reflog", "expire", "--expire=now", "--all"]);
    git(dir, &["gc", "--quiet", "--prune=now"]);
    let pruned = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "-e", &c2])
        .output()
        .unwrap();
    assert!(!pruned.status.success(), "C2 is still in the repository");
    let lost = scan(dir, &cursor).unwrap();
    assert_eq!(shas(&lost), [c3.as_str(), b.as_str(), a.as_str()]);
    // Resumed from the stored commits, as after a restart, the pruned ones among them, only
    // the new commit is read.
    let stored = store.reader().commit_shas(&key(dir)).await.unwrap();
    let resumed = scan(dir, &Cursor::from_shas(stored)).unwrap();
    assert_eq!(shas(&resumed), [c3.as_str()]);
    store_scan(&store, dir, lost).await;

    // The commits the rewrites replaced stay stored.
    let mut expected = vec![a, b, c, c2, c3];
    expected.sort();
    assert_eq!(
        store.reader().commit_shas(&key(dir)).await.unwrap(),
        expected
    );
    store.close().await.unwrap();
}

/// Inserts a synthetic session whose first working directory is `cwd`, spanning
/// `[started, ended]` seconds. Sessions are written by transcript ingest (#21), so the test
/// writes the row itself, on a connection of its own.
async fn session(db: &TempDir, key: &str, cwd: &Path, started: i64, ended: i64) {
    let options = SqliteConnectOptions::new().filename(db.path().join("bosun.db"));
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "INSERT INTO session (provider, session_key, first_cwd, started_ms, ended_ms)
         VALUES ('claude', ?, ?, ?, ?)",
    )
    .bind(key)
    .bind(cwd.to_str().unwrap())
    .bind(started * 1000)
    .bind(ended * 1000)
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

/// Commit sha to the keys of its linked sessions.
async fn links(reader: &Reader) -> BTreeMap<String, Vec<String>> {
    reader
        .commit_links(0, i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(
            |CommitLink {
                 commit, sessions, ..
             }| {
                (
                    commit.sha,
                    sessions.into_iter().map(|s| s.session_key).collect(),
                )
            },
        )
        .collect()
}

const T: i64 = 1_700_000_000;

#[tokio::test]
async fn commits_inside_session_spans_link_to_them() {
    let db = tempfile::tempdir().unwrap();
    let store = open_store(&db).await;
    let repo = repository();
    let dir = repo.path();
    let early = commit(dir, T + 100, &[("a.txt", b"1\n")], "In the first session");
    let middle = commit(dir, T + 200, &[("a.txt", b"2\n")], "In the second session");
    let outside = commit(dir, T + 500, &[("a.txt", b"3\n")], "In no session");
    store_scan(&store, dir, scan(dir, &Cursor::new()).unwrap()).await;
    session(&db, "first", dir, T + 50, T + 150).await;
    session(&db, "second", &dir.join("src"), T + 190, T + 300).await;

    let found = links(store.reader()).await;
    assert_eq!(found[&early], ["first"]);
    assert_eq!(found[&middle], ["second"]);
    assert_eq!(
        found[&outside],
        Vec::<String>::new(),
        "linked outside every span"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn each_commit_links_only_within_its_repository() {
    let db = tempfile::tempdir().unwrap();
    let store = open_store(&db).await;
    let (one, two) = (repository(), repository());
    let in_one = commit(one.path(), T + 100, &[("a.txt", b"1\n")], "In one");
    let in_two = commit(two.path(), T + 120, &[("b.txt", b"2\n")], "In two");
    for repo in [&one, &two] {
        let found = scan(repo.path(), &Cursor::new()).unwrap();
        store_scan(&store, repo.path(), found).await;
    }
    // The two spans overlap, and both commits are inside both of them.
    session(&db, "session-one", one.path(), T + 50, T + 200).await;
    session(&db, "session-two", two.path(), T + 60, T + 250).await;

    let found = links(store.reader()).await;
    assert_eq!(found[&in_one], ["session-one"]);
    assert_eq!(found[&in_two], ["session-two"]);
    store.close().await.unwrap();
}

#[tokio::test]
async fn refresh_stores_new_commits_once() {
    let db = tempfile::tempdir().unwrap();
    let store = open_store(&db).await;
    let repo = repository();
    let dir = repo.path();
    let a = commit(
        dir,
        1_700_000_000,
        &[(
            "a.txt", b"a
",
        )],
        "A",
    );
    let b = commit(
        dir,
        1_700_000_100,
        &[(
            "a.txt", b"b
",
        )],
        "B",
    );

    assert_eq!(
        refresh(store.reader(), store.writer(), dir).await.unwrap(),
        2
    );
    let mut stored = vec![a, b];
    stored.sort();
    assert_eq!(store.reader().commit_shas(&key(dir)).await.unwrap(), stored);
    // The stored commits are the cursor: nothing is read twice.
    assert_eq!(
        refresh(store.reader(), store.writer(), dir).await.unwrap(),
        0
    );
    let c = commit(
        dir,
        1_700_000_200,
        &[(
            "a.txt", b"c
",
        )],
        "C",
    );
    assert_eq!(
        refresh(store.reader(), store.writer(), dir).await.unwrap(),
        1
    );
    stored.push(c);
    stored.sort();
    assert_eq!(store.reader().commit_shas(&key(dir)).await.unwrap(), stored);

    // A directory that is not a repository is an error and stores nothing.
    let plain = tempfile::tempdir().unwrap();
    let err = refresh(store.reader(), store.writer(), plain.path())
        .await
        .unwrap_err();
    assert!(matches!(err, RefreshError::Git(_)), "{err}");
    assert_eq!(
        store
            .reader()
            .commit_shas(&key(plain.path()))
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    store.close().await.unwrap();
}
