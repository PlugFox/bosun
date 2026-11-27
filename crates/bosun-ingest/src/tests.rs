//! Discovery, tailing, rescan and reingest against a real store, over copies of the fixture
//! corpus in temporary directories (#21). Sums are read back with SQL and compared with each
//! case's hand-computed `expected.json`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bosun_core::{Commit, Tokens};
use bosun_store::{Options as StoreOptions, Store, Write};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{Connection, Row};
use tempfile::TempDir;

use crate::testing::{Totals, cases, expected, files, fixtures, roots};
use crate::{Ingest, LineParser, Options, Outcome, Provider, Record, Source, claude, spawn, stub};

mod backfill;

/// A temporary identity: a transcript directory and a database.
struct Bench {
    dir: TempDir,
    store: Store,
}

impl Bench {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("bosun.db"), StoreOptions::default())
            .await
            .unwrap();
        Self { dir, store }
    }

    /// The provider's transcript directory: `<config dir>/projects`.
    fn projects(&self) -> PathBuf {
        self.dir.path().join("claude").join("projects")
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("bosun.db")
    }

    async fn ingest<P: Provider>(&self, provider: P, root: PathBuf, case: &Path) -> Ingest<P> {
        self.ingest_with(provider, root, case, Options::default())
            .await
    }

    async fn ingest_with<P: Provider>(
        &self,
        provider: P,
        root: PathBuf,
        case: &Path,
        options: Options,
    ) -> Ingest<P> {
        let ingest = Ingest::new(provider, root, &self.store, options);
        ingest.register_repos(&roots(case)).await.unwrap();
        ingest
    }

    async fn close(self) {
        self.store.close().await.unwrap();
    }
}

fn case(name: &str) -> PathBuf {
    fixtures().join("claude").join(name)
}

/// Copies the file `relative` of `case/<from>` to `to`, keeping its relative path.
fn copy_file(case: &Path, from: &str, relative: &Path, to: &Path) {
    let target = to.join(relative);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(case.join(from).join(relative), target).unwrap();
}

/// The files of `case/<from>`, relative to it.
fn relative_files(case: &Path, from: &str) -> Vec<PathBuf> {
    let base = case.join(from);
    files(&base)
        .into_iter()
        .map(|path| path.strip_prefix(&base).unwrap().to_owned())
        .collect()
}

fn copy_case(case: &Path, to: &Path) {
    for file in relative_files(case, "projects") {
        copy_file(case, "projects", &file, to);
    }
}

async fn connect(db: &Path) -> SqliteConnection {
    let options = SqliteConnectOptions::new().filename(db).read_only(true);
    SqliteConnection::connect_with(&options).await.unwrap()
}

fn tokens(row: &sqlx::sqlite::SqliteRow, from: usize) -> Tokens {
    let n = |i: usize| u64::try_from(row.get::<i64, _>(from + i)).unwrap();
    Tokens {
        input: n(0),
        cache_write_5m: n(1),
        cache_write_1h: n(2),
        cache_read: n(3),
        output: n(4),
    }
}

/// Message counts and token sums per context window, per repository and in total, by SQL.
async fn stored_totals(db: &Path) -> Totals {
    let mut conn = connect(db).await;
    let rows = sqlx::query(
        "SELECT s.session_key, s.agent_key, r.path, count(*),
                sum(m.input_tokens), sum(m.cache_write_5m_tokens), sum(m.cache_write_1h_tokens),
                sum(m.cache_read_tokens), sum(m.output_tokens)
         FROM message m
         JOIN session s ON s.id = m.session_id
         LEFT JOIN repo r ON r.id = m.repo_id
         GROUP BY m.session_id, m.repo_id",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    let mut totals = Totals::default();
    for row in &rows {
        totals.add_rows(
            (row.get(0), row.get(1)),
            row.get(2),
            u64::try_from(row.get::<i64, _>(3)).unwrap(),
            tokens(row, 4),
        );
    }
    totals.drop_zeros()
}

/// The row count of every ingested table.
async fn row_counts(db: &Path) -> Vec<(&'static str, i64)> {
    let mut conn = connect(db).await;
    let mut out = Vec::new();
    for table in [
        "source_file",
        "session",
        "message",
        "tool_call",
        "file_touch",
        "ingest_gap",
        "session_check",
        "repo",
        "model",
    ] {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&mut conn)
                .await
                .unwrap();
        out.push((table, n));
    }
    conn.close().await.unwrap();
    out
}

async fn scalar(db: &Path, sql: &'static str) -> i64 {
    let mut conn = connect(db).await;
    let n = sqlx::query_scalar(sql).fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    n
}

fn want(case: &Path) -> Totals {
    expected(case).canonical_repos()
}

#[tokio::test]
async fn every_fixture_lands_in_the_store() {
    for case in cases("claude") {
        let bench = Bench::new().await;
        copy_case(&case, &bench.projects());
        let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
        let report = ingest.scan().await.unwrap();
        assert_eq!(report.failed, [], "{}", case.display());
        assert_eq!(
            stored_totals(&bench.db()).await,
            want(&case),
            "{}",
            case.display()
        );
        bench.close().await;
    }
}

#[tokio::test]
async fn a_split_message_over_two_reads_counts_once() {
    let case = case("split-message");
    let bench = Bench::new().await;
    let file = relative_files(&case, "projects").remove(0);
    let bytes = std::fs::read(case.join("projects").join(&file)).unwrap();
    // Cut inside the second record of the split message `M1`: the first read sees one record
    // and half of the next, the second read the rest.
    let first_record = bytes
        .windows(b"\"type\":\"assistant\"".len())
        .position(|w| w == b"\"type\":\"assistant\"")
        .unwrap();
    let second_line = first_record
        + bytes[first_record..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap();
    let cut = second_line + 40;
    let target = bench.projects().join(&file);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, &bytes[..cut]).unwrap();

    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    let partial = scalar(&bench.db(), "SELECT count(*) FROM message").await;
    assert_eq!(partial, 1, "the first record of M1 is stored");

    std::fs::write(&target, &bytes).unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.restarted, 0, "the file grew; it was not re-read");
    assert_eq!(stored_totals(&bench.db()).await, want(&case));
    bench.close().await;
}

#[tokio::test]
async fn the_fork_counts_once_in_either_scan_order() {
    let case = case("fork-continue");
    let files = relative_files(&case, "projects");
    assert_eq!(files.len(), 3);
    for order in [files.clone(), files.into_iter().rev().collect()] {
        let bench = Bench::new().await;
        let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
        // One file at a time, each followed by a scan: the scan order is the file order.
        for file in &order {
            copy_file(&case, "projects", file, &bench.projects());
            ingest.scan().await.unwrap();
        }
        assert_eq!(stored_totals(&bench.db()).await, want(&case), "{order:?}");
        bench.close().await;
    }
}

#[tokio::test]
async fn a_second_pass_parses_nothing() {
    let bench = Bench::new().await;
    let mut cases = Vec::new();
    for name in [
        "fork-continue",
        "subagent-cost-state",
        "report",
        "skipped-records",
    ] {
        let case = case(name);
        copy_case(&case, &bench.projects());
        cases.push(case);
    }
    let ingest = bench
        .ingest(claude::Parser, bench.projects(), &cases[0])
        .await;
    let first = ingest.scan().await.unwrap();
    assert!(first.parsed_bytes > 0);
    let counts = row_counts(&bench.db()).await;
    let totals = stored_totals(&bench.db()).await;

    let second = ingest.scan().await.unwrap();
    assert_eq!(second.parsed_bytes, 0, "the second pass parsed bytes");
    assert_eq!(ingest.parsed_bytes(), first.parsed_bytes);
    assert_eq!(row_counts(&bench.db()).await, counts);
    assert_eq!(stored_totals(&bench.db()).await, totals);

    // A new ingest over the same store resumes from the stored offsets, as after a restart.
    let restarted = bench
        .ingest(claude::Parser, bench.projects(), &cases[0])
        .await;
    assert_eq!(restarted.scan().await.unwrap().parsed_bytes, 0);
    assert_eq!(row_counts(&bench.db()).await, counts);
    bench.close().await;
}

#[tokio::test]
async fn a_partial_last_line_waits_for_its_newline() {
    let case = case("partial-last-line");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let file = bench
        .projects()
        .join(relative_files(&case, "projects").remove(0));
    let size = std::fs::metadata(&file).unwrap().len();
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();

    assert_eq!(stored_totals(&bench.db()).await, want(&case));
    assert_eq!(
        scalar(&bench.db(), "SELECT count(*) FROM ingest_gap").await,
        0
    );
    let offset = scalar(&bench.db(), "SELECT parsed_offset FROM source_file").await;
    let offset = u64::try_from(offset).unwrap();
    let bytes = std::fs::read(&file).unwrap();
    let last_newline = bytes.iter().rposition(|&b| b == b'\n').unwrap() as u64;
    assert_eq!(
        offset,
        last_newline + 1,
        "the offset stops before the torn line"
    );
    assert!(offset < size);
    // Lines are counted up to the offset: the torn line is not one yet (#26).
    let complete = i64::try_from(bytes.split(|&b| b == b'\n').count() - 1).unwrap();
    let lines = "SELECT parsed_lines FROM source_file";
    assert_eq!(scalar(&bench.db(), lines).await, complete);

    // The newline arrives: the third message is stored.
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    std::io::Write::write_all(&mut appended, b"\n").unwrap();
    drop(appended);
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.parsed_bytes, size + 1 - offset);
    assert_eq!(scalar(&bench.db(), "SELECT count(*) FROM message").await, 3);
    assert_eq!(
        scalar(&bench.db(), "SELECT count(*) FROM ingest_gap").await,
        0
    );
    // A resumed read adds its lines; a read from zero counts them again, not twice.
    assert_eq!(scalar(&bench.db(), lines).await, complete + 1);
    ingest.reingest().await.unwrap();
    assert_eq!(scalar(&bench.db(), lines).await, complete + 1);
    bench.close().await;
}

#[tokio::test]
async fn truncate_rewrite_rename_and_delete_keep_the_sums() {
    let case = case("report");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let files: Vec<PathBuf> = relative_files(&case, "projects")
        .into_iter()
        .map(|f| bench.projects().join(f))
        .collect();
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    let want = want(&case);
    assert_eq!(stored_totals(&bench.db()).await, want);
    let original = std::fs::read(&files[0]).unwrap();

    // Truncate to the first half of the lines: the file shrank, it is re-read from zero, and
    // the rows of the cut lines stay.
    let half = original
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(5)
        .unwrap()
        .0
        + 1;
    std::fs::write(&files[0], &original[..half]).unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.restarted, 1, "truncate");
    assert_eq!(stored_totals(&bench.db()).await, want, "truncate");

    // The lines come back: the file grew from the new offset.
    std::fs::write(&files[0], &original).unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.restarted, 0, "regrow");
    assert_eq!(stored_totals(&bench.db()).await, want, "regrow");

    // Rewrite in place, longer, with different bytes before the offset: the hash at the
    // offset no longer matches and the file is re-read from zero.
    let mut rewritten = b"{\"type\":\"summary\",\"summary\":\"Synthetic.\"}\n".to_vec();
    rewritten.extend_from_slice(&original);
    std::fs::write(&files[0], &rewritten).unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.restarted, 1, "rewrite");
    assert_eq!(stored_totals(&bench.db()).await, want, "rewrite");

    // Rename (its project directory): the same file id, so nothing is parsed and the path
    // follows.
    let moved = bench.projects().join("-fixture-renamed");
    std::fs::create_dir_all(&moved).unwrap();
    let renamed = moved.join(files[0].file_name().unwrap());
    std::fs::rename(&files[0], &renamed).unwrap();
    let before = ingest.parsed_bytes();
    let report = ingest.scan().await.unwrap();
    assert_eq!((report.parsed_bytes, report.restarted), (0, 0), "rename");
    assert_eq!(ingest.parsed_bytes(), before);
    let paths = scalar(
        &bench.db(),
        "SELECT count(*) FROM source_file WHERE path LIKE '%-fixture-renamed%'",
    )
    .await;
    assert_eq!(paths, 1, "the stored path follows the rename");
    assert_eq!(stored_totals(&bench.db()).await, want, "rename");

    // Delete both: the rows stay.
    std::fs::remove_file(&renamed).unwrap();
    std::fs::remove_file(&files[1]).unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!((report.files, report.parsed_bytes), (0, 0), "delete");
    assert_eq!(stored_totals(&bench.db()).await, want, "delete");
    assert_eq!(
        scalar(&bench.db(), "SELECT count(*) FROM source_file").await,
        2
    );
    bench.close().await;
}

#[tokio::test]
async fn tool_results_and_file_touches_are_stored() {
    let case = case("report");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();

    let mut conn = connect(&bench.db()).await;
    let touches: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, path FROM file_touch ORDER BY kind, path")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let errors: i64 = sqlx::query_scalar("SELECT count(*) FROM tool_call WHERE is_error = 1")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    let unanswered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_call WHERE result_bytes = 0")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    conn.close().await.unwrap();
    let touches: Vec<(&str, &str)> = touches
        .iter()
        .map(|(kind, path)| (kind.as_str(), path.as_str()))
        .collect();
    // Relative to the repository of the message, with the separator of its paths; one row per
    // call, so the file read three times has three.
    assert_eq!(
        touches,
        [
            ("read", "src/lib.rs"),
            ("read", "src/lib.rs"),
            ("read", "src/lib.rs"),
            ("search", "src"),
            ("write", "src/lib.rs"),
            ("write", "src/new.rs"),
        ]
    );
    assert_eq!(errors, 1);
    assert_eq!(unanswered, 0, "every call got its result");
    bench.close().await;
}

/// #76, the twelve-runs fixture (`shell-touches`): one script run twelve times by commands
/// that also read another file with `sed -n` is twelve execs of the script and no read of
/// it. Mutant `mutant_ingest_exec_is_read`: the runs are twelve repeated reads.
#[tokio::test]
async fn twelve_runs_of_a_script_are_execs_not_reads() {
    let case = case("shell-touches");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();

    let mut conn = connect(&bench.db()).await;
    let touches: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT kind, path, count(*) FROM file_touch GROUP BY kind, path ORDER BY kind, path",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    let touches: Vec<(&str, &str, i64)> = touches
        .iter()
        .map(|(kind, path, n)| (kind.as_str(), path.as_str(), *n))
        .collect();
    assert_eq!(
        touches,
        [
            ("exec", "tool/check.py", 12),
            ("read", "src/lib.rs", 12),
            ("unknown", "src/lib.rs", 1),
        ]
    );
    bench.close().await;
}

/// #2, #76: a path out of a command's text outside the message's repository never reaches the
/// database as a file touch, and the target of a command is its head. The last command of the
/// fixture writes and reads files under `/fixture/elsewhere`, outside the repository.
///
/// Every message of the fixture lies inside the repository, so since #210 the redacted text
/// of its commands is stored; the claim that nothing of a command's text reaches the database
/// moved to [`a_command_text_is_kept_only_redacted_capped_and_inside_a_repository`], for a
/// message outside every repository.
#[tokio::test]
async fn a_command_stores_only_files_inside_the_repository() {
    let case = case("shell-touches");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();

    let mut conn = connect(&bench.db()).await;
    let paths: Vec<String> = sqlx::query_scalar("SELECT DISTINCT path FROM file_touch")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let targets: Vec<Option<String>> =
        sqlx::query_scalar("SELECT DISTINCT target FROM tool_call ORDER BY target")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    conn.close().await.unwrap();
    for path in &paths {
        assert!(!path.starts_with('/'), "{path}");
        assert!(!path.contains("elsewhere"), "{path}");
    }
    // The command itself is kept as its head only.
    assert_eq!(
        targets,
        [Some("rustfmt".to_owned()), Some("sed".to_owned())]
    );
    bench.close().await;
}

/// #210: the store's reader of tool calls carries the columns of parser version 4 that the
/// heat detectors X1, T1 and M1 read: every call of the `shell-touches` fixture has its times
/// and no media, and each of its thirteen commands (each a different `sed` range) its own
/// fingerprint, its redacted text and its head as the target.
#[tokio::test]
async fn tool_rows_carry_the_fields_of_parser_4() {
    let case = case("shell-touches");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();

    let rows = bench
        .store
        .reader()
        .tool_calls(i64::MIN, i64::MAX)
        .await
        .unwrap();
    assert_ne!(rows, []);
    for row in &rows {
        assert_eq!(row.parser_version, 4, "{row:?}");
        let called = row
            .call_ts_ms
            .unwrap_or_else(|| panic!("no call time: {row:?}"));
        let answered = row
            .result_ts_ms
            .unwrap_or_else(|| panic!("no result time: {row:?}"));
        assert!(answered > called, "{row:?}");
        assert_eq!(row.result_media_bytes, Some(0), "{row:?}");
    }
    let bash: Vec<_> = rows.iter().filter(|r| r.name == "Bash").collect();
    assert_eq!(bash.len(), 13);
    let hashes: std::collections::BTreeSet<&str> = bash
        .iter()
        .map(|r| {
            let hash = r
                .command_hash
                .as_deref()
                .unwrap_or_else(|| panic!("no hash: {r:?}"));
            assert_eq!(hash.len(), 64, "{r:?}");
            hash
        })
        .collect();
    assert_eq!(hashes.len(), 13, "{hashes:?}");
    for row in &bash {
        let target = row.target.as_deref();
        assert!(matches!(target, Some("sed" | "rustfmt")), "{row:?}");
    }
    assert_eq!(
        bash[0].command_text.as_deref(),
        Some("sed -n '1,40p' src/lib.rs && python tool/check.py -- cargo test")
    );
    bench.close().await;
}

/// Every byte of the database files (the database, its WAL and shared memory), as text.
fn database_dump(db: &Path) -> String {
    let dir = db.parent().unwrap().to_owned();
    let mut dump = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("bosun.db") && path.is_file() {
            dump.extend(std::fs::read(&path).unwrap());
        }
    }
    String::from_utf8_lossy(&dump).into_owned()
}

/// An `assistant` line of message `n` (a digit) in `cwd` with one `Bash` call of `command`.
fn bash_line(session: &str, n: u32, cwd: &str, command: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "sessionId": session,
        "cwd": cwd,
        "timestamp": format!("2000-01-30T00:00:0{n}.000Z"),
        "message": {
            "id": format!("msg_00000000000000009910000{n}"),
            "model": "claude-opus-5-5",
            "content": [{
                "type": "tool_use",
                "id": format!("toolu_00000000000000009919000{n}"),
                "name": "Bash",
                "input": {"command": command, "description": "Synthetic step"}
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    })
    .to_string()
}

/// #210: the text of a command is stored scrubbed and cut to 200 bytes, and only for a message
/// attributed to a repository; of a message in no repository neither its text nor its hash
/// reaches the database. Mutants `mutant_ingest_command_text_outside_repo` (kept whatever the
/// attribution) and `mutant_ingest_command_text_unredacted` (kept unscrubbed).
#[tokio::test]
async fn a_command_text_is_kept_only_redacted_capped_and_inside_a_repository() {
    // Built at run time, so that the source holds no token-shaped literal.
    let secret = format!("{}{}", "synthetic0123", "4567abcdefXYZ");
    let session = "00000000-0000-4000-8000-099100000000";
    let long = format!("echo {}", "synthetic ".repeat(50));
    let curl = format!(r#"curl -H "Authorization: Bearer {secret}" https://example.invalid/x"#);
    let mut transcript = String::new();
    for (n, cwd, command) in [
        (1, "/fixture/repo-a", curl.as_str()),
        (2, "/fixture/repo-a", long.as_str()),
        (
            3,
            "/fixture/elsewhere",
            "cat /fixture/elsewhere/notes.md --synthetic-flag",
        ),
    ] {
        transcript.push_str(&bash_line(session, n, cwd, command));
        transcript.push('\n');
    }
    let bench = Bench::new().await;
    let dir = bench.projects().join("-fixture-repo-a");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{session}.jsonl")), transcript).unwrap();
    // A case whose only root is `/fixture/repo-a`.
    let roots = case("shell-touches");
    let ingest = bench.ingest(claude::Parser, bench.projects(), &roots).await;
    ingest.scan().await.unwrap();

    let mut conn = connect(&bench.db()).await;
    let calls: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT tool_use_key, command_hash, command_text FROM tool_call ORDER BY tool_use_key",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let (_, hash, text) = &calls[0];
    assert!(hash.is_some());
    let text = text.as_deref().unwrap();
    assert!(text.contains("[redacted]"), "{text}");
    assert!(!text.contains(&secret), "{text}");
    let (_, hash, text) = &calls[1];
    assert!(hash.is_some());
    let text = text.as_deref().unwrap();
    assert!(text.len() <= 200 && long.starts_with(text), "{text}");
    assert_eq!(calls[2].1, None, "a message in no repository keeps no hash");
    assert_eq!(calls[2].2, None, "a message in no repository keeps no text");
    let dump = database_dump(&bench.db());
    for needle in [secret.as_str(), "--synthetic-flag", "notes.md"] {
        assert!(!dump.contains(needle), "{needle}");
    }
    bench.close().await;
}

#[tokio::test]
async fn a_new_message_lands_within_one_rescan_interval() {
    let case = case("partial-last-line");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let file = bench
        .projects()
        .join(relative_files(&case, "projects").remove(0));
    let every = Duration::from_millis(300);
    let options = Options {
        rescan_interval: every,
        // Watching off: only the rescan can find the new line (#21).
        watch: false,
        ..Options::default()
    };
    let ingest = bench
        .ingest_with(claude::Parser, bench.projects(), &case, options)
        .await;
    let handle = spawn(ingest);
    let db = bench.db();
    let messages = || scalar(&db, "SELECT count(*) FROM message");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while messages().await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first pass did not run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The newline of the torn line arrives; no command is sent: only the timer finds it.
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    std::io::Write::write_all(&mut appended, b"\n").unwrap();
    drop(appended);
    let written = tokio::time::Instant::now();
    while messages().await < 3 {
        // One interval, plus the pass itself and a margin for a loaded test machine.
        assert!(
            written.elapsed() < every * 2 + Duration::from_secs(2),
            "the message was not stored within one rescan interval"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(handle.last_pass().is_some_and(|pass| pass.is_ok()));
    handle.shutdown().await.unwrap();
    bench.close().await;
}

/// The Claude parser under another version number.
#[derive(Debug, Clone, Copy)]
struct NextVersion;

impl LineParser for NextVersion {
    fn provider(&self) -> &'static str {
        claude::PROVIDER
    }

    fn version(&self) -> u32 {
        claude::PARSER_VERSION + 1
    }

    fn parse_line(&self, source: &Source, offset: u64, line: &[u8]) -> Outcome {
        claude::Parser.parse_line(source, offset, line)
    }
}

impl Provider for NextVersion {
    fn locate(&self, path: &Path) -> Option<crate::Located> {
        claude::Parser.locate(path)
    }
}

/// The Claude parser as version 3 wrote its records: without a command's fingerprint, a call's
/// time, a result's time or its media bytes.
#[derive(Debug, Clone, Copy)]
struct PreviousVersion;

impl LineParser for PreviousVersion {
    fn provider(&self) -> &'static str {
        claude::PROVIDER
    }

    fn version(&self) -> u32 {
        claude::PARSER_VERSION - 1
    }

    fn parse_line(&self, source: &Source, offset: u64, line: &[u8]) -> Outcome {
        match claude::Parser.parse_line(source, offset, line) {
            Outcome::Records(records) => {
                Outcome::Records(records.into_iter().map(as_version_3).collect())
            }
            other => other,
        }
    }
}

impl Provider for PreviousVersion {
    fn locate(&self, path: &Path) -> Option<crate::Located> {
        claude::Parser.locate(path)
    }
}

/// `record` without the fields of #210.
fn as_version_3(record: Record) -> Record {
    match record {
        Record::Message(mut message) => {
            for call in message.tool_calls.values_mut() {
                call.command_hash = None;
                call.command_text = None;
                call.call_ts_ms = None;
                call.result_ts_ms = None;
                call.result_media_bytes = 0;
            }
            Record::Message(message)
        }
        Record::ToolResult(mut result) => {
            result.ts_ms = None;
            result.media_bytes = 0;
            Record::ToolResult(result)
        }
        other => other,
    }
}

/// The columns of #210 of one tool call, with its name and result size.
#[derive(Debug, sqlx::FromRow)]
struct NewColumns {
    name: String,
    command_hash: Option<String>,
    command_text: Option<String>,
    call_ts_ms: Option<i64>,
    result_ts_ms: Option<i64>,
    result_media_bytes: Option<i64>,
    result_bytes: i64,
}

async fn new_columns(db: &Path) -> Vec<NewColumns> {
    let mut conn = connect(db).await;
    let rows = sqlx::query_as(
        "SELECT tool_use_key, name, command_hash, command_text, call_ts_ms, result_ts_ms,
                result_media_bytes, result_bytes
         FROM tool_call ORDER BY tool_use_key",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    rows
}

/// #210: rows of parser version 3 hold NULL in the new columns; a reingest with version 4
/// fills them and moves no token. Mutant `mutant_ingest_result_time_from_call`: a result's
/// time is its call's.
#[tokio::test]
async fn reingest_fills_the_new_columns_and_changes_no_token_sum() {
    let case = case("shell-touches");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let old = bench.ingest(PreviousVersion, bench.projects(), &case).await;
    old.scan().await.unwrap();
    let want = want(&case);
    assert_eq!(stored_totals(&bench.db()).await, want);
    let counts = row_counts(&bench.db()).await;
    let before = new_columns(&bench.db()).await;
    assert!(!before.is_empty());
    for call in &before {
        assert!(
            call.command_hash.is_none()
                && call.command_text.is_none()
                && call.call_ts_ms.is_none()
                && call.result_ts_ms.is_none()
                && call.result_media_bytes.is_none(),
            "{call:?}"
        );
    }

    let next = bench.ingest(claude::Parser, bench.projects(), &case).await;
    next.reingest().await.unwrap();
    assert_eq!(stored_totals(&bench.db()).await, want);
    assert_eq!(row_counts(&bench.db()).await, counts);
    let versions: Vec<i64> = {
        let mut conn = connect(&bench.db()).await;
        let versions = sqlx::query_scalar("SELECT DISTINCT parser_version FROM message")
            .fetch_all(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        versions
    };
    assert_eq!(claude::PARSER_VERSION, 4);
    assert_eq!(versions, [4]);
    let after = new_columns(&bench.db()).await;
    assert_eq!(after.len(), before.len());
    for call in &after {
        if call.name == "Bash" {
            assert!(call.command_hash.is_some(), "{call:?}");
        }
        let called = call
            .call_ts_ms
            .unwrap_or_else(|| panic!("no time: {call:?}"));
        if call.result_bytes > 0 {
            let answered = call
                .result_ts_ms
                .unwrap_or_else(|| panic!("no result time: {call:?}"));
            assert!(answered > called, "{call:?}");
            assert_eq!(call.result_media_bytes, Some(0), "{call:?}");
        }
    }
    bench.close().await;
}

#[tokio::test]
async fn reingest_updates_existing_files_and_keeps_deleted_ones() {
    let case = case("report");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    let want = want(&case);
    let counts = row_counts(&bench.db()).await;

    let files = relative_files(&case, "projects");
    let deleted = bench.projects().join(&files[1]);
    let deleted_session = files[1].file_stem().unwrap().to_string_lossy().into_owned();
    std::fs::remove_file(&deleted).unwrap();

    let next = bench.ingest(NextVersion, bench.projects(), &case).await;
    let report = next.reingest().await.unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(stored_totals(&bench.db()).await, want);
    assert_eq!(row_counts(&bench.db()).await, counts);

    let mut conn = connect(&bench.db()).await;
    let versions: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT s.session_key, m.parser_version, count(*)
         FROM message m JOIN session s ON s.id = m.session_id
         GROUP BY s.session_key, m.parser_version ORDER BY s.session_key",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let file_versions: Vec<i64> =
        sqlx::query_scalar("SELECT parser_version FROM source_file ORDER BY path")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    conn.close().await.unwrap();
    let old = i64::from(claude::PARSER_VERSION);
    for (session, version, _) in &versions {
        let expected = if *session == deleted_session {
            old
        } else {
            old + 1
        };
        assert_eq!(*version, expected, "{session}: {versions:?}");
    }
    assert_eq!(versions.len(), 2, "{versions:?}");
    assert!(file_versions.contains(&old) && file_versions.contains(&(old + 1)));
    bench.close().await;
}

struct Local;

impl bosun_config::DiskProbe for Local {
    fn probe(&self, _: &Path) -> bosun_config::Disk {
        bosun_config::Disk::Local
    }
}

#[tokio::test]
async fn each_identity_holds_only_its_own_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let identities = [("personal", "split-message"), ("work", "cwd-change")];
    let mut text = String::new();
    for (name, fixture) in identities {
        let config_dir = dir.path().join(name).join("claude");
        copy_case(&case(fixture), &config_dir.join("projects"));
        writeln!(
            text,
            "[identity.{name}]\nclaude_config_dir = '{}'\ndatabase = '{}'",
            config_dir.display(),
            dir.path().join(name).join("bosun.db").display(),
        )
        .unwrap();
    }
    let config = bosun_config::parse(&text, &Local).unwrap();
    let plans = crate::plans(&config);
    assert_eq!(plans.len(), 2);

    for plan in &plans {
        let store = Store::open(&plan.database, StoreOptions::default())
            .await
            .unwrap();
        let ingest = Ingest::new(
            claude::Parser,
            plan.claude_projects.clone(),
            &store,
            Options::default(),
        );
        ingest.register_repos(&plan.repo_roots).await.unwrap();
        ingest.scan().await.unwrap();
        store.close().await.unwrap();
    }

    for (plan, (name, fixture)) in plans.iter().zip(identities) {
        assert_eq!(plan.identity, name);
        let mut conn = connect(&plan.database).await;
        let sessions: Vec<String> = sqlx::query_scalar("SELECT session_key FROM session")
            .fetch_all(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let own: Vec<String> = relative_files(&case(fixture), "projects")
            .iter()
            .map(|f| f.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(!sessions.is_empty(), "{name}");
        assert_eq!(sessions, own, "{name} holds another identity's sessions");
    }
}

#[tokio::test]
async fn subagents_are_ingested_and_agree_with_cost_state() {
    let case = case("subagent-cost-state");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    assert_eq!(stored_totals(&bench.db()).await, want(&case));

    let mut conn = connect(&bench.db()).await;
    // The provider's own totals per model equal the stored messages of the root session and
    // its sub-agents.
    let checks: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT model, input_tokens, cache_write_tokens, cache_read_tokens, output_tokens
         FROM session_check ORDER BY model",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let sums: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT md.name, sum(m.input_tokens),
                sum(m.cache_write_5m_tokens + m.cache_write_1h_tokens),
                sum(m.cache_read_tokens), sum(m.output_tokens)
         FROM message m
         JOIN model md ON md.id = m.model_id
         JOIN session s ON s.id = m.session_id
         JOIN session root ON root.id = s.root_id
         JOIN session_check c ON c.root_session_id = root.id AND c.model = md.name
         GROUP BY md.name ORDER BY md.name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let agent: (Option<String>, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT s.agent_type, s.spawn_depth, s.spawn_tool_use_id, p.session_key
         FROM session s JOIN session p ON p.id = s.parent_id
         WHERE s.agent_key IS NOT NULL",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    assert_eq!(checks.len(), 2, "{checks:?}");
    assert_eq!(
        checks, sums,
        "cost-state disagrees with the stored messages"
    );
    assert_eq!(
        agent,
        (
            Some("general-purpose".to_owned()),
            Some(1),
            Some("toolu_000000000000000003190001".to_owned()),
            Some("00000000-0000-4000-8000-003100000000".to_owned()),
        )
    );
    bench.close().await;
}

#[tokio::test]
async fn the_stub_fills_the_same_tables() {
    for case in cases("stub") {
        let bench = Bench::new().await;
        let sessions = bench.dir.path().join("stub").join("sessions");
        for file in relative_files(&case, "sessions") {
            copy_file(&case, "sessions", &file, &sessions);
        }
        let ingest = bench.ingest(stub::Parser, sessions, &case).await;
        let report = ingest.scan().await.unwrap();
        assert_eq!(report.failed, []);
        assert_eq!(
            stored_totals(&bench.db()).await,
            want(&case),
            "{}",
            case.display()
        );
        let calls = scalar(
            &bench.db(),
            "SELECT count(*) FROM tool_call WHERE result_bytes > 0",
        )
        .await;
        assert_eq!(calls, 2);
        let touches = scalar(&bench.db(), "SELECT count(*) FROM file_touch").await;
        assert_eq!(touches, 1);
        bench.close().await;
    }
}

#[tokio::test]
async fn small_reads_give_the_same_rows() {
    let case = case("subagent-cost-state");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    // Smaller than any line: every read doubles until one line fits, one batch per line.
    let options = Options {
        read_limit: 16,
        ..Options::default()
    };
    let ingest = bench
        .ingest_with(claude::Parser, bench.projects(), &case, options)
        .await;
    ingest.scan().await.unwrap();
    assert_eq!(stored_totals(&bench.db()).await, want(&case));
    assert_eq!(ingest.scan().await.unwrap().parsed_bytes, 0);
    bench.close().await;
}

/// The stored check of the `resumed-cost-state` case: its window (start, end, total cost) and
/// its per-model rows (model, input, cache write, cache read, output, cost).
type StoredCheck = (
    Vec<(Option<i64>, Option<i64>, Option<f64>)>,
    Vec<(String, i64, i64, i64, i64, Option<f64>)>,
);

async fn stored_check(db: &Path) -> StoredCheck {
    let mut conn = connect(db).await;
    let window =
        sqlx::query_as("SELECT start_ms, end_ms, total_cost_usd FROM session_check_window")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let models = sqlx::query_as(
        "SELECT model, input_tokens, cache_write_tokens, cache_read_tokens, output_tokens,
                cost_usd
         FROM session_check ORDER BY model",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    (window, models)
}

/// The last `cost-state` of the main transcript of `resumed-cost-state`, as the case's README
/// row describes it.
fn resumed_check() -> StoredCheck {
    // 2000-01-11T01:00:00Z, the resume, and 01:00:09, the `system` record before the line.
    let start = 947_552_400_000;
    (
        vec![(Some(start), Some(start + 9_000), None)],
        vec![
            ("claude-haiku-5-5".to_owned(), 100, 0, 0, 50, None),
            (
                "claude-opus-5-5[1m]".to_owned(),
                10,
                920,
                16_500,
                90,
                Some(0.2),
            ),
            (
                "claude-sonnet-5-5".to_owned(),
                5,
                430,
                11_000,
                1300,
                Some(0.05),
            ),
        ],
    )
}

const RESUMED_MAIN: &str = "-fixture-repo-a/00000000-0000-4000-8000-011100000000.jsonl";

/// #46: the check's window ends at the last timestamp before its line, also when the file is
/// read in pieces: the line starts a read, and the timestamp before it was read by an earlier
/// pass of another ingest, so it comes from the stored file row.
#[tokio::test]
async fn the_check_window_ends_at_the_last_timestamp_before_its_line() {
    let case = case("resumed-cost-state");
    let text = std::fs::read_to_string(case.join("projects").join(RESUMED_MAIN)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // Up to the `system` record at 01:00:09; the last `cost-state` is the next line.
    let cut = lines
        .iter()
        .rposition(|line| line.contains("\"type\":\"cost-state\""))
        .unwrap();
    for read_limit in [Options::default().read_limit, 16] {
        let bench = Bench::new().await;
        let main = bench.projects().join(RESUMED_MAIN);
        std::fs::create_dir_all(main.parent().unwrap()).unwrap();
        std::fs::write(&main, lines[..cut].join("\n") + "\n").unwrap();
        let options = Options {
            read_limit,
            ..Options::default()
        };
        let first = bench
            .ingest_with(claude::Parser, bench.projects(), &case, options.clone())
            .await;
        first.scan().await.unwrap();

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap();
        std::io::Write::write_all(&mut file, (lines[cut..].join("\n") + "\n").as_bytes()).unwrap();
        drop(file);
        let second = bench
            .ingest_with(claude::Parser, bench.projects(), &case, options)
            .await;
        let report = second.scan().await.unwrap();
        assert_eq!(report.restarted, 0, "the file grew; it is not re-read");
        assert_eq!(
            stored_check(&bench.db()).await,
            resumed_check(),
            "{read_limit}"
        );
        bench.close().await;
    }
}

/// #46: a `cost-state` line in a sub-agent's file does not replace the root session's check,
/// though it is read after the root's. Mutant `mutant_ingest_subagent_cost_state`.
#[tokio::test]
async fn a_subagent_cost_state_is_not_the_session_check() {
    let case = case("resumed-cost-state");
    let bench = Bench::new().await;
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    let (main, subagent): (Vec<PathBuf>, Vec<PathBuf>) = relative_files(&case, "projects")
        .into_iter()
        .partition(|file| !file.to_string_lossy().contains("subagents"));
    for file in &main {
        copy_file(&case, "projects", file, &bench.projects());
    }
    ingest.scan().await.unwrap();
    assert_eq!(stored_check(&bench.db()).await, resumed_check());

    assert_eq!(subagent.len(), 2, "{subagent:?}");
    for file in &subagent {
        copy_file(&case, "projects", file, &bench.projects());
    }
    ingest.scan().await.unwrap();
    assert_eq!(stored_totals(&bench.db()).await, want(&case));
    assert_eq!(
        stored_check(&bench.db()).await,
        resumed_check(),
        "the sub-agent's cost-state replaced the session's"
    );
    bench.close().await;
}

/// Every window's stored span and its first and last message, by SQL: (session, agent, start,
/// end, first, last).
type SpanRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

async fn spans(db: &Path) -> Vec<SpanRow> {
    let mut conn = connect(db).await;
    let rows = sqlx::query_as(
        "SELECT s.session_key, s.agent_key, s.started_ms, s.ended_ms,
                (SELECT min(ts_ms) FROM message WHERE session_id = s.id),
                (SELECT max(ts_ms) FROM message WHERE session_id = s.id)
         FROM session s ORDER BY s.session_key, s.agent_key",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    rows
}

/// Asserts that every window of `rows` spans exactly its first and last message.
fn assert_spans(rows: &[SpanRow], what: &str) {
    assert!(
        rows.iter().any(|r| r.4.is_some()),
        "{what}: no window has messages"
    );
    for (session, agent, start, end, first, last) in rows {
        assert_eq!(
            (start, end),
            (first, last),
            "{what}: window {session} {agent:?}"
        );
    }
}

/// #95: after an ingest, every window with messages, sub-agents included, has both bounds,
/// equal to its first and last message; also after a reingest, and when the files of a fork
/// are scanned one at a time in either order.
#[tokio::test]
async fn every_window_spans_its_first_and_last_message() {
    let mut agents = 0;
    for case in cases("claude") {
        let bench = Bench::new().await;
        copy_case(&case, &bench.projects());
        let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
        ingest.scan().await.unwrap();
        let rows = spans(&bench.db()).await;
        assert_spans(&rows, &case.display().to_string());
        agents += rows
            .iter()
            .filter(|r| r.1.is_some() && r.2.is_some())
            .count();
        ingest.reingest().await.unwrap();
        assert_eq!(
            spans(&bench.db()).await,
            rows,
            "{}: reingest",
            case.display()
        );
        bench.close().await;
    }
    assert!(agents > 0, "no sub-agent window was checked");

    let case = case("fork-continue");
    let files = relative_files(&case, "projects");
    let mut seen = Vec::new();
    for order in [files.clone(), files.into_iter().rev().collect()] {
        let bench = Bench::new().await;
        let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
        for file in &order {
            copy_file(&case, "projects", file, &bench.projects());
            ingest.scan().await.unwrap();
        }
        let rows = spans(&bench.db()).await;
        assert_spans(&rows, &format!("{order:?}"));
        seen.push(rows);
        bench.close().await;
    }
    assert_eq!(seen[0], seen[1], "the scan order changed the spans");
}

/// #95, #23: on an ingested store (no span seeded), a commit inside an interactive window's
/// span in the repository of its messages is linked to it; one after the span is not.
#[tokio::test]
async fn an_ingested_session_links_a_commit_inside_its_span() {
    let case = case("cwd-change");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    // A main window, the repository of its messages, and its first and last message there.
    let mut conn = connect(&bench.db()).await;
    let (session, repo, first, last): (String, String, i64, i64) = sqlx::query_as(
        "SELECT s.session_key, r.path, min(m.ts_ms), max(m.ts_ms)
         FROM message m JOIN session s ON s.id = m.session_id JOIN repo r ON r.id = m.repo_id
         WHERE s.kind = 'interactive' AND s.agent_key IS NULL
         GROUP BY s.id, r.id ORDER BY s.session_key, r.path LIMIT 1",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    // Git keeps whole seconds: the second that holds the middle of the span, and one after it.
    let second = |ms: i64| ms.div_euclid(1000) * 1000;
    let commit = |sha: &str, ts_ms: i64| Write::Commit {
        repo: repo.clone(),
        commit: Commit {
            sha: sha.to_owned(),
            ts_ms,
            author: "Fixture <fixture@example.invalid>".to_owned(),
            lines_added: 1,
            lines_removed: 0,
            trailers: BTreeMap::new(),
        },
    };
    bench
        .store
        .writer()
        .write(vec![
            commit("inside", second(first + (last - first) / 2)),
            commit("after", second(last) + 5_000),
        ])
        .await
        .unwrap();

    let links = bench
        .store
        .reader()
        .commit_links(0, i64::MAX)
        .await
        .unwrap();
    let linked = |sha: &str| -> Vec<String> {
        links
            .iter()
            .find(|l| l.commit.sha == sha)
            .unwrap()
            .sessions
            .iter()
            .map(|s| s.session_key.clone())
            .collect()
    };
    assert!(
        linked("inside").contains(&session),
        "{session} is not linked: {:?}",
        linked("inside")
    );
    assert_eq!(linked("after"), Vec::<String>::new());
    bench.close().await;
}

/// The drift counters of the transcript row whose path ends with `name`: unknown lines, usage
/// lines and unbilled lines.
async fn drift_of(bench: &Bench, name: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let files = bench.store.reader().source_files("claude").await.unwrap();
    let file = files
        .iter()
        .find(|file| file.path.ends_with(name))
        .unwrap_or_else(|| panic!("no row for {name}: {files:?}"));
    (file.unknown_lines, file.usage_lines, file.unbilled_lines)
}

/// The one transcript of `case`, relative to its `projects`.
fn transcript_of(case: &Path) -> PathBuf {
    let files = relative_files(case, "projects");
    assert_eq!(files.len(), 1, "{files:?}");
    files.into_iter().next().unwrap()
}

fn file_name(relative: &Path) -> String {
    relative.file_name().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn unknown_records_and_unbilled_usage_records_are_counted_per_file() {
    for (name, want) in [
        ("skipped-records", (Some(1), Some(4), Some(2))),
        ("split-message", (Some(0), Some(4), Some(0))),
    ] {
        let case = case(name);
        let bench = Bench::new().await;
        copy_case(&case, &bench.projects());
        let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
        ingest.scan().await.unwrap();
        let transcript = file_name(&transcript_of(&case));
        assert_eq!(drift_of(&bench, &transcript).await, want, "{name}");
        bench.close().await;
    }

    // The provider renamed its usage field: every assistant line still parses as a known
    // type, and bills nothing.
    let case = case("split-message");
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let relative = transcript_of(&case);
    let path = bench.projects().join(&relative);
    let text = std::fs::read_to_string(&path).unwrap();
    let renamed: String = text
        .split_inclusive('\n')
        .map(|line| {
            if line.contains(r#""type":"assistant""#) {
                line.replace(r#""usage":"#, r#""usage_v2":"#)
            } else {
                line.to_owned()
            }
        })
        .collect();
    assert_ne!(renamed, text, "the case has usage fields to rename");
    std::fs::write(&path, renamed).unwrap();
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    let want = (Some(0), Some(4), Some(4));
    assert_eq!(drift_of(&bench, &file_name(&relative)).await, want);
    bench.close().await;
}

#[tokio::test]
async fn the_drift_counts_carry_across_reads_and_restart_with_a_reingest() {
    let case = case("skipped-records");
    let relative = transcript_of(&case);
    let name = file_name(&relative);
    let want = (Some(1), Some(4), Some(2));

    // Many small reads count what one read does.
    let bench = Bench::new().await;
    copy_case(&case, &bench.projects());
    let options = Options {
        read_limit: 16,
        ..Options::default()
    };
    let ingest = bench
        .ingest_with(claude::Parser, bench.projects(), &case, options)
        .await;
    ingest.scan().await.unwrap();
    assert_eq!(drift_of(&bench, &name).await, want, "read_limit 16");
    bench.close().await;

    // Half the lines, then the rest appended: a resumed read adds its counts; a reingest
    // counts again from zero, not twice.
    let bench = Bench::new().await;
    let text = std::fs::read(case.join("projects").join(&relative)).unwrap();
    let lines: Vec<&[u8]> = text.split_inclusive(|&b| b == b'\n').collect();
    assert_eq!(lines.len(), 10);
    let (first, rest) = lines.split_at(lines.len() / 2);
    let path = bench.projects().join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, first.concat()).unwrap();
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    let half = drift_of(&bench, &name).await;
    assert!(half.1.is_some() && half != want, "{half:?}");
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut appended, &rest.concat()).unwrap();
    drop(appended);
    ingest.scan().await.unwrap();
    assert_eq!(drift_of(&bench, &name).await, want, "after the append");
    ingest.reingest().await.unwrap();
    assert_eq!(drift_of(&bench, &name).await, want, "after a reingest");

    // A row of a file parsed before the counters existed: they stay unknown through a resumed
    // read, until a reingest counts the file from zero.
    std::fs::write(&path, first.concat()).unwrap();
    ingest.reingest().await.unwrap();
    let files = bench.store.reader().source_files("claude").await.unwrap();
    let mut old = files
        .into_iter()
        .find(|file| file.path.ends_with(&name))
        .unwrap();
    old.unknown_lines = None;
    old.usage_lines = None;
    old.unbilled_lines = None;
    bench
        .store
        .writer()
        .write(vec![Write::SourceFile(old)])
        .await
        .unwrap();
    // A new ingest reads the stored rows afresh, as after a daemon restart.
    let ingest = bench.ingest(claude::Parser, bench.projects(), &case).await;
    ingest.scan().await.unwrap();
    assert_eq!(drift_of(&bench, &name).await, (None, None, None));
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut appended, &rest.concat()).unwrap();
    drop(appended);
    ingest.scan().await.unwrap();
    let parsed = bench.store.reader().source_files("claude").await.unwrap();
    let parsed = parsed
        .iter()
        .find(|file| file.path.ends_with(&name))
        .unwrap();
    assert_eq!(parsed.parsed_offset, text.len() as u64, "the rest was read");
    assert_eq!(drift_of(&bench, &name).await, (None, None, None));
    ingest.reingest().await.unwrap();
    assert_eq!(
        drift_of(&bench, &name).await,
        want,
        "counted after a reingest"
    );
    bench.close().await;
}
