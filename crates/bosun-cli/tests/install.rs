//! `bosun install`, `bosun uninstall` and `bosun doctor` (#28).
//!
//! The platform test of #12 registers a real logon process under a unique name, with a real
//! `bosund` on a temporary configuration and an endpoint of its own, so it never touches the
//! user's own registration, and removes it even when it fails. It skips, saying why, where the
//! platform's user service manager is not reachable (a CI runner without a login session); with
//! `BOSUN_REQUIRE_PLATFORM_TEST` set, a skip is a failure instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bosun_client::{Client, Endpoint};
use serde_json::Value;

/// The directory `bosun` was built into; `bosund` is built there too.
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
            .args(["build", "--locked", "--package", "bosund"])
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if dir.file_name().is_some_and(|n| n == "release") {
            build.arg("--release");
        }
        assert!(build.status().unwrap().success(), "building bosund failed");
        dir
    })
}

fn bosund() -> PathBuf {
    bin_dir().join(format!("bosund{}", std::env::consts::EXE_SUFFIX))
}

/// A name no real registration has.
fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

fn endpoint(dir: &Path) -> Endpoint {
    if cfg!(windows) {
        Endpoint::new(unique("bosun-install-test"))
    } else {
        Endpoint::new(dir.join("bosund.sock"))
    }
}

/// Runs `bosun` with `args` against `config` and `endpoint`, with neither variable set.
fn bosun(config: &Path, endpoint: &Endpoint, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bosun"))
        .args(args)
        .arg("--config")
        .arg(config)
        .arg("--endpoint")
        .arg(endpoint.path())
        .env_remove("BOSUN_CONFIG")
        .env_remove("BOSUN_ENDPOINT")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The exit code `bosun --help` documents for the line containing `meaning`.
fn documented(meaning: &str) -> i32 {
    let help = Command::new(env!("CARGO_BIN_EXE_bosun"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    let section = help.split_once("Exit codes:\n").unwrap().1;
    let lines: Vec<&str> = section.lines().filter(|l| l.contains(meaning)).collect();
    assert_eq!(lines.len(), 1, "`{meaning}` in {section}");
    lines[0].split_whitespace().next().unwrap().parse().unwrap()
}

/// The checks of `bosun doctor --format json` named `name`.
fn checks<'a>(report: &'a Value, name: &str) -> Vec<&'a Value> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["check"] == name)
        .collect()
}

fn doctor(config: &Path, endpoint: &Endpoint, name: &str) -> (Output, Value) {
    let output = bosun(
        config,
        endpoint,
        &["doctor", "--format", "json", "--name", name],
    );
    let report =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"));
    (output, report)
}

#[test]
fn doctor_warns_when_cleanup_period_is_below_the_reingest_window() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    std::fs::create_dir_all(&claude).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[identity.personal]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\
             [retention]\nreingest_window = '30d'\n",
            claude.display(),
            dir.path().join("personal.db").display()
        ),
    )
    .unwrap();
    let endpoint = endpoint(dir.path());
    // A registration that does not exist: doctor only queries it.
    let name = unique("bosun-doctor-test");
    let retention = |days: Option<u32>| {
        if let Some(days) = days {
            std::fs::write(
                claude.join("settings.json"),
                format!("{{\"cleanupPeriodDays\": {days}}}"),
            )
            .unwrap();
        }
        let (output, report) = doctor(&config, &endpoint, &name);
        // The registration and the daemon are missing: those checks fail.
        assert_eq!(
            output.status.code(),
            Some(documented("a check did not pass")),
            "{output:?}"
        );
        assert_eq!(report["status"], "fail", "{report}");
        let found = checks(&report, "retention");
        assert_eq!(found.len(), 1, "{report}");
        assert_eq!(found[0]["identity"], "personal", "{report}");
        found[0].clone()
    };

    // Claude Code's default of 30 days covers a window of 30 days.
    let check = retention(None);
    assert_eq!(check["status"], "ok", "{check}");

    let check = retention(Some(7));
    assert_eq!(check["status"], "warn", "{check}");
    assert!(
        check["fix"]
            .as_str()
            .unwrap()
            .contains("\"cleanupPeriodDays\": 30"),
        "{check}"
    );

    let check = retention(Some(60));
    assert_eq!(check["status"], "ok", "{check}");

    // The text form shows the warning and its fix.
    std::fs::write(claude.join("settings.json"), "{\"cleanupPeriodDays\": 7}").unwrap();
    let output = bosun(&config, &endpoint, &["doctor", "--name", &name]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("warn  retention [personal]: "), "{text}");
    assert!(
        text.contains("      fix: set \"cleanupPeriodDays\": 30"),
        "{text}"
    );
}

#[test]
fn doctor_reports_each_check() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[identity.work]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
            dir.path().join("missing").display(),
            dir.path().join("work.db").display()
        ),
    )
    .unwrap();
    let endpoint = endpoint(dir.path());
    let name = unique("bosun-doctor-test");
    let (output, report) = doctor(&config, &endpoint, &name);
    assert_eq!(
        output.status.code(),
        Some(documented("a check did not pass")),
        "{output:?}"
    );
    let status = |name: &str| -> Vec<String> {
        checks(&report, name)
            .iter()
            .map(|c| c["status"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(status("config"), ["ok"], "{report}");
    assert_eq!(status("claude_config_dir"), ["fail"], "{report}");
    assert_eq!(status("retention"), ["skip"], "{report}");
    assert_eq!(status("database_disk"), ["ok"], "{report}");
    assert_eq!(status("git").len(), 1, "{report}");
    assert_eq!(status("endpoint"), ["fail"], "{report}");
    assert_eq!(status("daemon"), ["skip"], "{report}");
    if cfg!(any(windows, target_os = "macos", target_os = "linux")) {
        assert_eq!(status("registration"), ["fail"], "{report}");
    }
    for check in report["checks"].as_array().unwrap() {
        if check["status"] == "fail" || check["status"] == "warn" {
            assert!(check["fix"].is_string(), "{check}");
        }
    }

    // A configuration that cannot be read fails one check; the others still run.
    let (output, report) = doctor(&dir.path().join("absent.toml"), &endpoint, &name);
    assert_eq!(
        output.status.code(),
        Some(documented("a check did not pass"))
    );
    assert_eq!(checks(&report, "config")[0]["status"], "fail", "{report}");
    assert!(checks(&report, "claude_config_dir").is_empty(), "{report}");
    assert_eq!(checks(&report, "endpoint").len(), 1, "{report}");
}

/// Why the platform's user service manager cannot be used here, if it cannot.
fn unavailable() -> Option<String> {
    let works = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if cfg!(windows) {
        None
    } else if cfg!(target_os = "macos") {
        let uid = Command::new("id").arg("-u").output().unwrap();
        let domain = format!("gui/{}", String::from_utf8_lossy(&uid.stdout).trim());
        (!works("launchctl", &["print", &domain])).then(|| {
            format!("`launchctl print {domain}` fails: no GUI login session to load an agent into")
        })
    } else if cfg!(target_os = "linux") {
        (!works("systemctl", &["--user", "show-environment"])).then(|| {
            "`systemctl --user show-environment` fails: no user service manager (no user bus)"
                .to_owned()
        })
    } else {
        Some(format!("no logon registration on {}", std::env::consts::OS))
    }
}

/// Removes the registration when the test ends, passed or not.
struct Registered<'a> {
    config: &'a Path,
    endpoint: &'a Endpoint,
    name: &'a str,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let _ = bosun(
            self.config,
            self.endpoint,
            &["uninstall", "--name", self.name],
        );
    }
}

async fn answers(endpoint: &Endpoint) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), Client::connect(endpoint)).await,
        Ok(Ok(_))
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn logon_registration_registers_queries_and_unregisters() {
    if let Some(why) = unavailable() {
        assert!(
            std::env::var_os("BOSUN_REQUIRE_PLATFORM_TEST").is_none(),
            "BOSUN_REQUIRE_PLATFORM_TEST is set, but {why}"
        );
        eprintln!("skipped: the logon registration platform test cannot run here: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let endpoint = endpoint(dir.path());
    let name = unique("bosun-install-test");
    let daemon = bosund();
    let daemon = daemon.to_str().unwrap();
    let _cleanup = Registered {
        config: &config,
        endpoint: &endpoint,
        name: &name,
    };

    // Register, twice: install is idempotent.
    let mut started = true;
    for round in 0..2 {
        let output = bosun(
            &config,
            &endpoint,
            &["install", "--name", &name, "--daemon", daemon],
        );
        assert_eq!(output.status.code(), Some(0), "round {round}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("started"), "{stdout}");
        if !stdout.contains("the daemon answers") {
            // A CI runner may have no interactive session for the task to start in; the
            // registration is still checked. On a workstation the daemon must start.
            assert!(
                std::env::var_os("CI").is_some(),
                "round {round}: the registered daemon did not start: {output:?}"
            );
            eprintln!(
                "note: the registered daemon did not answer (no interactive session?): {output:?}"
            );
            started = false;
        }

        // Query: doctor finds the registration and, when it started, the daemon.
        let (_, report) = doctor(&config, &endpoint, &name);
        let registration = checks(&report, "registration");
        assert_eq!(registration[0]["status"], "ok", "{report}");
        assert!(
            registration[0]["detail"].as_str().unwrap().contains(daemon),
            "{report}"
        );
        if started {
            assert_eq!(checks(&report, "daemon")[0]["status"], "ok", "{report}");
        }
    }

    // Unregister: the daemon stops and the registration is gone.
    let output = bosun(&config, &endpoint, &["uninstall", "--name", &name]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("nothing is registered"), "{stdout}");
    let (_, report) = doctor(&config, &endpoint, &name);
    assert_eq!(
        checks(&report, "registration")[0]["status"],
        "fail",
        "{report}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while answers(&endpoint).await {
        assert!(
            Instant::now() < deadline,
            "the daemon still answers after uninstall"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let output = bosun(&config, &endpoint, &["uninstall", "--name", &name]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("nothing is registered"),
        "{output:?}"
    );
}
