//! The session shim (#8), end to end: `bosun-shim` runs `bosun-proc-helper` as its child, and the
//! tests read the record and the output files the shim writes. Each test works in its own
//! temporary directory and kills only the pids it started.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bosun_proc::shim::{Record, Spec};
use bosun_proc::{ProcessKey, is_running, reap, start_time};
use tempfile::TempDir;

const SHIM: &str = env!("CARGO_BIN_EXE_bosun-shim");
const HELPER: &str = env!("CARGO_BIN_EXE_bosun-proc-helper");

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
