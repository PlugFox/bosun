//! The `bosun` binary against a real `bosund` (#25): `recount --compare` over
//! `bosun tokens --by session --format json`, identities as separate sections, and exit codes
//! held to the help text. Each test runs its own daemon on its own endpoint, over copies of the
//! synthetic fixtures (#16) in a temporary directory, as `crates/bosund/tests/daemon.rs` does.
//!
//! `bosund` and `recount` are binaries of other packages, so the tests build them once with
//! `cargo build` into the target directory this test runs from; neither is linked.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bosun_client::{Client, Endpoint};
use serde_json::Value;
use tempfile::TempDir;

/// How long a daemon may take to start.
const PATIENCE: Duration = Duration::from_secs(30);

/// The directory `bosun` was built into; `bosund` and `recount` are built there too.
fn bin_dir() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_BIN_EXE_bosun"))
            .parent()
            .unwrap()
            .to_path_buf();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut build = Command::new(cargo);
        build
            .args([
                "build",
                "--locked",
                "--package",
                "bosund",
                "--package",
                "recount",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if dir.file_name().is_some_and(|n| n == "release") {
            build.arg("--release");
        }
        let status = build.status().unwrap();
        assert!(status.success(), "building bosund and recount failed");
        dir
    })
}

fn exe(name: &str) -> PathBuf {
    bin_dir().join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn fixture(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude")
        .join(case)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The repository roots of a fixture case that are absolute paths on this platform: the
/// fixtures name Unix roots and Windows roots, and the configuration takes absolute ones only.
fn roots(case: &str) -> Vec<String> {
    let text = std::fs::read_to_string(fixture(case).join("repos.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    value["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_owned())
        .filter(|r| Path::new(r).is_absolute())
        .collect()
}

fn unique_endpoint(dir: &Path) -> Endpoint {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    if cfg!(windows) {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Endpoint::new(format!("bosun-cli-test-{}-{n}", std::process::id()))
    } else {
        Endpoint::new(dir.join("bosund.sock"))
    }
}

/// A temporary home: identities whose transcripts are copies of fixture cases, the
/// repositories those cases name, a configuration, and an endpoint of its own.
struct Home {
    dir: TempDir,
    endpoint: Endpoint,
}

impl Home {
    /// `identities`: each identity's name and the fixture case its transcripts come from.
    fn new(identities: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = String::new();
        for (name, case) in identities {
            let claude = dir.path().join(name).join("claude");
            copy_dir(&fixture(case).join("projects"), &claude.join("projects"));
            let database = dir.path().join(name).join("bosun.db");
            let _ = write!(
                config,
                "[identity.{name}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
                claude.display(),
                database.display()
            );
            let roots = roots(case);
            for (i, root) in roots.iter().enumerate() {
                let _ = write!(
                    config,
                    "[repo.{name}_{i}]\npath = '{root}'\nidentity = '{name}'\n"
                );
            }
            // The same roots for recount, so that both sides attribute the same way.
            let repos = serde_json::json!({ "roots": roots });
            std::fs::write(dir.path().join(name).join("repos.json"), repos.to_string()).unwrap();
        }
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let endpoint = unique_endpoint(dir.path());
        Self { dir, endpoint }
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.dir.path().join(rest)
    }

    /// Starts a daemon and waits until it answers `hello`.
    async fn start(&self) -> Daemon {
        let mut child = Command::new(exe("bosund"))
            .env("BOSUN_CONFIG", self.path("config.toml"))
            .env("BOSUN_ENDPOINT", self.endpoint.path())
            .env("BOSUN_OPERATOR_FILE", self.path("operator.token"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("the daemon exited at start: {status}");
            }
            match Client::connect(&self.endpoint).await {
                Ok(_) => return Daemon { child },
                Err(e) if Instant::now() > deadline => panic!("the daemon never answered: {e}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    /// Runs `bosun` against this home's configuration and endpoint.
    fn bosun(&self, args: &[&str]) -> Output {
        bosun(&self.path("config.toml"), &self.endpoint, args)
    }

    /// Runs `bosun` and returns its stdout; panics unless it exits with 0.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.bosun(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// `recount --compare` of `document` with a recount of the identity's transcripts.
    fn recount_compare(&self, identity: &str, document: &str) -> Output {
        let file = self.path(&format!("{identity}.tokens.json"));
        std::fs::write(&file, document).unwrap();
        let identity_dir = self.path(identity);
        Command::new(exe("recount"))
            .arg("--repos")
            .arg(identity_dir.join("repos.json"))
            .arg("--compare")
            .arg(&file)
            .arg(identity_dir.join("claude"))
            .output()
            .unwrap()
    }

    /// Asserts that `recount --compare` finds no difference.
    fn assert_recount_equals(&self, identity: &str, document: &str) {
        let output = self.recount_compare(identity, document);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(0), "{identity}: {output:?}");
        assert!(stdout.contains("recount equals"), "{identity}: {stdout}");
    }
}

fn bosun(config: &Path, endpoint: &Endpoint, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bosun"))
        .args(args)
        .env("BOSUN_CONFIG", config)
        .env("BOSUN_ENDPOINT", endpoint.path())
        .env(
            "BOSUN_OPERATOR_FILE",
            config.with_file_name("operator.token"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// A running daemon; killed when dropped.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The sections of text output, by their `== <identity> ==` headers.
fn headers(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|l| l.strip_prefix("== ")?.strip_suffix(" =="))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_by_session_equals_recount() {
    // Identities from cases that between them hold sub-agents and a working directory that
    // changes repository. The fixtures name Unix roots in one case and Windows roots in the
    // other, so on every platform one identity has configured repositories.
    let cases = [("personal", "subagent-cost-state"), ("work", "cwd-change")];
    let home = Home::new(&cases);
    let _daemon = home.start().await;
    // A reingest answers when its pass is done, so the store holds every line after it.
    for (identity, _) in cases {
        home.ok(&["reingest", "--identity", identity]);
    }
    let mut attributed = 0;
    for (identity, _) in cases {
        let document = home.ok(&[
            "tokens",
            "--by",
            "session",
            "--format",
            "json",
            "--identity",
            identity,
        ]);
        let value: Value = serde_json::from_str(&document).unwrap();
        assert_eq!(value["version"], 1, "{document}");
        assert!(
            value["total"]["messages"].as_u64().unwrap() > 0,
            "{document}"
        );
        home.assert_recount_equals(identity, &document);
        attributed += value["repos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| !r["repo"].is_null())
            .count();

        // The check goes red on a document that differs.
        let mut wrong = value.clone();
        let output = wrong["total"]["output"].as_u64().unwrap();
        wrong["total"]["output"] = (output + 1).into();
        let compared = home.recount_compare(identity, &wrong.to_string());
        assert_eq!(compared.status.code(), Some(1), "{compared:?}");
    }
    assert!(attributed > 0, "no message was in a configured repository");
}

#[tokio::test(flavor = "multi_thread")]
async fn identities_are_separate_sections() {
    // At most one case whose roots are absolute here: two repositories may not share a path.
    let cases = [("personal", "cache-and-models"), ("work", "cwd-change")];
    let home = Home::new(&cases);
    let _daemon = home.start().await;

    // Reingest every identity, each in its section.
    let reingest = home.ok(&["reingest"]);
    assert_eq!(headers(&reingest), ["personal", "work"], "{reingest}");

    // JSON: an object keyed by identity whose values are each recount's document.
    let json = home.ok(&["tokens", "--by", "session", "--format", "json"]);
    let value: Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["personal", "work"], "{json}");
    for (identity, _) in cases {
        home.assert_recount_equals(identity, &value[identity].to_string());
    }

    // Text: one section per identity, each with its own total.
    for by in ["session", "repo", "model"] {
        let text = home.ok(&["tokens", "--by", by]);
        assert_eq!(headers(&text), ["personal", "work"], "{by}: {text}");
        let totals = text.lines().filter(|l| l.starts_with("total")).count();
        assert_eq!(totals, 2, "{by}: {text}");
        let json = home.ok(&["tokens", "--by", by, "--format", "json"]);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2, "{by}: {json}");
    }
    let status = home.ok(&["ingest", "status"]);
    assert_eq!(headers(&status), ["personal", "work"], "{status}");
    let status = home.ok(&["ingest", "status", "--format", "json"]);
    let value: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(value["work"]["identity"], "work", "{status}");
}

/// The exit code `bosun --help` documents for the line containing `meaning`.
fn documented(meaning: &str) -> i32 {
    let help = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let help = String::from_utf8(help.stdout).unwrap();
    let section = help
        .split_once("Exit codes:\n")
        .expect("the help documents exit codes")
        .1;
    let lines: Vec<&str> = section.lines().filter(|l| l.contains(meaning)).collect();
    assert_eq!(lines.len(), 1, "`{meaning}` in {section}");
    lines[0].split_whitespace().next().unwrap().parse().unwrap()
}

/// A fake daemon on `endpoint` that answers `hello` with `api_version` and every other
/// request with the error `code`. It serves connections until the test ends; call it on the
/// runtime.
#[cfg_attr(
    unix,
    expect(
        clippy::needless_pass_by_value,
        reason = "the Windows branch consumes the endpoint; the Unix one only borrows it"
    )
)]
fn fake_daemon(endpoint: Endpoint, api_version: u32, code: i32) {
    use bosun_api::Method as _;
    use bosun_api::rpc::{ErrorObject, Request, Response};
    use bosun_api::{HelloResult, method};
    use bosun_client::Stream;
    use bosun_client::framing::{read_line, write_message};

    async fn serve(stream: Box<dyn Stream>, api_version: u32, code: i32) {
        let (read, mut write) = tokio::io::split(stream);
        let mut read = tokio::io::BufReader::new(read);
        while let Ok(Some(line)) = read_line(&mut read).await {
            let request: Request = serde_json::from_str(&line).unwrap();
            let id = request.id.unwrap();
            let response = if request.method == method::Hello::NAME {
                let hello = HelloResult {
                    api_version,
                    daemon_version: "0.0.0".to_owned(),
                    pid: 1,
                };
                Response::result(id, serde_json::to_value(hello).unwrap())
            } else {
                Response::error(id, ErrorObject::new(code, "refused by the fake daemon"))
            };
            if write_message(&mut write, &response).await.is_err() {
                return;
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(endpoint.path())
            .unwrap();
        tokio::spawn(async move {
            loop {
                server.connect().await.unwrap();
                let next = ServerOptions::new().create(endpoint.path()).unwrap();
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(serve(Box::new(connected), api_version, code));
            }
        });
    }
    #[cfg(unix)]
    {
        let listener = tokio::net::UnixListener::bind(endpoint.path()).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(Box::new(stream), api_version, code));
            }
        });
    }
}

/// Runs `bosun` off the runtime's threads, so a fake daemon on them can answer it.
async fn run_bosun(config: PathBuf, endpoint: Endpoint, args: &'static [&'static str]) -> Output {
    tokio::task::spawn_blocking(move || bosun(&config, &endpoint, args))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_codes_match_the_help() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    let config = home.path("config.toml");
    let code = |output: &Output| output.status.code();

    // Bad arguments.
    let output = home.bosun(&["tokens", "--by", "day"]);
    assert_eq!(
        code(&output),
        Some(documented("bad arguments")),
        "{output:?}"
    );
    let output = home.bosun(&["tokens", "--since", "1d", "--until", "2d"]);
    assert_eq!(
        code(&output),
        Some(documented("bad arguments")),
        "{output:?}"
    );

    // The configuration: missing, and invalid.
    let missing = home.path("missing.toml");
    let output = bosun(&missing, &home.endpoint, &["ingest", "status"]);
    assert_eq!(
        code(&output),
        Some(documented("configuration")),
        "{output:?}"
    );
    let invalid = home.path("invalid.toml");
    std::fs::write(&invalid, "[identity.x]\nclaude_config_dir = 'relative'\n").unwrap();
    let output = bosun(&invalid, &home.endpoint, &["tokens"]);
    assert_eq!(
        code(&output),
        Some(documented("configuration")),
        "{output:?}"
    );

    // Nothing listens at the endpoint yet.
    for args in [
        &["ingest", "status"][..],
        &["tokens", "--identity", "personal"],
    ] {
        let output = home.bosun(args);
        assert_eq!(code(&output), Some(documented("not running")), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }

    {
        let _daemon = home.start().await;
        let output = home.bosun(&["ingest", "status"]);
        assert_eq!(code(&output), Some(documented("success")), "{output:?}");
        for args in [
            &["tokens", "--identity", "nobody"][..],
            &["reingest", "--identity", "nobody"],
            &[
                "ingest",
                "status",
                "--identity",
                "nobody",
                "--format",
                "json",
            ],
        ] {
            let output = home.bosun(args);
            assert_eq!(
                code(&output),
                Some(documented("identity is unknown")),
                "{output:?}"
            );
            assert!(output.stdout.is_empty(), "{output:?}");
        }
    }

    // A daemon of another API version, and one that refuses requests.
    let newer = unique_endpoint(&home.path("newer"));
    std::fs::create_dir_all(home.path("newer")).unwrap();
    fake_daemon(newer.clone(), bosun_api::API_VERSION + 1, 0);
    let output = run_bosun(config.clone(), newer, &["tokens"]).await;
    assert_eq!(code(&output), Some(documented("API version")), "{output:?}");

    let refusing = unique_endpoint(&home.path("refusing"));
    std::fs::create_dir_all(home.path("refusing")).unwrap();
    fake_daemon(
        refusing.clone(),
        bosun_api::API_VERSION,
        bosun_api::rpc::code::FAILED,
    );
    let output = run_bosun(config, refusing, &["reingest"]).await;
    assert_eq!(code(&output), Some(documented("failed")), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("bosun: "), "{stderr}");

    // Help and version succeed.
    for args in [&["--help"][..], &["--version"], &["tokens", "--help"]] {
        let output = home.bosun(args);
        assert_eq!(code(&output), Some(documented("success")), "{output:?}");
    }
}

/// Runs `git args` in `dir` with no user or system configuration, no hooks and no signing,
/// the commit dates set to `at` (Unix seconds) when given.
fn git(dir: &Path, at: Option<i64>, args: &[&str]) {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", &format!("core.hooksPath={null}")])
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "user.name=Test"])
        .args(["-c", "user.email=test@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(at) = at {
        let date = format!("@{at} +0000");
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// The titles of the Markdown documents of a report, one per identity.
fn titles(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|l| l.strip_prefix("# bosun report: "))
        .collect()
}

/// `bosun report` through a real daemon: the agreement over the sub-agent fixture, commits
/// read from a configured repository by the daemon's commit reader, `--repo`, Markdown, and
/// the usage errors of the command.
#[tokio::test(flavor = "multi_thread")]
async fn the_report_through_the_daemon() {
    let cases = [("personal", "subagent-cost-state"), ("work", "cwd-change")];
    let home = Home::new(&cases);
    // A repository of `personal` with one commit in the range (2000-01-03, the case's day)
    // and one on the day before.
    let repo = home.path("code");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, None, &["init", "--quiet", "--initial-branch=main"]);
    for (at, message) in [(946_771_200 + 3_600, "before"), (946_857_600 + 3_600, "in")] {
        git(
            &repo,
            Some(at),
            &["commit", "--quiet", "--allow-empty", "-m", message],
        );
    }
    let mut config = std::fs::read_to_string(home.path("config.toml")).unwrap();
    let _ = write!(
        config,
        "[repo.code]\npath = '{}'\nidentity = 'personal'\n",
        repo.display()
    );
    std::fs::write(home.path("config.toml"), config).unwrap();
    let _daemon = home.start().await;
    // A reingest answers when its pass and a commit scan are done.
    home.ok(&["reingest"]);

    let range = ["--since", "2000-01-03", "--until", "2000-01-04"];
    let json = home.ok(&[
        &["report", "--format", "json", "--identity", "personal"][..],
        &range,
    ]
    .concat());
    let document: Value = serde_json::from_str(&json).unwrap();
    // The exact part and the estimates, apart.
    let estimates = &document["estimates"];
    assert_eq!(estimates["estimate"], true, "{json}");
    assert_eq!(estimates["by_tool"]["estimate"], true, "{json}");
    let report = &document["exact"];
    assert_eq!(report["total"]["messages"], 4, "{json}");
    let agreement = &report["health"]["agreement"];
    assert_eq!(agreement["sessions"], 1, "{json}");
    assert_eq!(agreement["within"], 1, "{json}");
    let named = |list: &Value| -> Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|item| item["repo"]["name"] == "code")
            .cloned()
            .unwrap()
    };
    let scan = named(&report["per_commit"]["scans"]);
    assert_eq!(scan["state"]["type"], "done", "{json}");
    let row = named(&report["per_commit"]["rows"]);
    assert_eq!(row["trend"]["current"]["commits"], 1, "{json}");
    assert_eq!(row["trend"]["previous"]["commits"], 1, "{json}");

    // Markdown, one document per identity; with --repo, only its owner is asked.
    let text = home.ok(&[&["report"][..], &range].concat());
    assert_eq!(titles(&text), ["personal", "work"], "{text}");
    assert_eq!(text.matches("\n## Estimates\n").count(), 2, "{text}");
    let md = home.ok(&[&["report", "--format", "md"][..], &range].concat());
    assert_eq!(md, text);
    let scoped = home.ok(&[&["report", "--repo", "code"][..], &range].concat());
    assert_eq!(titles(&scoped), ["personal"], "{scoped}");
    assert!(scoped.contains("Scope: code."), "{scoped}");

    for bad in [
        &["report", "--repo", "nowhere"][..],
        &["report", "--since", "2000-01-04", "--until", "2000-01-03"],
        &["tokens", "--format", "md"],
    ] {
        let output = home.bosun(bad);
        assert_eq!(
            output.status.code(),
            Some(documented("bad arguments")),
            "{bad:?}: {output:?}"
        );
    }
}

/// #67: `bosun pause`, `resume` and `kill` send the operator token the daemon wrote; without
/// the file (an agent's environment does not name it) the daemon is never asked.
#[tokio::test(flavor = "multi_thread")]
async fn the_control_switches_take_the_operator_token() {
    let home = Home::new(&[("personal", "cache-and-models")]);
    let demo = home.path("demo");
    std::fs::create_dir_all(&demo).unwrap();
    let mut config = std::fs::read_to_string(home.path("config.toml")).unwrap();
    let _ = write!(
        config,
        "[repo.demo]
path = '{}'
identity = 'personal'
",
        demo.display()
    );
    std::fs::write(home.path("config.toml"), config).unwrap();
    let _daemon = home.start().await;
    let token = std::fs::read_to_string(home.path("operator.token")).unwrap();
    assert!(token.starts_with("bosun-operator-"), "{token}");

    let paused = home.ok(&["pause", "--repo", "demo", "--format", "json"]);
    let value: Value = serde_json::from_str(&paused).unwrap();
    let demo_row = value["personal"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == "demo")
        .cloned()
        .unwrap();
    assert_eq!(demo_row["hold"], "pause", "{paused}");
    let resumed = home.ok(&["resume", "--repo", "demo"]);
    assert!(resumed.contains("repo demo: hold none"), "{resumed}");
    let all = home.ok(&["pause"]);
    assert!(all.contains("every repository is paused by hand"), "{all}");

    let killed = home.ok(&["kill", "--format", "json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&killed).unwrap()["killed"],
        true
    );
    let released = home.ok(&["kill", "--release"]);
    assert!(released.contains("released"), "{released}");

    // Without the token's file the switches fail before they reach the daemon.
    let output = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .args(["kill"])
        .env("BOSUN_CONFIG", home.path("config.toml"))
        .env("BOSUN_ENDPOINT", home.endpoint.path())
        .env("BOSUN_OPERATOR_FILE", home.path("missing.token"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("operator token"),
        "{output:?}"
    );
}
