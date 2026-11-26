//! A daemon whose hosting console has ended (#131): the logon task hosts `bosund` in a
//! `conhost.exe --headless`, and whatever ends that `conhost` leaves the daemon without its
//! console. Its log, its git and its board reads must still work.
//!
//! The helper pattern of `bosun-proc`'s tests: the test executable is re-invoked with one test
//! selected, [`orphan_role`], under a `conhost` this test starts and then ends; the role's
//! directory is in `BOSUN_CONSOLE_HELPER`. The helper reports through files in that directory,
//! never its own output, which is the console that is gone.

use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use bosun_queue::projects_v2::{GhCli, GraphQl as _};

const ROLE: &str = "BOSUN_CONSOLE_HELPER";
/// `CREATE_NO_WINDOW`: the host gets a hidden console of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Waits up to `limit` for `path` to exist.
fn wait_for(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while !path.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// The helper: nothing unless `BOSUN_CONSOLE_HELPER` names a directory. Then it reports
/// `ready`, waits for `go` (its host is gone by then), logs a line, runs `git --version` and
/// reads a board through the `gh.cmd` stand-in, and writes what it saw to `results`.
#[test]
fn orphan_role() {
    let Some(dir) = std::env::var_os(ROLE).map(PathBuf::from) else {
        return;
    };
    std::fs::write(dir.join("ready"), "").expect("ready");
    if !wait_for(&dir.join("go"), Duration::from_mins(1)) {
        return;
    }

    let log_file = dir.join("bosund.log");
    let line = "bosund: after the host";
    let _ = crate::log::init(&log_file);
    crate::log::line(line);
    let logged = std::fs::read_to_string(&log_file).is_ok_and(|text| text.contains(line));

    let git = match bosun_git::Git::new(&dir)
        .command()
        .arg("--version")
        .output()
    {
        Ok(out) => out
            .status
            .code()
            .map_or_else(|| "no code".to_owned(), |c| c.to_string()),
        Err(e) => format!("cannot start: {e}"),
    };

    let env = ["SystemRoot", "PATH"]
        .into_iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (k, v)));
    let board = match GhCli::new(dir.join("gh.cmd"), env)
        .execute("query{viewer{login}}", &serde_json::json!({}))
    {
        Ok(reply) => reply.data["viewer"]["login"]
            .as_str()
            .map_or_else(|| format!("no login: {}", reply.data), str::to_owned),
        Err(e) => format!("error: {e}"),
    };

    let results = format!("log {logged}\ngit {git}\nboard {board}\n");
    std::fs::write(dir.join("results.tmp"), results).expect("results");
    std::fs::rename(dir.join("results.tmp"), dir.join("results")).expect("results");
}

#[test]
fn children_start_after_the_hosting_console_has_ended() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("gh.cmd"),
        "@echo off\r\necho HTTP/2.0 200 OK\r\necho.\r\necho {\"data\":{\"viewer\":{\"login\":\"demo\"}}}\r\n",
    )
    .unwrap();
    let root = std::env::var_os("SystemRoot").expect("SystemRoot");
    let conhost = Path::new(&root).join("System32").join("conhost.exe");
    // A headless `conhost` ends, and its console with it, when its input reaches end of file:
    // its standard input is a pipe this test holds open until it ends the host itself. Its
    // output (the helper's console, rendered) is discarded, so nothing the helper's libtest
    // prints mixes with this test's own output.
    let mut host = std::process::Command::new(conhost)
        .arg("--headless")
        .arg(std::env::current_exe().unwrap())
        .args([
            "console_tests::orphan_role",
            "--exact",
            "--nocapture",
            "--quiet",
        ])
        .env(ROLE, dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A hidden console of the host's own, never this test's.
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("conhost starts");

    let ready = wait_for(&dir.path().join("ready"), Duration::from_secs(30));
    // Ends the host by the handle this test holds; the helper lives on without its console.
    host.kill().ok();
    host.wait().ok();
    assert!(ready, "the helper did not start under conhost");

    std::fs::write(dir.path().join("go"), "").unwrap();
    let results_path = dir.path().join("results");
    assert!(
        wait_for(&results_path, Duration::from_mins(1)),
        "the helper wrote no results"
    );
    let results = std::fs::read_to_string(&results_path).unwrap();
    for expected in ["log true", "git 0", "board demo"] {
        assert!(
            results.lines().any(|l| l == expected),
            "expected `{expected}`:\n{results}"
        );
    }
}
