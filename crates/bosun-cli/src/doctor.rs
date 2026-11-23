//! `bosun doctor` (#4, #28): checks what the daemon needs, with the daemon up or down.
//!
//! It reads the configuration through `bosun-config` and asks the OS; the daemon is only
//! asked `hello`. Each check is `ok`, `warn`, `fail` or `skip` (it could not run because an
//! earlier check failed), and each warning and failure carries a fix.
//!
//! | Check | Fails or warns when |
//! | --- | --- |
//! | `config` | the configuration file is missing, malformed or invalid (fail) |
//! | `identity` | `--identity` names an identity the configuration does not have (fail) |
//! | `claude_config_dir` | an identity's Claude configuration directory does not exist (fail) |
//! | `retention` | Claude Code deletes transcripts (`cleanupPeriodDays` of the directory's `settings.json`, 30 when absent) sooner than `retention.reingest_window` (warn, #3) |
//! | `database_disk` | an identity's database directory is on a network or synced disk (fail) |
//! | `git` | `git` is not on the `PATH` the daemon gets (fail) |
//! | `registration` | no logon registration (fail); its daemon is missing (fail); it is not loaded or enabled (warn) |
//! | `endpoint` | nothing listens at the endpoint (fail) |
//! | `daemon` | the daemon does not answer `hello`, or speaks another API version (fail) |
//!
//! The per-identity checks carry the identity's name. The `PATH` the daemon gets is the one
//! the registration sets on macOS and Linux; on Windows, and with no registration, it is the
//! `PATH` `bosun doctor` runs with, which approximates the environment the logon task gets.
//!
//! The configuration is validated without the disk probe, so that a database on a network disk
//! is reported by `database_disk` and not as an invalid file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bosun_client::{Client, Endpoint, Error};
use bosun_config::{Config, Disk, DiskProbe, SystemProbe};
use serde::Serialize;

use crate::install::{self, Platform};

/// Claude Code's `cleanupPeriodDays` when its settings do not set it.
pub(crate) const DEFAULT_CLEANUP_PERIOD_DAYS: u64 = 30;

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
            Self::Skip => "skip",
        }
    }
}

/// One check and its outcome.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Check {
    #[serde(rename = "check")]
    pub(crate) name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identity: Option<String>,
    pub(crate) status: Status,
    pub(crate) detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fix: Option<String>,
}

impl Check {
    fn new(check: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: check,
            identity: None,
            status,
            detail: detail.into(),
            fix: None,
        }
    }

    fn ok(check: &'static str, detail: impl Into<String>) -> Self {
        Self::new(check, Status::Ok, detail)
    }

    fn fail(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(check, Status::Fail, detail).fix(fix)
    }

    fn warn(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(check, Status::Warn, detail).fix(fix)
    }

    fn skip(check: &'static str, detail: impl Into<String>) -> Self {
        Self::new(check, Status::Skip, detail)
    }

    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    fn of(mut self, identity: &str) -> Self {
        self.identity = Some(identity.to_owned());
        self
    }
}

/// What `bosun doctor` looks at.
#[derive(Debug)]
pub(crate) struct Inputs {
    /// The configuration file, when one is named.
    pub(crate) config: Option<PathBuf>,
    /// Only this identity, when given.
    pub(crate) identity: Option<String>,
    /// The endpoint, or why it has no name.
    pub(crate) endpoint: Result<Endpoint, String>,
    /// The registration's name.
    pub(crate) name: String,
}

/// Validation without the disk probe: `database_disk` reports the disk.
struct NoProbe;

impl DiskProbe for NoProbe {
    fn probe(&self, _: &Path) -> Disk {
        Disk::Local
    }
}

/// Runs every check, in the order of the table above.
pub(crate) async fn run(inputs: &Inputs) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Some(config) = config(inputs, &mut checks) {
        identities(inputs, &config, &mut checks);
    }

    let platform = Platform::host();
    let registration = match &platform {
        Ok(platform) => Some(install::query(*platform, &inputs.name)),
        Err(_) => None,
    };
    let registered_path = registration
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(Option::as_ref)
        .and_then(|r| r.path_var.clone());
    checks.push(git(registered_path));
    checks.push(match (platform, registration) {
        (Err(why), _) | (_, Some(Err(why))) => Check::skip("registration", why),
        (Ok(platform), Some(Ok(found))) => registration_check(platform, &inputs.name, found),
        (Ok(_), None) => Check::skip("registration", "not queried"),
    });
    daemon(&inputs.endpoint, &mut checks).await;
    checks
}

fn config(inputs: &Inputs, checks: &mut Vec<Check>) -> Option<Config> {
    let Some(path) = inputs.config.clone().or_else(bosun_config::default_path) else {
        checks.push(Check::fail(
            "config",
            "no configuration file: neither --config nor BOSUN_CONFIG is set, and the \
             environment names no home directory",
            "pass --config <file> or set BOSUN_CONFIG",
        ));
        return None;
    };
    match bosun_config::load(&path, &NoProbe) {
        Ok(config) => {
            let mut detail = format!(
                "{}: {} identities, {} repositories",
                path.display(),
                config.identities.len(),
                config.repos.len()
            );
            if !config.notices.is_empty() {
                let _ = write!(
                    detail,
                    "; {} keys not read by this version",
                    config.notices.len()
                );
            }
            checks.push(Check::ok("config", detail));
            Some(config)
        }
        Err(e @ bosun_config::Error::Read { .. }) => {
            checks.push(Check::fail(
                "config",
                e.to_string(),
                format!(
                    "create {} (its format is in the bosun-config documentation), or pass \
                     --config <file>",
                    path.display()
                ),
            ));
            None
        }
        Err(e) => {
            checks.push(Check::fail(
                "config",
                format!("{}: {e}", path.display()),
                "correct each problem listed, at its line and column",
            ));
            None
        }
    }
}

fn identities(inputs: &Inputs, config: &Config, checks: &mut Vec<Check>) {
    if let Some(name) = &inputs.identity
        && !config.identities.contains_key(name)
    {
        checks.push(Check::fail(
            "identity",
            format!("the configuration has no identity `{name}`"),
            format!("add [identity.{name}] to the configuration, or pass another --identity"),
        ));
        return;
    }
    let probe = SystemProbe::new();
    let window = config.retention.reingest_window;
    for (name, identity) in &config.identities {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let dir = &identity.claude_config_dir;
        if dir.is_dir() {
            checks.push(Check::ok("claude_config_dir", dir.display().to_string()).of(name));
            // Mutant `mutant_doctor_skip_retention` (#28): the retention check is not run.
            if !cfg!(mutant_doctor_skip_retention) {
                checks.push(retention(dir, window).of(name));
            }
        } else {
            checks.push(
                Check::fail(
                    "claude_config_dir",
                    format!("{} is not a directory", dir.display()),
                    format!(
                        "run Claude Code once with CLAUDE_CONFIG_DIR={}, or correct \
                         identity.{name}.claude_config_dir",
                        dir.display()
                    ),
                )
                .of(name),
            );
            checks.push(Check::skip("retention", "no configuration directory").of(name));
        }
        checks.push(database_disk(&probe, name, &identity.database).of(name));
    }
}

/// Days, rounded up.
fn days(window: Duration) -> u64 {
    window.as_secs().div_ceil(86_400)
}

/// Claude Code's transcript retention in `dir` against the reingest window (#3, "Replay").
pub(crate) fn retention(dir: &Path, window: Duration) -> Check {
    let file = dir.join("settings.json");
    let fix = {
        format!(
            "set \"cleanupPeriodDays\": {} (or more) in {}, or lower \
             retention.reingest_window",
            days(window),
            file.display()
        )
    };
    let period = match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Check::warn(
                "retention",
                format!("cannot read {}: {e}", file.display()),
                "make the file readable",
            );
        }
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Err(e) => {
                return Check::warn(
                    "retention",
                    format!("{} is not valid JSON: {e}", file.display()),
                    "correct the file; Claude Code reads it too",
                );
            }
            Ok(value) => match value.get("cleanupPeriodDays") {
                None => None,
                Some(days) => match days.as_u64() {
                    Some(days) => Some(days),
                    None => {
                        return Check::warn(
                            "retention",
                            format!(
                                "cleanupPeriodDays in {} is {days}, not a whole number of days",
                                file.display()
                            ),
                            fix.clone(),
                        );
                    }
                },
            },
        },
    };
    let (period, source) = match period {
        Some(days) => (
            days,
            format!("cleanupPeriodDays {days} in {}", file.display()),
        ),
        None => (
            DEFAULT_CLEANUP_PERIOD_DAYS,
            format!(
                "cleanupPeriodDays not set in {}: Claude Code's default, {DEFAULT_CLEANUP_PERIOD_DAYS}",
                file.display()
            ),
        ),
    };
    let window_text = format!("the reingest window of {} days", days(window));
    if u128::from(period) * 86_400 < u128::from(window.as_secs()) {
        Check::warn(
            "retention",
            format!(
                "{source}: Claude Code deletes transcripts sooner than {window_text}, so a \
                 reingest cannot reach that far back"
            ),
            fix.clone(),
        )
    } else {
        Check::ok("retention", format!("{source}, at least {window_text}"))
    }
}

fn database_disk(probe: &SystemProbe, name: &str, database: &Path) -> Check {
    let Some(dir) = database.parent() else {
        return Check::skip("database_disk", "the database names a root");
    };
    match probe.probe(dir) {
        Disk::Local => Check::ok(
            "database_disk",
            format!("{} is on a local disk", dir.display()),
        ),
        Disk::Network(why) | Disk::Synced(why) => Check::fail(
            "database_disk",
            why,
            format!(
                "move identity.{name}.database to a directory on a local disk that no sync \
                 client copies"
            ),
        ),
    }
}

/// `git` on `registered` (the registration's `PATH`), else on this process's `PATH`.
fn git(registered: Option<String>) -> Check {
    let (path, source) = match registered {
        Some(path) => (
            Some(path.into()),
            "the PATH the registration gives the daemon",
        ),
        None => (
            std::env::var_os("PATH"),
            if cfg!(windows) {
                "this PATH, which the logon task inherits from your environment"
            } else {
                "this PATH, which bosun install gives the daemon"
            },
        ),
    };
    let exe = format!("git{}", std::env::consts::EXE_SUFFIX);
    let found = path
        .as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file());
    match found {
        Some(git) => Check::ok("git", format!("{} on {source}", git.display())),
        None => Check::fail(
            "git",
            format!("no {exe} on {source}"),
            "install git and put its directory on PATH, then run bosun install again so the \
             registration takes the new PATH",
        ),
    }
}

fn registration_check(
    platform: Platform,
    name: &str,
    found: Option<install::Registration>,
) -> Check {
    let reinstall = if name == install::DEFAULT_NAME {
        "run bosun install".to_owned()
    } else {
        format!("run bosun install --name {name}")
    };
    let Some(found) = found else {
        return Check::fail(
            "registration",
            format!("no logon registration named `{name}`"),
            reinstall,
        );
    };
    if let Some(daemon) = &found.daemon
        && !daemon.is_file()
    {
        return Check::fail(
            "registration",
            format!(
                "{} runs {}, which does not exist",
                found.place,
                daemon.display()
            ),
            format!("{reinstall} again, from the bosun next to the bosund to run"),
        );
    }
    let runs = found
        .daemon
        .as_ref()
        .map_or_default(|d| format!(", runs {}", d.display()));
    if !found.loaded {
        let state = match platform {
            Platform::Macos => "not loaded into this login session",
            _ => "not enabled",
        };
        return Check::warn(
            "registration",
            format!("{}{runs}: {state}", found.place),
            reinstall,
        );
    }
    Check::ok("registration", format!("{}{runs}", found.place))
}

async fn daemon(endpoint: &Result<Endpoint, String>, checks: &mut Vec<Check>) {
    let start = "start it: log in again, or run bosun install, which starts it now; run \
                 bosund in a terminal to see why it stops";
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(why) => {
            checks.push(Check::fail(
                "endpoint",
                why.clone(),
                "pass --endpoint or set BOSUN_ENDPOINT",
            ));
            checks.push(Check::skip("daemon", "the endpoint has no name"));
            return;
        }
    };
    let opened = tokio::time::timeout(bosun_client::CONNECT_TIMEOUT, bosun_client::open(endpoint))
        .await
        .unwrap_or(Err(Error::Timeout));
    let stream = match opened {
        Ok(stream) => stream,
        Err(e) => {
            checks.push(Check::fail("endpoint", e.to_string(), start));
            checks.push(Check::skip("daemon", "the endpoint is not reachable"));
            return;
        }
    };
    checks.push(Check::ok("endpoint", format!("{endpoint} is reachable")));
    let hello = tokio::time::timeout(bosun_client::CONNECT_TIMEOUT, Client::over(stream))
        .await
        .unwrap_or(Err(Error::Timeout));
    checks.push(match hello {
        Ok(client) => {
            let hello = client.daemon();
            Check::ok(
                "daemon",
                format!(
                    "bosund {} (pid {}, API version {}) answers",
                    hello.daemon_version, hello.pid, hello.api_version
                ),
            )
        }
        Err(e @ Error::UnsupportedVersion { .. }) => Check::fail(
            "daemon",
            e.to_string(),
            "install bosun and bosund of one release",
        ),
        Err(e) => Check::fail(
            "daemon",
            format!("{endpoint} does not answer hello: {e}"),
            "stop whatever listens there and start bosund again",
        ),
    });
}

/// The worst status of `checks`: `fail`, else `warn`, else `ok`.
pub(crate) fn overall(checks: &[Check]) -> Status {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Ok
    }
}

/// The checks as text: one line each, the fix indented under it, then a summary.
pub(crate) fn text(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let name = match &check.identity {
            Some(identity) => format!("{} [{identity}]", check.name),
            None => check.name.to_owned(),
        };
        let _ = writeln!(out, "{:<4}  {name}: {}", check.status.label(), check.detail);
        if let Some(fix) = &check.fix {
            let _ = writeln!(out, "      fix: {fix}");
        }
    }
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let _ = writeln!(
        out,
        "{} checks: {} ok, {} warnings, {} failed, {} skipped",
        checks.len(),
        count(Status::Ok),
        count(Status::Warn),
        count(Status::Fail),
        count(Status::Skip)
    );
    out
}

/// The checks as JSON: `{"status": <overall>, "checks": [...]}`.
pub(crate) fn json(checks: &[Check]) -> String {
    let value = serde_json::json!({
        "status": overall(checks),
        "checks": checks,
    });
    let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(dir: &Path, settings: Option<&str>, days: u64) -> Check {
        if let Some(text) = settings {
            std::fs::write(dir.join("settings.json"), text).unwrap();
        }
        retention(dir, Duration::from_hours(days * 24))
    }

    #[test]
    fn retention_compares_cleanup_period_days_with_the_window() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file: the default of 30 days.
        assert_eq!(check(dir.path(), None, 30).status, Status::Ok);
        assert_eq!(check(dir.path(), None, 31).status, Status::Warn);
        // Absent key.
        assert_eq!(check(dir.path(), Some("{}"), 31).status, Status::Warn);
        let short = check(dir.path(), Some(r#"{"cleanupPeriodDays": 7}"#), 14);
        assert_eq!(short.status, Status::Warn, "{short:?}");
        assert!(
            short
                .fix
                .as_deref()
                .unwrap()
                .contains("\"cleanupPeriodDays\": 14"),
            "{short:?}"
        );
        let long = check(dir.path(), Some(r#"{"cleanupPeriodDays": 90}"#), 60);
        assert_eq!(long.status, Status::Ok, "{long:?}");
        for bad in ["not json", r#"{"cleanupPeriodDays": "a week"}"#] {
            assert_eq!(
                check(dir.path(), Some(bad), 1).status,
                Status::Warn,
                "{bad}"
            );
        }
        // A window of part of a day rounds up.
        assert_eq!(days(Duration::from_hours(12)), 1);
    }

    #[test]
    fn the_overall_status_is_the_worst() {
        let ok = Check::ok("a", "");
        let warn = Check::warn("b", "", "");
        let fail = Check::fail("c", "", "");
        let skip = Check::skip("d", "");
        assert_eq!(overall(&[ok.clone(), skip.clone()]), Status::Ok);
        assert_eq!(overall(&[ok.clone(), warn.clone()]), Status::Warn);
        assert_eq!(overall(&[warn, fail, ok, skip]), Status::Fail);
    }
}
