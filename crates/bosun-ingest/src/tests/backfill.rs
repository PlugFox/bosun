//! Watching and the bounded backfill against a real store (#22), over transcripts generated in
//! temporary directories from the templates of `fixtures/README.md`: synthetic ids under the
//! case number 97, timestamps in January 2000, paths under `/fixture`.
//!
//! Latency is asserted on wall-clock deadlines far above what the mechanism needs (the
//! debounce is 50-500 ms; the deadline is 5 s), and every wait polls the store for the
//! awaited row rather than sleeping for a fixed time. The ordering the mutants break is
//! asserted on the scheduler's queue, not on time: a live message must land while the backfill
//! still has files queued, which a backfill taken first cannot do on any machine.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use bosun_store::{Options as StoreOptions, Store};
use tokio::time::Instant;

use super::{connect, row_counts, scalar, stored_totals};
use crate::meter::failpoint::{self, Point};
use crate::{Ingest, Options, Stats, claude, spawn};

/// Files of the live-latency backfill: the 10k of #22's acceptance. Each is small (1-4 lines
/// of about 600 bytes), and the test stops once the live file is checked, so it costs the
/// generation and the first walk, not the whole backfill.
const BACKFILL_FILES: u64 = 10_000;

/// Files of the bound test, which runs its backfill to the end. Fewer than 10k because the
/// whole run is bounded by the store's one writer (about 1 ms a batch on a Windows runner),
/// not by the bounds under test: the caps are fixed when the run starts, so the high-water
/// marks reach them within the first few dozen files, and a removed cap overshoots them as
/// early. The 10k-file backfill asserts the same counters in
/// `a_live_file_is_tailed_during_the_backfill`.
const BOUND_FILES: u64 = 2_000;

/// Files of the kill test, which ingests its corpus four times (once whole, three times
/// killed and resumed). Large enough that each failpoint fires well inside the backfill.
const KILL_FILES: u64 = 300;

const ROOT: &str = "/fixture/repo-a";

/// The id of session `n` (case 97).
fn session_id(n: u64) -> String {
    format!("00000000-0000-4000-8000-097{n:09}")
}

/// One assistant record of message `m` of session `n`, with its newline.
fn line(n: u64, m: u64) -> String {
    let id = n * 100 + m;
    let output = 1 + (n + m) % 5;
    let session = session_id(n);
    format!(
        concat!(
            r#"{{"parentUuid":null,"isSidechain":false,"userType":"external","cwd":"{root}","#,
            r#""sessionId":"{session}","version":"0.0.0","gitBranch":"main","type":"assistant","#,
            r#""uuid":"00000000-0000-4000-8000-197{id:09}","timestamp":"2000-01-27T00:00:{sec:02}.000Z","#,
            r#""entrypoint":"cli","message":{{"id":"msg_0000000000000000{id:08}","type":"message","#,
            r#""role":"assistant","model":"claude-opus-5-5","content":[{{"type":"text","#,
            r#""text":"Synthetic."}}],"stop_reason":"end_turn","stop_sequence":null,"#,
            r#""usage":{{"input_tokens":1,"cache_creation_input_tokens":2,"#,
            r#""cache_read_input_tokens":3,"cache_creation":{{"ephemeral_5m_input_tokens":2,"#,
            r#""ephemeral_1h_input_tokens":0}},"output_tokens":{output},"#,
            r#""service_tier":"standard"}}}},"requestId":"req_0000000000000000{id:08}"}}"#,
            "\n"
        ),
        root = ROOT,
        session = session,
        id = id,
        sec = m % 60,
        output = output,
    )
}

/// The transcript path of session `n` under `projects`, spread over 16 project directories.
fn transcript(projects: &Path, n: u64) -> PathBuf {
    projects
        .join(format!("-fixture-repo-a-{:02}", n % 16))
        .join(format!("{}.jsonl", session_id(n)))
}

/// A generated corpus: its files' total bytes (every line complete) and message count.
#[derive(Debug, Clone, Copy)]
struct Corpus {
    bytes: u64,
    messages: u64,
}

/// Writes sessions `from..to` with 1-4 messages each, dated two days ago so that they are
/// backfill, not live.
fn generate(projects: &Path, from: u64, to: u64) -> Corpus {
    let old = SystemTime::now() - Duration::from_hours(48);
    let mut corpus = Corpus {
        bytes: 0,
        messages: 0,
    };
    for n in from..to {
        let path = transcript(projects, n);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for m in 0..=n % 4 {
            text.push_str(&line(n, m));
            corpus.messages += 1;
        }
        corpus.bytes += text.len() as u64;
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.set_modified(old).unwrap();
    }
    corpus
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

async fn open(db: &Path) -> Store {
    Store::open(db, StoreOptions::default()).await.unwrap()
}

async fn messages(db: &Path) -> i64 {
    scalar(db, "SELECT count(*) FROM message").await
}

/// Waits until `db` holds at least `n` messages, or `limit` passes; returns how long it took.
async fn until_messages(db: &Path, n: i64, limit: Duration, what: &str) -> Duration {
    let start = Instant::now();
    loop {
        if messages(db).await >= n {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < limit,
            "{what}: {} of {n} messages after {limit:?}",
            messages(db).await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_message_lands_within_seconds_with_watching_on() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let db = dir.path().join("bosun.db");
    let live = transcript(&projects, 900_000);
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    append(&live, &line(900_000, 0));
    let store = open(&db).await;
    let debounce = Duration::from_millis(500);
    let options = Options {
        // Only the watch can find the new line within the test.
        rescan_interval: Duration::from_secs(3600),
        debounce,
        ..Options::default()
    };
    let ingest = Ingest::new(claude::Parser, projects, &store, options);
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    let handle = spawn(ingest);
    until_messages(&db, 1, Duration::from_secs(30), "the first pass").await;

    append(&live, &line(900_000, 1));
    let took = until_messages(&db, 2, Duration::from_secs(30), "the watched line").await;
    assert!(
        took < Duration::from_secs(5),
        "the new message took {took:?}"
    );

    // A burst of appends, each its own write and close: a read per debounce window the burst
    // spans (and one more for events that arrive while it reads), not one per append.
    let before = handle.stats().tails;
    let burst = Instant::now();
    for m in 2..202 {
        append(&live, &line(900_000, m));
    }
    let windows = u64::try_from(burst.elapsed().as_millis() / debounce.as_millis()).unwrap() + 2;
    until_messages(&db, 202, Duration::from_secs(30), "the burst").await;
    let tails = handle.stats().tails - before;
    assert!(
        tails <= 2 * windows,
        "a burst of 200 appends over {windows} debounce windows took {tails} reads"
    );
    handle.shutdown().await.unwrap();
    store.close().await.unwrap();
}

/// The bounds of the backfill tests: four tailers, reads of 2 KiB (files of three and four
/// lines take two, so tailers hold files across commits), and a byte budget of three reads.
fn bounded() -> Options {
    Options {
        max_open_files: usize::try_from(OPEN).unwrap(),
        read_limit: READ,
        max_in_flight_bytes: IN_FLIGHT,
        ..Options::default()
    }
}

const OPEN: u64 = 4;
const READ: u64 = 2 << 10;
const IN_FLIGHT: u64 = 3 * READ;

fn assert_bounded(stats: Stats) {
    assert!(
        (1..=OPEN).contains(&stats.max_open_files),
        "open files: {stats:?}"
    );
    assert!(
        (1..=IN_FLIGHT).contains(&stats.max_in_flight_bytes),
        "bytes in flight: {stats:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_file_is_tailed_during_the_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let db = dir.path().join("bosun.db");
    generate(&projects, 0, BACKFILL_FILES);
    // A session in progress: written now, so live from the first pass.
    let live = transcript(&projects, 900_000);
    append(&live, &line(900_000, 0));
    let store = open(&db).await;
    let options = Options {
        // Only the watch can find the appended line within the test.
        rescan_interval: Duration::from_secs(3600),
        debounce: Duration::from_millis(50),
        ..bounded()
    };
    let ingest = Ingest::new(claude::Parser, projects, &store, options);
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    let handle = spawn(ingest);
    // The first pass walks the whole corpus before it queues anything: a generous wait.
    let (_, at_first) = wait_live(&handle, &db, 0, Duration::from_secs(60)).await;
    assert!(
        at_first.queued_backfill > 0,
        "the live file waited for the backfill: {at_first:?}"
    );

    append(&live, &line(900_000, 1));
    let (took, at_second) = wait_live(&handle, &db, 1, Duration::from_secs(30)).await;
    assert!(
        took < Duration::from_secs(5),
        "the live message took {took:?} during the backfill"
    );
    assert!(
        at_second.queued_backfill > 0,
        "the backfill was over before the live message was stored: {at_second:?}"
    );
    // The bounds held under the 10k-file backfill so far, the live tails included.
    assert_bounded(handle.stats());
    // Shutdown waits for the running tailers only; the rest of the backfill is left for the
    // next start, as `a_killed_backfill_resumes_without_parsing_a_byte_twice` checks.
    handle.shutdown().await.unwrap();
    store.close().await.unwrap();
}

/// Waits for message `m` of the live session; returns how long it took and the counters read
/// just after the store showed it. The backfill queue only shrinks once planned, so a backfill
/// they show queued was queued when the message was stored.
async fn wait_live(
    handle: &crate::Handle,
    db: &Path,
    m: u64,
    limit: Duration,
) -> (Duration, Stats) {
    let id = format!("msg_0000000000000000{:08}", 900_000 * 100 + m);
    let start = Instant::now();
    loop {
        let mut conn = connect(db).await;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM message WHERE message_key = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        sqlx::Connection::close(conn).await.unwrap();
        let stats = handle.stats();
        if n == 1 {
            return (start.elapsed(), stats);
        }
        assert!(
            start.elapsed() < limit,
            "live message {m} not stored after {limit:?}: {stats:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backfill_holds_its_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let db = dir.path().join("bosun.db");
    let corpus = generate(&projects, 0, BOUND_FILES);
    let store = open(&db).await;
    let ingest = Ingest::new(claude::Parser, projects, &store, bounded());
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    let report = ingest.scan().await.unwrap();
    assert_eq!(report.failed, []);
    assert_eq!(report.files, BOUND_FILES);
    assert_eq!(report.parsed_bytes, corpus.bytes, "every byte once");

    let stats = ingest.stats();
    assert_eq!(stats.parsed_bytes, corpus.bytes);
    assert_bounded(stats);
    assert_eq!((stats.open_files, stats.in_flight_bytes), (0, 0));
    assert_eq!(messages(&db).await, i64::try_from(corpus.messages).unwrap());
    store.close().await.unwrap();
}

/// A rescan that lands while the backfill is still queued joins the running pass; the pass's
/// report counts the files found once, not once per discovery (found on a real history, #6:
/// 6660 files reported for 666).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_joined_by_a_rescan_counts_each_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let db = dir.path().join("bosun.db");
    let corpus = generate(&projects, 0, BOUND_FILES);
    let store = open(&db).await;
    let options = Options {
        rescan_interval: Duration::from_secs(3600),
        ..bounded()
    };
    let ingest = Ingest::new(claude::Parser, projects, &store, options);
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    let handle = spawn(ingest);
    // The first pass has planned its files and is reading them.
    let start = Instant::now();
    while handle.stats().queued_backfill == 0 {
        assert!(start.elapsed() < Duration::from_secs(60), "no backfill queued");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let report = handle.rescan().await.unwrap();
    assert_eq!(report.files, BOUND_FILES);
    assert_eq!(report.parsed_bytes, corpus.bytes, "every byte once");
    handle.shutdown().await.unwrap();
    store.close().await.unwrap();
}

const KILL_TEST: &str ="tests::backfill::a_killed_backfill_resumes_without_parsing_a_byte_twice";
const KILL_CHILD: &str = "BOSUN_INGEST_KILL_CHILD";

/// The options of every run of the kill test: small reads, so files take several commits and
/// a kill can fall between two of them.
fn kill_options() -> Options {
    Options {
        read_limit: 1 << 10,
        max_open_files: 4,
        ..Options::default()
    }
}

fn point_name(point: Point) -> &'static str {
    match point {
        Point::Read => "read",
        Point::Write => "write",
        Point::Commit => "commit",
    }
}

/// The child half: backfills `projects` into `db` and aborts at the `nth` time `point` is
/// reached.
async fn kill_child(spec: &str) {
    let mut parts = spec.split('|');
    let (projects, db, point, nth) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap().parse().unwrap(),
    );
    let point = [Point::Read, Point::Write, Point::Commit]
        .into_iter()
        .find(|p| point_name(*p) == point)
        .unwrap();
    let store = open(Path::new(db)).await;
    let ingest = Ingest::new(claude::Parser, projects.into(), &store, kill_options());
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    failpoint::arm(point, nth);
    let _ = ingest.scan().await;
    // Reaching this line means the failpoint did not fire; the parent sees a clean exit.
    eprintln!("failpoint not reached");
}

#[tokio::test]
async fn a_killed_backfill_resumes_without_parsing_a_byte_twice() {
    if let Ok(spec) = std::env::var(KILL_CHILD) {
        kill_child(&spec).await;
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let corpus = generate(&projects, 0, KILL_FILES);

    // The uninterrupted run.
    let whole = dir.path().join("whole.db");
    let store = open(&whole).await;
    let ingest = Ingest::new(claude::Parser, projects.clone(), &store, kill_options());
    ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
    assert_eq!(ingest.scan().await.unwrap().parsed_bytes, corpus.bytes);
    store.close().await.unwrap();
    let want = stored_totals(&whole).await;
    let want_rows = row_counts(&whole).await;

    for (point, nth) in [(Point::Read, 40), (Point::Write, 120), (Point::Commit, 200)] {
        let name = point_name(point);
        let db = dir.path().join(format!("killed-{name}.db"));
        let mut spec = String::new();
        write!(spec, "{}|{}|{name}|{nth}", projects.display(), db.display()).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([KILL_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(KILL_CHILD, &spec)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            !child.status.success(),
            "{name}: the child did not abort: {stderr}"
        );
        assert!(
            stderr.contains(&format!("failpoint: abort at {point:?}")),
            "{name}: the child failed before the failpoint: {stderr}"
        );

        // What the killed run committed: the offsets stored with its rows.
        let committed = scalar(
            &db,
            "SELECT coalesce(sum(parsed_offset), 0) FROM source_file",
        )
        .await;
        let committed = u64::try_from(committed).unwrap();
        assert!(
            0 < committed && committed < corpus.bytes,
            "{name}: the kill fell outside the backfill ({committed} of {} bytes)",
            corpus.bytes
        );

        // The restart parses exactly the bytes the killed run did not commit.
        let store = open(&db).await;
        let ingest = Ingest::new(claude::Parser, projects.clone(), &store, kill_options());
        let report = ingest.scan().await.unwrap();
        assert_eq!(report.failed, [], "{name}");
        assert_eq!(
            report.parsed_bytes,
            corpus.bytes - committed,
            "{name}: the restart parsed committed bytes again"
        );
        assert_eq!(report.restarted, 0, "{name}: a file was re-read from zero");
        assert_eq!(ingest.scan().await.unwrap().parsed_bytes, 0, "{name}");
        store.close().await.unwrap();
        assert_eq!(stored_totals(&db).await, want, "{name}");
        assert_eq!(row_counts(&db).await, want_rows, "{name}");
    }
}
