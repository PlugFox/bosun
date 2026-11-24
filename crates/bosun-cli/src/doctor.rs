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
//! | `stop_hook` | a repository's `.claude/settings.json` or `.claude/settings.local.json` still has a `hooks.Stop` entry, left over from an in-repository loop (warn, #75); either file is not valid JSON (warn) |
//! | `git` | `git` is not on the `PATH` the daemon gets (fail) |
//! | `sh` | the verdict shell (D4 of #7: `sh` of the Git installation on Windows, or a repository's `verdict.shell`) is not found (fail; warn when no repository has a queue) |
//! | `claude` | the agent CLI is not on the daemon's `PATH` (fail; warn when no repository has a queue) |
//! | `gh` | `gh` is not on the daemon's `PATH` (fail when a repository has a queue or an identity a `github_login`, else warn) |
//! | `github_login` | an identity's `gh` (its `gh_config_dir`) is not logged in, or answers as another login than `github_login` (fail) |
//! | `registration` | no logon registration (fail); its daemon is missing (fail); it is not loaded or enabled (warn) |
//! | `endpoint` | nothing listens at the endpoint (fail) |
//! | `endpoint_acl` | Unix: the socket's mode is not 0600 (fail). Windows: skipped, see below |
//! | `daemon` | the daemon does not answer `hello`, or speaks another API version (fail) |
//!
//! The per-identity checks carry the identity's name. The `PATH` the daemon gets is the one
//! the registration sets on macOS and Linux; on Windows, and with no registration, it is the
//! `PATH` `bosun doctor` runs with, which approximates the environment the logon task gets.
//!
//! The tool checks (#71) look on that same `PATH`: a tool missing there is missing for the
//! daemon, and its check fails, so `bosun doctor` exits 7 naming it. `github_login` runs
//! `gh api graphql` for the viewer's login, as the GitHub transport does (D5 of #7), under the
//! identity's `GH_CONFIG_DIR` and with every token variable removed, so only the identity's
//! own login answers.
//!
//! On Windows `endpoint_acl` is skipped: reading a pipe's DACL takes a handle to it, which
//! connects to one of the daemon's instances, and a client has no `unsafe` to read it with
//! (#12). The daemon itself creates every instance with a DACL of one entry, for its user, and
//! refuses to listen when the DACL it reads back differs (#71).
//!
//! `stop_hook` runs for each configured repository (of `--identity` only, when given) and names
//! the repository in its detail. A stop hook that refuses the end of a turn would keep every
//! headless session the daemon spawns from exiting; `docs/migration.md` says what to remove.
//!
//! The configuration is validated without the disk probe, so that a database on a network disk
//! is reported by `database_disk` and not as an invalid file.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bosun_client::{Client, Endpoint, Error};
use bosun_config::{Config, Disk, DiskProbe, SystemProbe, tools};
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
    let config = config(inputs, &mut checks);
    if let Some(config) = &config {
        identities(inputs, config, &mut checks);
        // Mutant `mutant_doctor_skip_stop_hook` (#75): repositories are not looked at.
        if !cfg!(mutant_doctor_skip_stop_hook) {
            repositories(inputs, config, &mut checks);
        }
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
    let path = DaemonPath::new(registered_path);
    checks.push(git(&path));
    // Mutant `mutant_doctor_skip_tool_paths` (#71): only `git` is looked for.
    if !cfg!(mutant_doctor_skip_tool_paths) {
        let gh = tool_paths(config.as_ref(), &path, &mut checks);
        if let (Some(config), Some(gh)) = (&config, gh) {
            logins(inputs, config, &gh, &path, &mut checks);
        }
    }
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

/// The agent settings files that can declare hooks, in a repository's `.claude` (#75).
const AGENT_SETTINGS: [&str; 2] = ["settings.json", "settings.local.json"];

/// `stop_hook` of each configured repository.
fn repositories(inputs: &Inputs, config: &Config, checks: &mut Vec<Check>) {
    for (name, repo) in &config.repos {
        if inputs
            .identity
            .as_ref()
            .is_some_and(|only| *only != repo.identity)
        {
            continue;
        }
        checks.push(stop_hook(name, &repo.path).of(&repo.identity));
    }
}

/// Whether a `hooks.Stop` value declares anything: `null` and empty are not hooks.
fn declares(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Array(entries) => !entries.is_empty(),
        serde_json::Value::Object(entries) => !entries.is_empty(),
        _ => true,
    }
}

/// A stop hook left in repository `name` at `root` by an in-repository loop (#75): Claude
/// Code's `.claude/settings.json` and `.claude/settings.local.json`, each looked at for a
/// `hooks.Stop` entry. Every file that has one is named.
pub(crate) fn stop_hook(name: &str, root: &Path) -> Check {
    if !root.is_dir() {
        return Check::skip(
            "stop_hook",
            format!("repo.{name}: {} is not a directory", root.display()),
        );
    }
    let dir = root.join(".claude");
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for file in AGENT_SETTINGS.map(|f| dir.join(f)) {
        let text = match std::fs::read_to_string(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                unreadable.push(format!("cannot read {}: {e}", file.display()));
                continue;
            }
            Ok(text) => text,
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Err(e) => unreadable.push(format!("{} is not valid JSON: {e}", file.display())),
            Ok(value) => {
                if value.pointer("/hooks/Stop").is_some_and(declares) {
                    found.push(file.display().to_string());
                }
            }
        }
    }
    if !found.is_empty() {
        let files = found.join(", ");
        return Check::warn(
            "stop_hook",
            format!(
                "repo.{name}: a Stop hook in {files}; a hook that refuses the end of a turn \
                 keeps the headless sessions bosun runs there from exiting"
            ),
            format!(
                "remove the Stop entry from \"hooks\" in {files}, and the script it runs \
                 (docs/migration.md)"
            ),
        );
    }
    if !unreadable.is_empty() {
        return Check::warn(
            "stop_hook",
            format!("repo.{name}: {}", unreadable.join("; ")),
            "correct the file; Claude Code reads it too",
        );
    }
    Check::ok(
        "stop_hook",
        format!(
            "repo.{name}: no Stop hook in {}",
            AGENT_SETTINGS
                .map(|f| dir.join(f).display().to_string())
                .join(" or ")
        ),
    )
}

/// The `PATH` the daemon gets: the registration's, else this process's (see the module
/// documentation), and how to name it.
struct DaemonPath {
    path: Option<OsString>,
    source: &'static str,
}

impl DaemonPath {
    fn new(registered: Option<String>) -> Self {
        match registered {
            Some(path) => Self {
                path: Some(path.into()),
                source: "the PATH the registration gives the daemon",
            },
            None => Self {
                path: std::env::var_os("PATH"),
                source: if cfg!(windows) {
                    "this PATH, which the logon task inherits from your environment"
                } else {
                    "this PATH, which bosun install gives the daemon"
                },
            },
        }
    }

    fn find(&self, program: &str) -> Option<PathBuf> {
        tools::find_program(program, self.path.as_deref())
    }

    fn shell(&self, configured: Option<&str>) -> Option<PathBuf> {
        tools::verdict_shell(configured, self.path.as_deref())
    }
}

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// The fix of a tool missing from the daemon's `PATH`.
fn on_path_fix(what: &str) -> String {
    format!(
        "install {what} and put its directory on PATH, then run bosun install again so the \
         registration takes the new PATH"
    )
}

/// `git` on the daemon's `PATH`.
fn git(path: &DaemonPath) -> Check {
    match path.find("git") {
        Some(git) => Check::ok("git", format!("{} on {}", git.display(), path.source)),
        None => Check::fail(
            "git",
            format!("no {} on {}", exe("git"), path.source),
            on_path_fix("git"),
        ),
    }
}

/// A tool that is missing: a failure when it is `required`, else a warning.
fn missing(name: &'static str, required: bool, detail: String, fix: String) -> Check {
    if required {
        Check::fail(name, detail, fix)
    } else {
        Check::warn(name, format!("{detail}; not needed until it has work"), fix)
    }
}

/// The verdict shell, the agent CLI and `gh` on the daemon's `PATH` (#71). Each is required
/// when the configuration gives the daemon work (a repository with a queue) or could not be
/// read; `gh` also when an identity names a `github_login`. Returns where `gh` is.
fn tool_paths(
    config: Option<&Config>,
    path: &DaemonPath,
    checks: &mut Vec<Check>,
) -> Option<PathBuf> {
    let queued: Vec<(&String, &bosun_config::Repo)> = config.map_or_else(Vec::new, |c| {
        c.repos.iter().filter(|(_, r)| r.queue.is_some()).collect()
    });
    let works = config.is_none() || !queued.is_empty();

    // The default shell, unless every repository with a queue names its own.
    if queued.is_empty() || queued.iter().any(|(_, r)| r.verdict.shell.is_none()) {
        let what = if cfg!(windows) {
            "sh.exe of the Git installation"
        } else {
            "sh"
        };
        checks.push(match path.shell(None) {
            Some(sh) => Check::ok("sh", format!("{} on {}", sh.display(), path.source)),
            None => missing(
                "sh",
                works,
                format!("no {what} for verdicts on {}", path.source),
                if cfg!(windows) {
                    "install Git for Windows, which provides sh.exe, or set \
                     repo.<name>.verdict.shell"
                        .to_owned()
                } else {
                    on_path_fix("sh")
                },
            ),
        });
    }
    for (name, repo) in &queued {
        let Some(shell) = repo.verdict.shell.as_deref() else {
            continue;
        };
        checks.push(match path.shell(Some(shell)) {
            Some(sh) => Check::ok(
                "sh",
                format!(
                    "repo.{name}.verdict.shell: {} on {}",
                    sh.display(),
                    path.source
                ),
            ),
            None => Check::fail(
                "sh",
                format!(
                    "repo.{name}.verdict.shell `{shell}` is not found on {}",
                    path.source
                ),
                format!("install {shell}, or correct repo.{name}.verdict.shell"),
            ),
        });
    }

    checks.push(match path.find("claude") {
        Some(claude) => Check::ok("claude", format!("{} on {}", claude.display(), path.source)),
        None => missing(
            "claude",
            works,
            format!(
                "no {} (the agent CLI, Claude Code) on {}",
                exe("claude"),
                path.source
            ),
            on_path_fix("Claude Code"),
        ),
    });

    let logins = config.is_some_and(|c| c.identities.values().any(|i| i.github_login.is_some()));
    let gh = path.find("gh");
    checks.push(match &gh {
        Some(gh) => Check::ok("gh", format!("{} on {}", gh.display(), path.source)),
        None => missing(
            "gh",
            works || logins,
            format!("no {} (the GitHub CLI) on {}", exe("gh"), path.source),
            on_path_fix("the GitHub CLI (gh)"),
        ),
    });
    gh
}

/// How long `gh` has to name the login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The viewer's login, as the GitHub transport asks for it (D5 of #7).
const VIEWER_QUERY: &str = "query{viewer{login}}";

/// Variables through which `gh` would use another token or configuration than the identity's.
const GH_OVERRIDES: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
];

/// The `gh` command that names the login of `gh_config_dir` on `host`.
fn login_command(gh: &Path, host: &str, gh_config_dir: &Path, path: &DaemonPath) -> Command {
    let mut command = Command::new(gh);
    command
        .args(["api", "graphql", "--hostname", host, "-f"])
        .arg(format!("query={VIEWER_QUERY}"))
        .args(["--jq", ".data.viewer.login"]);
    for var in GH_OVERRIDES {
        command.env_remove(var);
    }
    command
        .env("GH_CONFIG_DIR", gh_config_dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dirs) = &path.path {
        command.env("PATH", dirs);
    }
    command
}

/// Runs `command` for at most `timeout`: its trimmed stdout, or why not.
fn login_of(mut command: Command, timeout: Duration) -> Result<String, String> {
    let mut child = command.spawn().map_err(|e| format!("cannot run gh: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("gh did not answer within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("cannot wait for gh: {e}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("cannot read the output of gh: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        Err(format!(
            "gh exited with {}: {}",
            output.status,
            first.trim()
        ))
    }
}

/// The `github_login` check of identity `name`, from what `gh` answered.
fn login_check(
    name: &str,
    expected: &str,
    host: &str,
    gh_config_dir: &Path,
    answer: Result<String, String>,
) -> Check {
    let dir = gh_config_dir.display();
    let relogin =
        format!("log in as {expected}: GH_CONFIG_DIR={dir} gh auth login --hostname {host}");
    match answer {
        // GitHub logins are case-insensitive.
        Ok(login) if login.eq_ignore_ascii_case(expected) => Check::ok(
            "github_login",
            format!("{dir} answers as {login} on {host}"),
        ),
        Ok(login) if login.is_empty() => Check::fail(
            "github_login",
            format!("{dir} named no login on {host}"),
            relogin,
        ),
        Ok(login) => Check::fail(
            "github_login",
            format!(
                "{dir} answers as {login} on {host}, not as {expected} \
                 (identity.{name}.github_login)"
            ),
            format!("{relogin}, or correct identity.{name}.github_login"),
        ),
        Err(why) => Check::fail("github_login", format!("{dir} on {host}: {why}"), relogin),
    }
}

/// `github_login` of each identity that names one (#71).
fn logins(inputs: &Inputs, config: &Config, gh: &Path, path: &DaemonPath, checks: &mut Vec<Check>) {
    for (name, identity) in &config.identities {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let Some(expected) = &identity.github_login else {
            continue;
        };
        let host = &identity.github_host;
        let check = match &identity.gh_config_dir {
            None => Check::fail(
                "github_login",
                format!("identity.{name}.github_login is set, but gh_config_dir is not"),
                format!(
                    "set identity.{name}.gh_config_dir to the GH_CONFIG_DIR where {expected} \
                     is logged in"
                ),
            ),
            Some(dir) => {
                let answer = login_of(login_command(gh, host, dir, path), LOGIN_TIMEOUT);
                login_check(name, expected, host, dir, answer)
            }
        };
        checks.push(check.of(name));
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

/// Who may open the endpoint (#4, #71): on Unix the socket's mode, which must be 0600.
#[cfg(unix)]
fn endpoint_acl(endpoint: &Endpoint) -> Check {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(endpoint.path()) {
        Err(e) => Check::fail(
            "endpoint_acl",
            format!("cannot read the mode of {endpoint}: {e}"),
            "make the socket's directory readable",
        ),
        Ok(meta) if meta.permissions().mode() & 0o777 == 0o600 => {
            Check::ok("endpoint_acl", format!("{endpoint} has mode 0600"))
        }
        Ok(meta) => Check::fail(
            "endpoint_acl",
            format!(
                "{endpoint} has mode {:04o}, not 0600: other users may connect",
                meta.permissions().mode() & 0o777
            ),
            "stop whatever listens there and start bosund again; it creates the socket with \
             mode 0600",
        ),
    }
}

/// Who may open the endpoint (#4, #71): on Windows the daemon checks it (see the module
/// documentation).
#[cfg(windows)]
fn endpoint_acl(_: &Endpoint) -> Check {
    Check::skip(
        "endpoint_acl",
        "bosund creates the pipe with a DACL of one entry, for its user, and refuses to \
         listen when the DACL it reads back differs",
    )
}

/// Other platforms have neither a socket mode nor a DACL to read here.
#[cfg(not(any(unix, windows)))]
fn endpoint_acl(_: &Endpoint) -> Check {
    Check::skip("endpoint_acl", "not checked on this platform")
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
    checks.push(endpoint_acl(endpoint));
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
    fn the_login_check_runs_gh_under_the_identity_s_directory_only() {
        let path = DaemonPath {
            path: Some("tools".into()),
            source: "a test",
        };
        let command = login_command(
            Path::new("gh"),
            "github.example",
            Path::new("gh-dir"),
            &path,
        );
        let envs: Vec<_> = command.get_envs().collect();
        let value = |key: &str| {
            envs.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        for var in GH_OVERRIDES {
            assert_eq!(value(var), Some(None), "{var} is removed");
        }
        assert_eq!(value("GH_CONFIG_DIR"), Some(Some("gh-dir".to_owned())));
        assert_eq!(value("PATH"), Some(Some("tools".to_owned())));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args[..4],
            ["api", "graphql", "--hostname", "github.example"]
        );

        let check = |answer| login_check("work", "me", "github.com", Path::new("d"), answer);
        assert_eq!(check(Ok("ME".to_owned())).status, Status::Ok);
        assert_eq!(check(Ok("other".to_owned())).status, Status::Fail);
        assert_eq!(check(Ok(String::new())).status, Status::Fail);
        assert_eq!(check(Err("offline".to_owned())).status, Status::Fail);
    }

    #[test]
    fn a_stop_hook_is_found_in_either_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let claude = root.join(".claude");
        // No `.claude` directory at all.
        assert_eq!(stop_hook("app", root).status, Status::Ok);
        std::fs::create_dir_all(&claude).unwrap();
        let hook = r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "loop"}]}]}}"#;
        for (settings, local, status) in [
            (Some("{}"), None, Status::Ok),
            (Some(r#"{"hooks": {"PreToolUse": [{}]}}"#), None, Status::Ok),
            (Some(r#"{"hooks": {"Stop": []}}"#), None, Status::Ok),
            (Some(r#"{"hooks": {"Stop": null}}"#), None, Status::Ok),
            (Some(hook), None, Status::Warn),
            (None, Some(hook), Status::Warn),
            (Some("not json"), None, Status::Warn),
        ] {
            for (file, text) in [("settings.json", settings), ("settings.local.json", local)] {
                let path = claude.join(file);
                match text {
                    Some(text) => std::fs::write(&path, text).unwrap(),
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            let check = stop_hook("app", root);
            assert_eq!(check.status, status, "{settings:?} {local:?}: {check:?}");
            if status == Status::Warn {
                assert!(check.fix.is_some(), "{check:?}");
            }
        }
        assert_eq!(stop_hook("app", &root.join("absent")).status, Status::Skip);
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
