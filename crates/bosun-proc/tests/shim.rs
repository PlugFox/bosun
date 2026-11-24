//! The session shim (#8), end to end: `bosun-shim` runs `bosun-proc-helper` as its child, and the
//! tests read the record and the output files the shim writes. Each test works in its own
//! temporary directory and kills only the pids it started.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bosun_proc::shim::{Offsets, Record, ShimSpawner, Spec, attach};
use bosun_proc::{ChildSpec, ProcessKey, Spawner, StartTime, is_running, reap, start_time};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

const SHIM: &str = env!("CARGO_BIN_EXE_bosun-shim");
const HELPER: &str = env!("CARGO_BIN_EXE_bosun-proc-helper");

/// The environment a helper child gets through a session: only what Windows programs need.
fn helper_env() -> Vec<(OsString, OsString)> {
    std::env::var_os("SystemRoot")
        .map_or_else(Vec::new, |root| vec![(OsString::from("SystemRoot"), root)])
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Reads from `stream` until `marker` has come in, and returns every byte read so far.
async fn read_through(stream: &mut (impl AsyncReadExt + Unpin), marker: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let mut chunk = [0_u8; 256];
    while !contains(&got, marker) {
        let n = stream.read(&mut chunk).await.expect("read the output");
        assert_ne!(n, 0, "the output ended before {marker:?}");
        got.extend_from_slice(&chunk[..n]);
    }
    got
}

/// The one session directory under `sessions`, and the record of its spec.
fn only_record(sessions: &Path) -> PathBuf {
    let mut dirs = fs::read_dir(sessions)
        .expect("the sessions directory")
        .map(|e| e.expect("an entry").path());
    let session = dirs.next().expect("one session");
    assert!(dirs.next().is_none(), "one session, not two");
    session.join("spec.record")
}

/// Writes a spec that runs the helper with `role` into `dir`, and returns the spec's path.
fn write_spec(dir: &Path, role: &str, cap: Duration) -> PathBuf {
    let mut env = Vec::new();
    // Windows programs expect the system root; nothing else of this process is passed.
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.push(("SystemRoot".to_owned(), root.to_string_lossy().into_owned()));
    }
    let spec = Spec {
        program: HELPER.to_owned(),
        args: vec![role.to_owned()],
        env,
        cwd: dir.to_owned(),
        output: dir.join("out"),
        cap,
    };
    let path = dir.join("spec.json");
    spec.write_to(&path).expect("write the spec");
    path
}

/// Starts the shim for `spec`. Its output is discarded: the tests read the files it writes.
fn start_shim(spec: &Path) -> Child {
    Command::new(SHIM)
        .arg(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the shim")
}

/// Polls the record next to `spec` until `ready` accepts it, for at most `limit`.
fn wait_for_record(spec: &Path, limit: Duration, ready: impl Fn(&Record) -> bool) -> Record {
    let record_path = spec.with_extension("record");
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(record) = Record::read_from(&record_path)
            && ready(&record)
        {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "the record {} does not satisfy the check within {limit:?}",
            record_path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The value the helper reported under `key` in the file `path`, polled for at most `limit`.
fn wait_for_report(path: &Path, key: &str, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        let found = text.lines().find_map(|line| {
            line.strip_prefix("bosun-proc-helper ")?
                .strip_prefix(key)?
                .strip_prefix(' ')
                .map(str::to_owned)
        });
        if let Some(value) = found {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{key} is not reported in {} within {limit:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn key_of(pid: u32) -> ProcessKey {
    let start = start_time(pid)
        .expect("start time")
        .expect("a live process");
    ProcessKey { pid, start }
}

/// Polls until no process of `keys` runs, for at most `limit`. Returns those still running.
fn survivors(keys: &[ProcessKey], limit: Duration) -> Vec<ProcessKey> {
    let deadline = Instant::now() + limit;
    loop {
        let left: Vec<_> = keys.iter().copied().filter(|&k| is_running(k)).collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kills whatever of `keys` still runs, so that a failed test leaves no helper behind.
fn clean_up(keys: &[ProcessKey]) {
    for &key in keys {
        reap(key).ok();
    }
}

/// A killed shim takes its child and the child's grandchild with it, within 5 s: the container's
/// kill-on-close reaches every process the shim started, and a shim that was killed without
/// warning ends with the same effect.
#[test]
fn a_killed_shim_leaves_no_descendant_within_5_s() {
    let dir = TempDir::new().expect("tempdir");
    let spec = write_spec(dir.path(), "parent-stay", Duration::from_secs(60));
    let mut shim = start_shim(&spec);
    let record = wait_for_record(&spec, Duration::from_secs(10), |_| true);
    let grandchild = key_of(
        wait_for_report(
            &dir.path().join("out/stdout.log"),
            "grandchild",
            Duration::from_secs(10),
        )
        .parse()
        .expect("a pid"),
    );
    let keys = [record.child, grandchild];

    shim.kill().expect("kill the shim");
    shim.wait().expect("wait for the killed shim");
    let left = survivors(&keys, Duration::from_secs(5));
    clean_up(&keys);
    assert!(
        left.is_empty(),
        "still running after the shim died: {left:?}"
    );
}

/// A child that exits while nobody waits for the shim still has its exit code in the record,
/// and its output in the files the record names: the shim, not its caller, does the recording.
#[test]
fn the_exit_of_a_child_nobody_waited_for_is_in_the_record() {
    let dir = TempDir::new().expect("tempdir");
    let spec = write_spec(dir.path(), "speak", Duration::from_secs(60));
    let mut shim = start_shim(&spec);

    let record = wait_for_record(&spec, Duration::from_secs(10), |r| r.exit.is_some());
    assert_eq!(record.exit, Some(3), "the child's exit code");
    assert!(!record.capped, "the child was not capped");
    assert!(!is_running(record.child), "the child is gone");

    let stdout = fs::read_to_string(&record.stdout).expect("stdout file");
    assert!(
        stdout.contains("bosun-proc-helper greeting hello"),
        "stdout: {stdout:?}"
    );
    let stderr = fs::read_to_string(&record.stderr).expect("stderr file");
    assert!(
        stderr.contains("bosun-proc-helper alarm bell"),
        "stderr: {stderr:?}"
    );
    let status = shim.wait().expect("wait for the shim");
    assert_eq!(
        status.code(),
        Some(3),
        "the shim exits with the child's code"
    );
}

/// The wall-clock cap is enforced by the shim alone: a child that runs past the cap is killed,
/// and the record says so.
#[test]
fn the_wall_clock_cap_kills_the_child_and_records_it() {
    let dir = TempDir::new().expect("tempdir");
    let spec = write_spec(dir.path(), "sleep", Duration::from_millis(500));
    let mut shim = start_shim(&spec);

    let record = wait_for_record(&spec, Duration::from_secs(20), |r| r.exit.is_some());
    assert!(record.capped, "the cap killed the child");
    assert_ne!(record.exit, Some(0), "a killed child does not exit with 0");
    assert!(!is_running(record.child), "the capped child is gone");
    let status = shim.wait().expect("wait for the shim");
    assert_eq!(
        status.code(),
        record.exit,
        "the shim exits with the recorded code"
    );
}

/// A session outlives the holder that spawned it: the holder drops its streams and its tree
/// without killing, and a new attach from the record reads the rest of the output and the exit.
/// The session has one exit: both readers see the same one, and the output is never doubled.
#[tokio::test]
async fn a_session_survives_its_holder_and_is_reattached() {
    let dir = TempDir::new().expect("tempdir");
    let sessions = dir.path().join("sessions");
    let shims = ShimSpawner::new(SHIM, &sessions, Duration::from_secs(60));
    let spec = ChildSpec {
        program: HELPER.into(),
        args: vec!["slow-speak".into()],
        env: helper_env(),
        cwd: dir.path().to_owned(),
        stdin: false,
    };

    // The holder: reads the first line, then drops everything while the child still runs.
    let (record_path, consumed) = {
        let mut session = shims.spawn(&spec).expect("spawn through the shim");
        let first = read_through(&mut session.stdout, b"bosun-proc-helper first one\n").await;
        let record_path = only_record(&sessions);
        let record = Record::read_from(&record_path).expect("the record");
        assert!(record.exit.is_none(), "the child still runs");
        assert!(is_running(record.shim), "the shim outlives the spawn");
        assert!(is_running(record.child), "the child runs");
        (record_path, first.len() as u64)
        // The holder drops here, without a kill.
    };

    let mut again = attach(
        &record_path,
        Offsets {
            stdout: consumed,
            stderr: 0,
        },
    )
    .expect("attach from the record");
    let mut rest = Vec::new();
    again
        .stdout
        .read_to_end(&mut rest)
        .await
        .expect("the rest of stdout");
    assert_eq!(
        String::from_utf8_lossy(&rest),
        "bosun-proc-helper second two\n",
        "the rest of the output"
    );
    let status = again.tree.wait().await.expect("the exit");
    assert_eq!(status.code(), Some(3), "the child's exit");

    let mut from_start = attach(&record_path, Offsets::default()).expect("attach again");
    let mut all = Vec::new();
    from_start
        .stdout
        .read_to_end(&mut all)
        .await
        .expect("all of stdout");
    assert_eq!(
        String::from_utf8_lossy(&all),
        "bosun-proc-helper first one\nbosun-proc-helper second two\n",
        "the whole output, once"
    );
    let again_status = from_start.tree.wait().await.expect("the same exit");
    assert_eq!(again_status.code(), Some(3), "one session, one exit");
    let record = Record::read_from(&record_path).expect("the record");
    assert_eq!(record.exit, Some(3), "the record holds the exit");
}

/// A record whose shim pid is live but started at another time names another process, so attach
/// refuses it. The same record with the shim's real start time attaches, so the refusal is the
/// start time's doing and not the pid's.
#[test]
fn attach_refuses_a_record_whose_pid_changed_hands() {
    let dir = TempDir::new().expect("tempdir");
    let mut other = Command::new(HELPER)
        .arg("sleep")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a live process");
    let pid = other.id();
    let start = start_time(pid).expect("start time").expect("live");
    let output = dir.path().join("out");
    fs::create_dir_all(&output).expect("output directory");
    let (stdout, stderr) = (output.join("stdout.log"), output.join("stderr.log"));
    fs::write(&stdout, b"").expect("stdout file");
    fs::write(&stderr, b"").expect("stderr file");
    let record_of = |shim: ProcessKey| Record {
        shim,
        child: ProcessKey { pid, start },
        stdout: stdout.clone(),
        stderr: stderr.clone(),
        exit: None,
        capped: false,
    };
    let changed = dir.path().join("changed.record");
    record_of(ProcessKey {
        pid,
        start: StartTime::from_raw(start.raw() ^ 1),
    })
    .write_to(&changed)
    .expect("write the record");
    let genuine = dir.path().join("genuine.record");
    record_of(ProcessKey { pid, start })
        .write_to(&genuine)
        .expect("write the record");

    let refused = attach(&changed, Offsets::default()).is_err();
    let attached = attach(&genuine, Offsets::default()).is_ok();
    other.kill().ok();
    other.wait().ok();
    assert!(refused, "a record whose shim pid changed hands is refused");
    assert!(
        attached,
        "the same record with the shim's start time attaches"
    );
}

/// Killing a session through its tree ends the child and the shim, and the child's exit is in the
/// record, so a reader that waits sees it end.
#[tokio::test]
async fn kill_through_the_tree_ends_the_session() {
    let dir = TempDir::new().expect("tempdir");
    let sessions = dir.path().join("sessions");
    let shims = ShimSpawner::new(SHIM, &sessions, Duration::from_secs(60));
    let spec = ChildSpec {
        program: HELPER.into(),
        args: vec!["sleep".into()],
        env: helper_env(),
        cwd: dir.path().to_owned(),
        stdin: false,
    };
    let mut session = shims.spawn(&spec).expect("spawn through the shim");
    let record_path = only_record(&sessions);
    let record = Record::read_from(&record_path).expect("the record");
    assert!(record.exit.is_none(), "the child runs");

    session.tree.kill_tree().expect("kill through the tree");
    let status = session.tree.wait().await.expect("the exit after the kill");
    let keys = [record.child, record.shim];
    let left = survivors(&keys, Duration::from_secs(5));
    clean_up(&keys);
    assert!(left.is_empty(), "still running after the kill: {left:?}");
    assert!(status.code().is_some(), "the session has an exit code");
}
