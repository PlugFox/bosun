//! `fake-agent` runs its script: the stream fixture on stdout, the git actions in its cwd, the
//! exit code.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// `program` in `dir`, its git seeing neither the user's nor the system's configuration.
fn isolated(program: &str, dir: &Path, empty_config: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .env_remove("FAKE_AGENT_SCRIPT")
        .env("GIT_CONFIG_GLOBAL", empty_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fake Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Fake Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com");
    command
}

fn git(dir: &Path, empty_config: &Path) -> Command {
    isolated("git", dir, empty_config)
}

fn run(script: &Path, repo: &Path, empty_config: &Path, stdin: &str) -> Output {
    let mut child = isolated(env!("CARGO_BIN_EXE_fake-agent"), repo, empty_config)
        .args(["-p", "--verbose", "--fake-script"])
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_script_replays_the_stream_commits_and_exits_with_its_code() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    assert!(
        git(&repo, &empty_config)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude-stream/normal-end.jsonl")
        .canonicalize()
        .unwrap();
    let script = scratch.path().join("script.txt");
    std::fs::write(
        &script,
        format!(
            "# a session that commits one file and leaves one untracked\n\
             stdin prompt.txt\n\
             argv argv.txt\n\
             replay {}\n\
             commit src/lib.rs Add the flag\n\
             write notes/untracked.txt left behind\n\
             eprint working\n\
             sleep 10\n\
             exit 3\n\
             print never reached\n",
            fixture.display()
        ),
    )
    .unwrap();

    let output = run(&script, &repo, &empty_config, "the prompt");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        std::fs::read_to_string(&fixture).unwrap()
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), "working");

    let log = git(&repo, &empty_config)
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(log.stdout).unwrap().trim(),
        "Add the flag"
    );
    let status = git(&repo, &empty_config)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("?? notes/untracked.txt"), "{status}");
    assert!(!status.contains("src/lib.rs"), "{status}");

    assert_eq!(
        std::fs::read_to_string(repo.join("prompt.txt")).unwrap(),
        "the prompt"
    );
    let argv = std::fs::read_to_string(repo.join("argv.txt")).unwrap();
    assert!(argv.starts_with("-p\n--verbose\n--fake-script\n"), "{argv}");
}

#[test]
fn a_script_error_exits_with_125() {
    let scratch = tempfile::tempdir().unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let script = scratch.path().join("script.txt");
    std::fs::write(&script, "print before\nfrobnicate now\n").unwrap();
    let output = run(&script, scratch.path(), &empty_config, "");
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "before\n");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unknown command")
    );
}

/// A copy of `fake-agent` named like another tool reads the script named like it (#70).
#[test]
fn a_copy_reads_the_script_next_to_it() {
    let scratch = tempfile::tempdir().unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let copy = scratch
        .path()
        .join(format!("gh{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-agent"), &copy).unwrap();
    std::fs::write(
        scratch.path().join("gh.script"),
        "print synthetic-login
",
    )
    .unwrap();
    let output = isolated(copy.to_str().unwrap(), scratch.path(), &empty_config)
        .args(["api", "graphql", "--jq", ".data.viewer.login"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "synthetic-login
"
    );
}

#[test]
fn the_script_dir_picks_the_items_script_and_wait_waits_for_its_file() {
    let scratch = tempfile::tempdir().unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let scripts = scratch.path().join("scripts");
    std::fs::create_dir(&scripts).unwrap();
    let release = scratch.path().join("release");
    std::fs::write(
        scripts.join("demo_7.script"),
        format!("wait {}\nprint item seven\n", release.display()),
    )
    .unwrap();
    let mut child = isolated(
        env!("CARGO_BIN_EXE_fake-agent"),
        scratch.path(),
        &empty_config,
    );
    let child = child
        .arg("--fake-script-dir")
        .arg(&scripts)
        .env("BOSUN_ITEM", "demo#7")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut child = child;
    assert!(child.try_wait().unwrap().is_none(), "it waits for the file");
    std::fs::write(&release, "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "item seven");
}

#[test]
fn once_runs_its_command_only_the_first_time() {
    let scratch = tempfile::tempdir().unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let (printed, exited) = (scratch.path().join("m1"), scratch.path().join("m2"));
    let script = scratch.path().join("script.txt");
    std::fs::write(
        &script,
        format!(
            "once {} print first\nonce {} exit 3\nprint later\n",
            printed.display(),
            exited.display()
        ),
    )
    .unwrap();
    let first = run(&script, scratch.path(), &empty_config, "");
    assert_eq!(first.status.code(), Some(3));
    assert_eq!(String::from_utf8(first.stdout).unwrap(), "first\n");
    assert!(printed.exists() && exited.exists());
    let second = run(&script, scratch.path(), &empty_config, "");
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(String::from_utf8(second.stdout).unwrap(), "later\n");
}

#[test]
fn a_server_of_the_mcp_configuration_that_cannot_start_is_left_out() {
    let scratch = tempfile::tempdir().unwrap();
    let empty_config = scratch.path().join("gitconfig");
    std::fs::write(&empty_config, "").unwrap();
    let mcp = scratch.path().join("mcp.json");
    let missing = scratch.path().join("no-such-server");
    std::fs::write(
        &mcp,
        serde_json::json!({"mcpServers": {"bosun": {"command": missing, "args": []}}}).to_string(),
    )
    .unwrap();
    let script = scratch.path().join("script.txt");
    std::fs::write(&script, "print ran\nmcp bosun note {\"text\": \"x\"}\n").unwrap();
    let output = isolated(
        env!("CARGO_BIN_EXE_fake-agent"),
        scratch.path(),
        &empty_config,
    )
    .arg("--fake-script")
    .arg(&script)
    .arg("--mcp-config")
    .arg(&mcp)
    .stdin(Stdio::null())
    .output()
    .unwrap();
    // The script runs without the server, and a call to it is a script error.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran\n");
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot start"), "{stderr}");
    assert!(stderr.contains("no connected MCP server"), "{stderr}");
}
