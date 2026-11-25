//! [`processes_named`] as the process sweep of #8 uses it: the test executable plays the
//! helper (re-invoked with one test selected, [`run_role`], and the role in an environment
//! variable, as `tests/shim.rs` does), under its own name and as a copy under a unique random
//! name in a temporary directory. Only the copy's name is ever listed; only pids this test
//! spawned are ever killed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bosun_proc::{ProcessKey, processes_named, start_time};

const ROLE: &str = "BOSUN_NAMED_HELPER";
/// How long a waiting helper lives: far beyond the test, short enough that a leak ends.
const WAIT: Duration = Duration::from_secs(60);
/// How far a listed start may lie from the wall clock around the spawn.
const SLACK_MS: i64 = 3_000;

/// A unique name of at most 15 characters: `bn` and 13 hex digits of the clock, the pid and a
/// counter, mixed.
fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    let mixed =
        (nanos ^ (u128::from(std::process::id()) << 64)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    #[expect(clippy::cast_possible_truncation, reason = "13 hex digits are kept")]
    let low = (mixed >> 32) as u64 & 0x000f_ffff_ffff_ffff;
    format!("bn{low:013x}")
}

/// Copies the test executable into `dir` under `name` (with the platform's suffix).
fn copy_of_self(dir: &Path, name: &str) -> PathBuf {
    let copy = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().expect("current exe"), &copy).expect("copy");
    copy
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis(),
    )
    .expect("in range")
}

/// Starts `program` as a waiting helper; returns it with the wall clock just before the spawn.
fn wait_helper(program: &Path) -> (Child, i64) {
    let mut command = Command::new(program);
    command
        .args(["run_role", "--exact", "--nocapture", "--quiet"])
        .env(ROLE, "wait")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let before = now_ms();
    (command.spawn().expect("spawn a helper"), before)
}

/// The keys of the processes named `name`, polled until there are `count` of them, at most
/// 10 s: a process just spawned may not be listed at once.
fn listed(name: &str, count: usize) -> Vec<bosun_proc::Named> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = processes_named(name).expect("list");
        if found.len() >= count || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Two helpers started from a copy under a unique name and one from the test's own name: the
/// copy's name lists exactly the two, each with the key `start_time` reports and a start
/// within a few seconds of the wall clock at its spawn.
#[test]
fn processes_named_finds_a_copy_of_the_helper_by_its_name_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let name = unique_name();
    let copy = copy_of_self(dir.path(), &name);
    let mut children = Vec::new();
    let mut spawned_at = Vec::new();
    for program in [
        copy.as_path(),
        copy.as_path(),
        &std::env::current_exe().expect("exe"),
    ] {
        let (child, before) = wait_helper(program);
        spawned_at.push((child.id(), before, now_ms()));
        children.push(child);
    }
    let found = listed(&name, 2);
    // A second listing under the platform's own spelling: on Windows the name matches with or
    // without `.exe`, case-insensitively.
    let spelled = cfg!(windows)
        .then(|| processes_named(&format!("{}.EXE", name.to_uppercase())).expect("list"));
    let keys: Vec<Option<ProcessKey>> = children
        .iter()
        .map(|c| {
            start_time(c.id())
                .expect("start time")
                .map(|start| ProcessKey { pid: c.id(), start })
        })
        .collect();
    for child in &mut children {
        child.kill().ok();
        child.wait().ok();
    }

    let mut expected: Vec<ProcessKey> = keys[..2].iter().map(|k| k.expect("live")).collect();
    expected.sort_by_key(|k| k.pid);
    let mut got: Vec<ProcessKey> = found.iter().map(|n| n.key).collect();
    got.sort_by_key(|k| k.pid);
    assert_eq!(got, expected, "listed: {found:?}");
    for named in &found {
        let &(_, before, after) = spawned_at
            .iter()
            .find(|(pid, _, _)| *pid == named.key.pid)
            .expect("one of ours");
        assert!(
            named.started_unix_ms >= before - SLACK_MS && named.started_unix_ms <= after + SLACK_MS,
            "{named:?} started outside {before}..{after}"
        );
    }
    if let Some(spelled) = spelled {
        let mut spelled: Vec<ProcessKey> = spelled.iter().map(|n| n.key).collect();
        spelled.sort_by_key(|k| k.pid);
        assert_eq!(spelled, expected);
    }
}

/// The helper's entry point: a no-op unless the role variable selects a role.
#[test]
fn run_role() {
    let Some(role) = std::env::var(ROLE).ok() else {
        return;
    };
    match role.as_str() {
        "wait" => std::thread::sleep(WAIT),
        other => panic!("an unknown role: {other}"),
    }
}
