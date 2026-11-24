//! The session shim (#129) as a process: started with `bosun-shim <spec>`, its child is this
//! test executable re-invoked with one test selected ([`run_role`]) and the role in an
//! environment variable, as the crate's own helper does. A helper reports on stdout, which the
//! shim sends to a file, as `bosun-shim-helper <key> <value>` lines.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bosun_proc::shim::{
    Exit, Offsets, RECORD_VERSION, Record, ShimSpawner, Spec, attach, record_path, session_record,
};
use bosun_proc::{ChildSpec, ProcessKey, StartTime, is_running, reap, start_time};

const ROLE: &str = "BOSUN_SHIM_HELPER";
/// The directory a `holder` helper works in.
const DIR: &str = "BOSUN_SHIM_HELPER_DIR";
/// The code the `talk` role exits with.
const TALKED_CODE: i32 = 9;
const PREFIX: &str = "bosun-shim-helper";
/// How long a sleeping helper lives: far beyond any test, short enough that a leak ends.
const SLEEP: Duration = Duration::from_secs(60);
/// The code the `speak` role exits with.
const SPOKEN_CODE: i32 = 7;
/// How long a test waits for the shim to get somewhere it should get at once.
const PATIENCE: Duration = Duration::from_secs(30);

/// The spec of a helper with `role`, in `dir`, with the wall-clock cap `cap`.
fn spec(dir: &Path, role: &str, cap: Duration) -> Spec {
    let mut env = vec![(OsString::from(ROLE), OsString::from(role))];
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        env.push(("SystemRoot".into(), root));
    }
    Spec {
        program: std::env::current_exe().expect("current exe"),
        args: ["run_role", "--exact", "--nocapture", "--quiet"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        env,
        cwd: dir.to_path_buf(),
        output_dir: dir.join("output"),
        wall_clock: cap,
    }
}

/// Writes `spec` to `dir` and starts a shim on it; returns the shim and the record's path.
fn start_shim(dir: &Path, spec: &Spec) -> (Child, PathBuf) {
    let path = dir.join("session.spec");
    spec.write(&path).expect("write the spec");
    let shim = Command::new(env!("CARGO_BIN_EXE_bosun-shim"))
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the shim");
    (shim, record_path(&path))
}

/// Polls until `found` returns a value, for at most [`PATIENCE`].
fn poll<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(Instant::now() < deadline, "no {what} within {PATIENCE:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The `bosun-shim-helper` lines of the file at `path`, by key; empty while it is missing.
fn reports(path: &Path) -> HashMap<String, String> {
    parse(&std::fs::read_to_string(path).unwrap_or_default())
}

/// The `bosun-shim-helper` lines of `text`, by key.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(PREFIX)?.strip_prefix(' ')?;
            let (key, value) = rest.split_once(' ')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Polls until none of `keys` runs, for at most `limit`. Returns those still running.
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

/// The shim is killed without warning: within 5 s neither its child nor the child's own child
/// is left, by pid and start time.
#[test]
fn a_killed_shim_leaves_no_descendant_within_5_s() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut shim, record) = start_shim(dir.path(), &spec(dir.path(), "parent", SLEEP * 2));
    let record = poll("record", || Record::read(&record).ok());
    let grandchild: u32 = poll("grandchild report", || {
        reports(&record.stdout).remove("grandchild")
    })
    .parse()
    .expect("pid");
    let grandchild = ProcessKey {
        pid: grandchild,
        start: start_time(grandchild).expect("start time").expect("live"),
    };
    assert!(is_running(record.child), "the child runs before the kill");
    assert_eq!(record.shim.pid, shim.id());

    shim.kill().expect("kill the shim");
    shim.wait().expect("wait");
    let left = survivors(&[record.child, grandchild], Duration::from_secs(5));
    for &key in &left {
        reap(key).ok();
    }
    assert!(left.is_empty(), "still running: {left:?}");
}

/// Nobody waits on the shim; when its child has exited, the record holds the child's code and
/// the output files hold what it wrote, each stream in its own.
#[test]
fn the_exit_of_a_child_nobody_waited_for_is_in_the_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut shim, record) = start_shim(dir.path(), &spec(dir.path(), "speak", SLEEP));
    let exited = poll("exit in the record", || {
        Record::read(&record).ok().filter(|r| r.exit.is_some())
    });
    // Reaped only now, after the check: the shim is never waited on before.
    shim.wait().ok();
    assert_eq!(exited.exit, Some(Exit::Code(SPOKEN_CODE)));
    assert!(!exited.capped);
    let stdout = reports(&exited.stdout);
    let stderr = reports(&exited.stderr);
    assert_eq!(stdout.get("out").map(String::as_str), Some("line"));
    assert_eq!(stderr.get("err").map(String::as_str), Some("line"));
    assert!(!stdout.contains_key("err"), "stderr reached stdout");
}

/// The wall-clock cap holds with only the shim to enforce it: the child is killed, the record
/// says so, and the shim exits with the killed child's code.
#[test]
fn the_wall_clock_cap_kills_the_child_and_is_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    let (mut shim, record) = start_shim(
        dir.path(),
        &spec(dir.path(), "sleep", Duration::from_secs(1)),
    );
    let status = shim.wait().expect("wait");
    let elapsed = started.elapsed();
    let record = Record::read(&record).expect("record");
    let left = survivors(&[record.child], Duration::from_secs(5));
    for &key in &left {
        reap(key).ok();
    }
    assert!(left.is_empty(), "the capped child still runs");
    assert!(record.capped, "{record:?}");
    let exit = record.exit.expect("the exit is recorded");
    assert_eq!(status.code(), Some(exit.shim_code()));
    assert!(
        elapsed < SLEEP / 2,
        "the cap did not cut the sleep: {elapsed:?}"
    );
}

/// A bad spec: the shim reports and exits with its own failure code, and starts nothing.
#[test]
fn a_shim_without_a_readable_spec_fails_with_its_own_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.spec");
    let status = Command::new(env!("CARGO_BIN_EXE_bosun-shim"))
        .arg(&missing)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run the shim");
    assert_eq!(status.code(), Some(125));
    assert!(!record_path(&missing).exists());
}

/// The environment of a helper with `role`: the role, and on Windows `SystemRoot`.
fn role_env(role: &str) -> Vec<(OsString, OsString)> {
    let mut env = vec![(OsString::from(ROLE), OsString::from(role))];
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.push(("SystemRoot".into(), root));
    }
    env
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// A holder process spawns a session through `ShimSpawner`, reads its first line and exits
/// without a kill. The session outlives it; a new `attach` from the record alone reads the
/// rest of the output from the holder's offset on and the exit. One session, one exit: the
/// child that ran is the recorded one, it started once, and the exit seen is the one recorded.
#[test]
fn a_session_survives_its_holder_and_is_reattached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = Command::new(std::env::current_exe().expect("current exe"))
        .args(["run_role", "--exact", "--nocapture", "--quiet"])
        .env_clear()
        .envs(role_env("holder"))
        .env(DIR, dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run the holder");
    assert!(status.success(), "the holder failed: {status}");
    let held = std::fs::read_to_string(dir.path().join("held")).expect("the holder's report");
    let (path, offset) = held.trim_end().split_once('\n').expect("two lines");
    let path = PathBuf::from(path);
    let offset: u64 = offset.parse().expect("offset");

    let before = Record::read(&path).expect("record");
    let survived = before.exit.is_none() && is_running(before.child);
    if !survived {
        reap(before.child).ok();
    }
    assert!(survived, "the session ended with its holder: {before:?}");

    std::fs::write(dir.path().join("go"), "").expect("let the child go on");
    let (stdout, stderr, status, key) = runtime().block_on(async {
        use tokio::io::AsyncReadExt;

        let mut spawned = attach(
            &path,
            Offsets {
                stdout: offset,
                stderr: 0,
            },
        )
        .expect("attach");
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let (out, err) = tokio::join!(
            spawned.stdout.read_to_string(&mut stdout),
            spawned.stderr.read_to_string(&mut stderr),
        );
        out.expect("stdout");
        err.expect("stderr");
        let status = spawned.tree.wait().await.expect("wait");
        (stdout, stderr, status, spawned.tree.key())
    });
    let after = Record::read(&path).expect("record");
    let left = survivors(&[after.shim, after.child], Duration::from_secs(5));
    for &key in &left {
        reap(key).ok();
    }

    assert_eq!(key, before.child);
    assert_eq!(after.child, before.child, "another child ran");
    assert_eq!(status.code(), Some(TALKED_CODE));
    assert_eq!(after.exit, Some(Exit::Code(TALKED_CODE)));
    let rest = parse(&stdout);
    assert!(
        rest.contains_key("second"),
        "the rest of stdout: {stdout:?}"
    );
    assert!(
        !rest.contains_key("first"),
        "read again before the offset: {stdout:?}"
    );
    assert!(parse(&stderr).contains_key("err"), "stderr: {stderr:?}");
    let whole = std::fs::read_to_string(&after.stdout).expect("stdout file");
    let starts = whole.lines().filter(|l| l.contains(" first ")).count();
    assert_eq!(starts, 1, "the child started {starts} times: {whole:?}");
    assert!(left.is_empty(), "still running after the exit: {left:?}");
}

/// A record without an exit whose shim pid is live under another start time is refused; the
/// same record with the live process's own start time is attached.
#[test]
fn attach_refuses_a_record_whose_pid_changed_hands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("output");
    std::fs::create_dir(&output).expect("output dir");
    let (stdout, stderr) = (output.join("stdout"), output.join("stderr"));
    std::fs::write(&stdout, "").expect("stdout");
    std::fs::write(&stderr, "").expect("stderr");
    // A live process stands in for the shim and the child: this test.
    let pid = std::process::id();
    let live = ProcessKey {
        pid,
        start: start_time(pid).expect("start time").expect("live"),
    };
    let mut record = Record {
        version: RECORD_VERSION,
        shim: live,
        child: live,
        stdout,
        stderr,
        exit: None,
        capped: false,
    };
    let path = dir.path().join("session.spec.record");
    record.write(&path).expect("write the record");
    // Dropping an attached session kills nothing.
    let attached = attach(&path, Offsets::default());
    assert!(attached.is_ok(), "the live shim's record: {attached:?}");
    drop(attached);

    record.shim.start = StartTime::from_raw(live.start.raw() ^ 1);
    record.write(&path).expect("write the record");
    let refused = attach(&path, Offsets::default());
    assert!(
        refused.is_err(),
        "attached through a pid that changed hands"
    );
}

/// The `holder` role: spawns a `talk` session through `ShimSpawner` in [`DIR`], reads its
/// stdout up to the first line, writes the record's path and the bytes read to `held`, and
/// drops everything without a kill.
fn hold() {
    use tokio::io::AsyncReadExt;

    let dir = PathBuf::from(std::env::var_os(DIR).expect("the holder's directory"));
    let shim = ShimSpawner::new(
        env!("CARGO_BIN_EXE_bosun-shim"),
        dir.join("sessions"),
        SLEEP,
    );
    let child = ChildSpec {
        program: std::env::current_exe().expect("current exe"),
        args: ["run_role", "--exact", "--nocapture", "--quiet"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        env: role_env("talk"),
        cwd: dir.clone(),
        stdin: false,
    };
    let session = dir.join("session");
    runtime().block_on(async {
        let mut spawned = shim.spawn_in(&child, &session).expect("spawn");
        let mut seen = Vec::new();
        while !parse(&String::from_utf8_lossy(&seen)).contains_key("first") {
            let mut chunk = [0_u8; 256];
            let read = spawned.stdout.read(&mut chunk).await.expect("read");
            assert!(read > 0, "stdout ended before the first line");
            seen.extend_from_slice(&chunk[..read]);
        }
        let record = session_record(&session);
        std::fs::write(
            dir.join("held"),
            format!("{}\n{}\n", record.display(), seen.len()),
        )
        .expect("report");
        drop(spawned);
    });
}

/// The helper's entry point: a no-op unless the role variable selects a role.
#[test]
fn run_role() {
    use std::io::Write;

    let Some(role) = std::env::var(ROLE).ok() else {
        return;
    };
    let report = |out: &mut dyn Write, key: &str, value: &str| {
        writeln!(out, "{PREFIX} {key} {value}").expect("report");
        out.flush().expect("flush");
    };
    match role.as_str() {
        "sleep" => std::thread::sleep(SLEEP),
        "speak" => {
            report(&mut std::io::stdout(), "out", "line");
            report(&mut std::io::stderr(), "err", "line");
            std::process::exit(SPOKEN_CODE);
        }
        "holder" => hold(),
        // Says its first line, waits for `go` in its directory, says the rest and exits.
        "talk" => {
            report(&mut std::io::stdout(), "first", "line");
            let deadline = Instant::now() + SLEEP;
            while !Path::new("go").exists() {
                assert!(Instant::now() < deadline, "no go within {SLEEP:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
            report(&mut std::io::stdout(), "second", "line");
            report(&mut std::io::stderr(), "err", "line");
            std::process::exit(TALKED_CODE);
        }
        // Starts a sleeping grandchild, reports its pid, sleeps.
        "parent" => {
            let mut command = Command::new(std::env::current_exe().expect("current exe"));
            command
                .args(["run_role", "--exact", "--nocapture", "--quiet"])
                .env_clear()
                .env(ROLE, "sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(root) = std::env::var_os("SystemRoot") {
                command.env("SystemRoot", root);
            }
            #[expect(
                clippy::zombie_processes,
                reason = "the grandchild must outlive its parent; its new parent reaps it"
            )]
            let grandchild = command.spawn().expect("spawn the grandchild");
            report(
                &mut std::io::stdout(),
                "grandchild",
                &grandchild.id().to_string(),
            );
            std::thread::sleep(SLEEP);
        }
        other => panic!("an unknown role: {other}"),
    }
}
