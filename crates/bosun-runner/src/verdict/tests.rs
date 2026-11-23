//! Tests of the verdict executor. The verdicts run through a real POSIX shell (`/bin/sh`, or
//! Git's `sh.exe` on Windows, as D4 resolves it); a descendant whose pid the tests must know is
//! this test executable re-invoked as [`helper`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bosun_proc::{NativeSpawner, ProcessKey, StartTime, is_running};

use super::*;

const ROLE: &str = "BOSUN_RUNNER_VERDICT_HELPER";
const PREFIX: &str = "bosun-runner-helper";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// The POSIX shell of the tests: `/bin/sh`, or on Windows the `sh.exe` of the Git installation
/// that `git` on `PATH` belongs to (its exec path is `<root>/mingw64/libexec/git-core`).
fn shell() -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from("/bin/sh");
    }
    let out = std::process::Command::new("git")
        .arg("--exec-path")
        .output()
        .expect("git is installed");
    let exec = PathBuf::from(String::from_utf8(out.stdout).expect("utf-8").trim());
    exec.ancestors()
        .map(|root| root.join("bin").join("sh.exe"))
        .find(|p| p.is_file())
        .expect("Git's sh.exe")
}

/// `PATH` for the shell's tools, and on Windows `SystemRoot`; nothing else of this process.
fn env() -> Vec<(OsString, OsString)> {
    ["PATH", "SystemRoot"]
        .into_iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (k.into(), v)))
        .collect()
}

fn spec(command: &str, timeout: Duration) -> VerdictSpec {
    VerdictSpec {
        shell: shell(),
        command: command.to_owned(),
        env: env(),
        cwd: std::env::temp_dir(),
        timeout,
        limits: Limits::default(),
    }
}

/// Runs `spec`, failing the test if the run takes longer than `bound`.
fn run_within(spec: &VerdictSpec, bound: Duration) -> VerdictRun {
    runtime().block_on(async {
        tokio::time::timeout(bound, run(&NativeSpawner, spec))
            .await
            .unwrap_or_else(|_| panic!("the verdict run outlived the test's bound of {bound:?}"))
            .expect("run")
    })
}

const BOUND: Duration = Duration::from_secs(30);

/// The device-tool case of #44 B: the verdict's match on stdout, then a progress line on
/// stderr. The card quotes the match.
#[test]
fn a_stderr_line_after_stdouts_match_reports_the_stdout_line() {
    let run = run_within(
        &spec(
            "echo 'needle found'; sleep 0.3; echo '1 file pushed, 0 skipped' >&2",
            BOUND,
        ),
        BOUND,
    );
    assert_eq!(run.outcome, Outcome::Pass);
    assert_eq!(run.exit.code, Some(0));
    // The fixture is what it claims: in arrival order the stderr line is last.
    assert_eq!(
        run.interleaved.last_line(),
        Some("1 file pushed, 0 skipped")
    );
    assert_eq!(
        run.verdict_line(),
        Some(VerdictLine {
            stream: Stream::Stdout,
            text: "needle found".into(),
        })
    );
    assert_eq!(run.stderr.last_line(), Some("1 file pushed, 0 skipped"));
}

#[test]
fn a_verdict_that_prints_only_to_stderr_reports_the_stderr_line() {
    let run = run_within(
        &spec("printf '\\n  \\n'; echo 'only here' >&2; exit 3", BOUND),
        BOUND,
    );
    assert_eq!(run.outcome, Outcome::Fail);
    assert_eq!(run.exit.code, Some(3));
    assert_eq!(
        run.verdict_line(),
        Some(VerdictLine {
            stream: Stream::Stderr,
            text: "only here".into(),
        })
    );
}

#[test]
fn a_silent_verdict_has_no_line() {
    let run = run_within(&spec("true", BOUND), BOUND);
    assert_eq!(run.outcome, Outcome::Pass);
    assert_eq!(run.verdict_line(), None);
}

/// Single-quotes `path` for the shell.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// A verdict that sleeps past its timeout, with a descendant (this executable, sleeping) in the
/// background: `timeout`, within the test's own bound, and neither survives.
#[test]
fn a_verdict_past_its_timeout_is_timeout_and_no_descendant_survives() {
    let exe = std::env::current_exe().expect("current exe");
    let command = format!(
        "{ROLE}=sleep {} verdict::tests::helper --exact --nocapture --quiet & sleep 60",
        quoted(&exe)
    );
    let timeout = Duration::from_secs(3);
    let started = Instant::now();
    let run = run_within(&spec(&command, timeout), Duration::from_secs(20));
    assert_eq!(run.outcome, Outcome::Timeout);
    assert_eq!(run.exit.code, None);
    assert!(run.duration >= timeout, "{:?}", run.duration);
    assert!(started.elapsed() < Duration::from_secs(20));

    let descendant = run
        .stdout
        .text
        .lines()
        .find_map(|l| l.strip_prefix(PREFIX)?.trim().split_once(' '))
        .map_or_else(
            || panic!("the descendant did not report: {:?}", run.stdout.text),
            |(pid, start)| ProcessKey {
                pid: pid.parse().expect("pid"),
                start: StartTime::from_raw(start.parse().expect("start")),
            },
        );
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_running(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = is_running(descendant);
    if survived {
        bosun_proc::reap(descendant).ok();
    }
    assert!(
        !survived,
        "the descendant {descendant:?} survived the timeout"
    );
}

/// 50 MB of output, stderr first: the executor drains both pipes at once (reading stdout alone
/// would leave the child blocked on a full stderr pipe until the test's bound), and keeps only
/// the bounded tails.
#[test]
fn fifty_megabytes_keep_a_bounded_tail_and_do_not_block_the_child() {
    const HALF: u64 = 25 * 1024 * 1024;
    let command = format!(
        "yes 'stderr filler line' | head -c {HALF} >&2; \
         yes 'stdout filler line' | head -c {HALF}; echo; echo 'all done'"
    );
    let spec = spec(&command, Duration::from_secs(60));
    let run = run_within(&spec, Duration::from_secs(60));
    assert_eq!(run.outcome, Outcome::Pass, "{:?}", run.stderr.last_line());
    assert_eq!(run.stderr.total_bytes, HALF);
    assert_eq!(run.stdout.total_bytes, HALF + "\nall done\n".len() as u64);
    assert_eq!(run.interleaved.total_bytes, 2 * HALF + 10);
    for tail in [&run.stdout, &run.stderr] {
        assert!(tail.truncated());
        assert!(tail.text.len() <= spec.limits.stream, "{}", tail.text.len());
    }
    assert!(run.interleaved.text.len() <= spec.limits.interleaved);
    // `head -c` cuts the last filler line short.
    let last = run.stderr.last_line().expect("a stderr line");
    assert!("stderr filler line".starts_with(last), "{last:?}");
    assert_eq!(
        run.verdict_line().map(|l| l.text),
        Some("all done".to_owned())
    );
    // A dropped front never leaves a partial line.
    assert!(run.stdout.text.starts_with("stdout filler line\n"));
}

#[test]
fn the_tail_keeps_the_last_bytes_from_a_line_start() {
    let mut tail = ByteTail::new(8);
    for _ in 0..100_000 {
        tail.push(b"abc\n");
    }
    tail.push(b"last\n");
    let tail = tail.finish();
    assert_eq!(tail.total_bytes, 400_005);
    assert_eq!(tail.text, "last\n");
    assert!(tail.truncated());
}

/// Synthetic libtest output: three failures, one reported only by its result line, one only by
/// its stdout section, one by both; passing tests, the summary and the failures list are not
/// names.
const LIBTEST: &str = "\
running 6 tests
test parse::tests::accepts_empty ... ok
test store::tests::writes_in_order ... FAILED
test heat::tests::shared_flag - should panic ... FAILED
test merge::tests::ok_one ... ok
test merge::tests::ignored_one ... ignored

failures:

---- store::tests::writes_in_order stdout ----
thread 'store::tests::writes_in_order' panicked at src/store.rs:10:5:
assertion failed: ok
---- debug::tests::global_flag stdout ----
thread 'debug::tests::global_flag' panicked at src/debug.rs:3:1:
test something ... else

failures:
    debug::tests::global_flag
    heat::tests::shared_flag
    store::tests::writes_in_order

test result: FAILED. 2 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out
";

#[test]
fn libtest_output_with_three_failures_yields_the_three_names() {
    assert_eq!(
        Libtest.failing_tests(LIBTEST),
        [
            "store::tests::writes_in_order",
            "heat::tests::shared_flag",
            "debug::tests::global_flag",
        ]
    );
    assert_eq!(
        Libtest.failing_tests("test result: ok. 3 passed"),
        Vec::<String>::new()
    );
}

#[test]
fn a_failed_run_names_its_tests_from_both_streams() {
    let run = run_within(
        &spec(
            "echo 'test a::one ... FAILED'; echo '---- a::one stdout ----'; \
             echo '---- b::two stdout ----' >&2; exit 101",
            BOUND,
        ),
        BOUND,
    );
    assert_eq!(run.outcome, Outcome::Fail);
    assert_eq!(run.failing_tests(&Libtest), ["a::one", "b::two"]);
}

/// The descendant of the timeout test, selected by the role variable: reports its pid and start
/// time on stdout, then sleeps. A no-op in a normal test run.
#[test]
fn helper() {
    if std::env::var_os(ROLE).is_none() {
        return;
    }
    let pid = std::process::id();
    let start = bosun_proc::start_time(pid)
        .expect("start time")
        .expect("running");
    println!("{PREFIX} {pid} {}", start.raw());
    std::thread::sleep(Duration::from_secs(60));
}
