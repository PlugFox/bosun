//! `bosun doctor` (#4, #28): checks what the daemon needs, with the daemon up or down.
//!
//! It reads the configuration through `bosun-config` and asks the OS; the daemon is asked
//! `hello`, then `doctor.probe` of each identity for `daemon_gh`, then `ingest.status` of each
//! identity for `ingest_format`, then `status.overview` of
//! each identity for `served_config`, then `notify.channel.list` of each identity for `notify`.
//! Each check is `ok`,
//! `warn`, `fail` or `skip` (it could not run because an earlier check failed), and each
//! warning and failure carries a fix.
//!
//! | Check | Fails or warns when |
//! | --- | --- |
//! | `config` | the configuration file is missing, malformed or invalid (fail) |
//! | `identity` | `--identity` names an identity the configuration does not have (fail) |
//! | `claude_config_dir` | an identity's Claude configuration directory does not exist (fail) |
//! | `retention` | Claude Code deletes transcripts (`cleanupPeriodDays` of the directory's `settings.json`, 30 when absent) sooner than `retention.reingest_window` (warn, #3) |
//! | `database_disk` | an identity's database directory is on a network or synced disk (fail) |
//! | `stop_hook` | a repository's `.claude/settings.json` or `.claude/settings.local.json` still has a `hooks.Stop` entry, left over from an in-repository loop (warn, #75); either file is not valid JSON (warn) |
//! | `auto_memory` | never; names a repository's auto memory directory under its identity's Claude configuration directory when it exists, for review (#135) |
//! | `git` | `git` is not on the `PATH` the daemon gets (fail) |
//! | `sh` | the verdict shell (D4 of #7: `sh` of the Git installation on Windows, or a repository's `verdict.shell`) is not found (fail; warn when no repository has a queue) |
//! | `claude` | the agent CLI is not on the daemon's `PATH` (fail; warn when no repository has a queue) |
//! | `gh` | `gh` is not on the daemon's `PATH` (fail when a repository has a queue or an identity a `github_login`, else warn) |
//! | `github_login` | an identity's `gh` (its `gh_config_dir`) is not logged in, or answers as another login than `github_login` (fail) |
//! | `registration` | no logon registration (fail); its daemon is missing (fail); it is not loaded or enabled (warn) |
//! | `endpoint` | nothing listens at the endpoint (fail) |
//! | `endpoint_acl` | Unix: the socket's mode is not 0600 (fail). Windows: skipped, see below |
//! | `daemon` | the daemon does not answer `hello`, or speaks another API version (fail) |
//! | `daemon_gh` | the daemon cannot start its `gh` (`doctor.probe` of each identity, run in the daemon's own process: fail, #131); the daemon does not know `doctor.probe` (warn); skipped when the daemon did not answer |
//! | `ingest_format` | more than 1% of an identity's usage-bearing transcript lines billed nothing, or more than 1% of its lines are of record types the parser does not know (`ingest.status`, #6): the provider's transcript format may have moved under the parser (warn); ok with the counts, or `nothing counted yet`; skipped when the daemon did not answer |
//! | `served_config` | the configuration differs from what the daemon serves (`status.overview` of each identity) in a way that waits for the next daemon start: a repository added, removed, moved to another identity, or with another `path` or queue (its kind or `queue_source`); an identity the daemon does not serve (warn, #186); skipped when the daemon did not answer |
//! | `notify` | per channel of the identity (`notify.channel.list`, #9): ok naming the channel, its kind and `secret present`; fail when its secret is missing from the OS credential store (fix: remove it and add it again); ok `no channels` when there is none; never a secret's value; warn when the daemon does not know the method; skipped when the daemon did not answer |
//! | `log` | never fails: names the daemon's log file (`bosun_client::log`, #137), ok when it exists, skipped when the daemon has not written it yet |
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
//! `auto_memory` also runs for each configured repository. Sessions bosun spawned before #135
//! ran with Claude Code's auto memory on, under the identity's `CLAUDE_CONFIG_DIR`, and may
//! have written into `<config>/projects/<project>/memory/`, which every later session loads. The
//! files there cannot be told apart from the user's own interactive sessions' (a memory file
//! names no session), so the check names the directory and leaves the review to the user.
//!
//! The configuration is validated without the disk probe, so that a database on a network disk
//! is reported by `database_disk` and not as an invalid file.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use bosun_api::rpc::code;
use bosun_api::{
    Channels, IdentityParams, IngestStatus, Method, ProbeResult, RepoOverview, method,
};
use bosun_client::{Client, Endpoint, Error};
use bosun_config::{Config, Disk, DiskProbe, Queue, SystemProbe, tools};
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
    let mut client = daemon(&inputs.endpoint, &mut checks).await;
    if let Some(config) = &config {
        checks.extend(probes(inputs, config, &mut client).await);
        checks.extend(formats(inputs, config, &mut client).await);
    }
    if let Some(config) = &config {
        checks.extend(if client.is_some() {
            served_config(config, &served(inputs, config, &mut client).await)
        } else {
            vec![Check::skip("served_config", "the daemon did not answer")]
        });
    }
    if let Some(config) = &config {
        checks.extend(channels(inputs, config, &mut client).await);
    }
    checks.push(log(inputs));
    checks
}

/// `notify` of each configured identity (of `--identity` only, when given), from
/// `notify.channel.list` on the client that answered `hello`; skipped when none did.
async fn channels(inputs: &Inputs, config: &Config, client: &mut Option<Client>) -> Vec<Check> {
    let mut checks = Vec::new();
    for name in config.identities.keys() {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let params = IdentityParams {
            identity: name.clone(),
        };
        let answer =
            ask::<method::NotifyChannelList>(client, bosun_client::CONNECT_TIMEOUT, &params).await;
        checks.extend(notify(name, answer));
    }
    checks
}

/// `notify` (#9) of `identity` from the daemon's answer to `notify.channel.list` (`None`: the
/// daemon did not answer): one check per channel, naming it, its kind and whether its secret
/// is in the credential store, never the secret's value.
pub(crate) fn notify(identity: &str, answer: Option<Result<Channels, Error>>) -> Vec<Check> {
    let checks = match answer {
        None => vec![Check::skip("notify", "the daemon did not answer")],
        Some(Ok(list)) if list.channels.is_empty() => vec![Check::ok("notify", "no channels")],
        Some(Ok(list)) => list
            .channels
            .iter()
            .map(|c| {
                // Mutant (`mutant_doctor_notify_secret_unchecked`): a missing secret is ok.
                if c.secret_present || cfg!(mutant_doctor_notify_secret_unchecked) {
                    Check::ok(
                        "notify",
                        format!("channel {} ({}): secret present", c.name, c.kind),
                    )
                } else {
                    Check::fail(
                        "notify",
                        format!(
                            "channel {} ({}): its secret is missing from the credential store \
                             (entry {})",
                            c.name, c.kind, c.secret_entry
                        ),
                        format!(
                            "remove it and add it again: `bosun notify remove {0} --identity \
                             {identity}`, then `bosun notify add {0} --kind {1} ... --identity \
                             {identity}` with the secret on standard input",
                            c.name, c.kind
                        ),
                    )
                }
            })
            .collect(),
        Some(Err(Error::Rpc(e))) if e.code == code::METHOD_NOT_FOUND => vec![Check::warn(
            "notify",
            format!(
                "this bosund does not know notify.channel.list: {}",
                e.message
            ),
            "install bosun and bosund of one release",
        )],
        Some(Err(e)) => vec![Check::fail(
            "notify",
            format!("notify.channel.list: {e}"),
            "run bosund in a terminal to see why it does not answer",
        )],
    };
    checks.into_iter().map(|c| c.of(identity)).collect()
}

/// The daemon's log file (#137): where to look when the daemon stops or refuses an attempt,
/// since the logon task discards its standard error.
fn log(inputs: &Inputs) -> Check {
    let (Some(config), Ok(endpoint)) = (
        inputs.config.clone().or_else(bosun_config::default_path),
        &inputs.endpoint,
    ) else {
        return Check::skip("log", "no configuration file or endpoint to place it by");
    };
    let file = bosun_client::log::path(&config, endpoint);
    match std::fs::metadata(&file) {
        Ok(meta) => Check::ok("log", format!("{} ({} bytes)", file.display(), meta.len())),
        Err(_) => Check::skip(
            "log",
            format!(
                "{}: not written yet; the daemon writes it from its start",
                file.display()
            ),
        ),
    }
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
        if let Some(identity) = config.identities.get(&repo.identity) {
            checks.push(
                auto_memory(name, &identity.claude_config_dir, &repo.path).of(&repo.identity),
            );
        }
    }
}

/// Claude Code's project directory name for `root`: every character other than an ASCII
/// letter or digit becomes `-` (`C:\src\app` is `C--src-app`).
fn project_dir_name(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The auto memory directory of repository `name` at `root` under `config_dir` (#135): named
/// when it exists, since sessions bosun ran before #135 may have written into it.
pub(crate) fn auto_memory(name: &str, config_dir: &Path, root: &Path) -> Check {
    let dir = config_dir
        .join("projects")
        .join(project_dir_name(root))
        .join("memory");
    if dir.is_dir() {
        Check::ok(
            "auto_memory",
            format!("repo.{name}: auto memory at {}", dir.display()),
        )
        .fix(
            "bosun sessions no longer read or write it (#135); review it for notes that \
             sessions bosun ran before wrote, and remove what you did not mean to keep",
        )
    } else {
        Check::ok(
            "auto_memory",
            format!("repo.{name}: no auto memory at {}", dir.display()),
        )
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
    #[allow(
        clippy::disallowed_methods,
        reason = "the CLI's children share the user's console on purpose"
    )]
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

/// `endpoint`, `endpoint_acl` and `daemon`: the client that answered `hello`, if one did.
async fn daemon(endpoint: &Result<Endpoint, String>, checks: &mut Vec<Check>) -> Option<Client> {
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
            return None;
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
            return None;
        }
    };
    checks.push(Check::ok("endpoint", format!("{endpoint} is reachable")));
    checks.push(endpoint_acl(endpoint));
    let hello = tokio::time::timeout(bosun_client::CONNECT_TIMEOUT, Client::over(stream))
        .await
        .unwrap_or(Err(Error::Timeout));
    let mut answered = None;
    checks.push(match hello {
        Ok(client) => {
            let hello = client.daemon();
            let check = Check::ok(
                "daemon",
                format!(
                    "bosund {} (pid {}, API version {}) answers",
                    hello.daemon_version, hello.pid, hello.api_version
                ),
            );
            answered = Some(client);
            check
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
    answered
}

/// How long `doctor.probe` may take: the daemon gives its `gh --version` 30 s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(40);

/// Calls `M` on `client` within `limit`: `None` when there is no client to ask. A call that
/// times out leaves its reply unread on the connection, where the next call would read it as
/// its own (and fail on its id): so its connection is dropped, and nothing more is asked.
async fn ask<M: Method>(
    client: &mut Option<Client>,
    limit: Duration,
    params: &M::Params,
) -> Option<Result<M::Result, Error>> {
    let answered = tokio::time::timeout(limit, client.as_mut()?.call::<M>(params)).await;
    if answered.is_err() && !cfg!(mutant_doctor_timeout_keeps_client) {
        // Mutant (`mutant_doctor_timeout_keeps_client`): the connection is kept, so the next
        // call reads the late reply and fails.
        *client = None;
    }
    Some(answered.unwrap_or(Err(Error::Timeout)))
}

/// `daemon_gh` of each configured identity (of `--identity` only, when given): `doctor.probe`
/// on the client that answered `hello`, or skipped when none did (or an earlier call timed
/// out, which drops it).
async fn probes(inputs: &Inputs, config: &Config, client: &mut Option<Client>) -> Vec<Check> {
    let mut checks = Vec::new();
    for name in config.identities.keys() {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let params = IdentityParams {
            identity: name.clone(),
        };
        checks.push(
            match ask::<method::DoctorProbe>(client, PROBE_TIMEOUT, &params).await {
                Some(answer) => daemon_gh(name, answer),
                None => Check::skip("daemon_gh", "the daemon did not answer").of(name),
            },
        );
    }
    checks
}

/// `daemon_gh` (#131) of `identity` from the daemon's answer to `doctor.probe`: whether the
/// daemon itself could start a child, which a check run in this process cannot tell.
pub(crate) fn daemon_gh(identity: &str, answer: Result<ProbeResult, Error>) -> Check {
    // Mutant (`mutant_doctor_daemon_gh_ignored`): the check is ok whatever the daemon answered.
    if cfg!(mutant_doctor_daemon_gh_ignored) {
        return Check::ok("daemon_gh", "bosund was asked to start gh").of(identity);
    }
    let check = match answer {
        Ok(probe) if probe.ok => {
            Check::ok("daemon_gh", format!("bosund started gh: {}", probe.detail))
        }
        Ok(probe) => Check::fail(
            "daemon_gh",
            format!("bosund cannot start {}: {}", probe.program, probe.detail),
            "the daemon cannot start a child process (for one, a daemon whose console host was \
             ended): run `bosun install`, which stops it by its pid and starts it again",
        ),
        Err(Error::Rpc(e)) if e.code == code::METHOD_NOT_FOUND => Check::warn(
            "daemon_gh",
            format!("this bosund does not know doctor.probe: {}", e.message),
            "install bosun and bosund of one release",
        ),
        Err(e) => Check::fail(
            "daemon_gh",
            format!("doctor.probe: {e}"),
            "run bosund in a terminal to see why it does not answer",
        ),
    };
    check.of(identity)
}

/// The share of unbilled usage lines, or of lines of unknown record types, above which
/// `ingest_format` warns, in percent.
const DRIFT_PERCENT: u64 = 1;

/// `ingest_format` of each configured identity (of `--identity` only, when given), from
/// `ingest.status` on the client that answered `hello`; skipped when none did.
async fn formats(inputs: &Inputs, config: &Config, client: &mut Option<Client>) -> Vec<Check> {
    let mut checks = Vec::new();
    for name in config.identities.keys() {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let params = IdentityParams {
            identity: name.clone(),
        };
        checks.push(
            match ask::<method::IngestStatus>(client, PROBE_TIMEOUT, &params).await {
                Some(Ok(status)) => ingest_format(name, &status),
                Some(Err(e)) => Check::warn(
                    "ingest_format",
                    format!("ingest.status: {e}"),
                    "see `served_config`; run bosund in a terminal to see why it does not answer",
                )
                .of(name),
                None => Check::skip("ingest_format", "the daemon did not answer").of(name),
            },
        );
    }
    checks
}

/// Whether `n` is more than [`DRIFT_PERCENT`] percent of `d`, in integers.
fn above_share(n: u64, d: u64) -> bool {
    u128::from(n) * 100 > u128::from(d) * u128::from(DRIFT_PERCENT)
}

/// `ingest_format` (#6) of `identity` from its `ingest.status`: a warning when more than
/// [`DRIFT_PERCENT`] percent of its usage-bearing lines billed nothing, or of its lines are of
/// unknown record types, as when the provider's transcript format moved under the parser.
pub(crate) fn ingest_format(identity: &str, status: &IngestStatus) -> Check {
    let drift = &status.drift;
    if drift.counted_files == 0 {
        return Check::ok(
            "ingest_format",
            "nothing counted yet; `bosun reingest` counts every file",
        )
        .of(identity);
    }
    let counts = format!(
        "{} usage records unbilled, {} of unknown record types",
        crate::render::share(drift.unbilled, drift.usage, ""),
        crate::render::share(drift.unknown, drift.lines, " lines"),
    );
    // Mutant (`mutant_doctor_ingest_format_ignored`): the check is ok whatever the counts.
    let drifted = !cfg!(mutant_doctor_ingest_format_ignored)
        && (above_share(drift.unbilled, drift.usage) || above_share(drift.unknown, drift.lines));
    let check = if drifted {
        let files = drift.files.len() as u64 + drift.more_files;
        Check::warn(
            "ingest_format",
            format!("{counts}, in {files} drifted files"),
            "the transcript format may have changed: `bosun ingest status` names the files; \
             install the newest bosun, then run `bosun reingest`",
        )
    } else {
        Check::ok("ingest_format", counts)
    };
    check.of(identity)
}

/// What [`served_config`] takes for an identity the daemon answers it does not serve.
pub(crate) const NOT_SERVED: &str = "the daemon serves no such identity";

/// The end of every difference [`served_config`] names, as the daemon's reload line says it.
const NEXT_START: &str = "takes effect at the next daemon start";

/// The fix of every difference [`served_config`] names.
const RESTART: &str = "restart the daemon (log in again, or run bosun install)";

/// `status.overview` of each configured identity (of `--identity` only, when given), asked on
/// the client that answered `hello`: the repositories the daemon serves, [`NOT_SERVED`], or
/// another error's text.
async fn served(
    inputs: &Inputs,
    config: &Config,
    client: &mut Option<Client>,
) -> BTreeMap<String, Result<Vec<RepoOverview>, String>> {
    let mut served = BTreeMap::new();
    for name in config.identities.keys() {
        if inputs.identity.as_ref().is_some_and(|only| only != name) {
            continue;
        }
        let params = IdentityParams {
            identity: name.clone(),
        };
        let asked = ask::<method::StatusOverview>(client, bosun_client::CONNECT_TIMEOUT, &params)
            .await
            .unwrap_or_else(|| {
                Err(Error::Protocol(
                    "not asked: the daemon did not answer an earlier call in time".to_owned(),
                ))
            });
        let answer = match asked {
            Ok(overview) => Ok(overview.repos),
            Err(Error::Rpc(e)) if e.code == code::UNKNOWN_IDENTITY => Err(NOT_SERVED.to_owned()),
            Err(e) => Err(e.to_string()),
        };
        served.insert(name.clone(), answer);
    }
    served
}

/// The kind of a configured queue, as `status.overview` names it.
fn queue_kind(queue: Option<&Queue>) -> &'static str {
    match queue {
        Some(Queue::LocalFile { .. }) => "local_file",
        Some(Queue::Projects { .. }) => "projects_v2",
        Some(_) => "other",
        None => "none",
    }
}

/// Where a configured queue is, as `status.overview` names it (`queue_source`).
fn queue_source(queue: Option<&Queue>) -> Option<String> {
    match queue {
        Some(Queue::LocalFile { file }) => Some(file.display().to_string()),
        Some(Queue::Projects { owner, number }) => Some(format!("{owner}/{number}")),
        Some(_) | None => None,
    }
}

/// `served_config` (#186): the configuration against what the daemon serves, per identity of
/// `served` (what [`served`] asked). Each difference that only a daemon start applies is a
/// warning naming the repository or identity; none is one `ok`.
pub(crate) fn served_config(
    config: &Config,
    served: &BTreeMap<String, Result<Vec<RepoOverview>, String>>,
) -> Vec<Check> {
    // Mutant (`mutant_doctor_ignores_served_config`): nothing is compared.
    if cfg!(mutant_doctor_ignores_served_config) {
        return vec![Check::ok(
            "served_config",
            "the daemon serves the configuration as written",
        )];
    }
    let warn =
        |identity: &str, detail: String| Check::warn("served_config", detail, RESTART).of(identity);
    let served_under = |repo: &str| {
        served.iter().find_map(|(identity, repos)| {
            let repos = repos.as_ref().ok()?;
            repos
                .iter()
                .any(|r| r.repo == repo)
                .then_some(identity.as_str())
        })
    };
    let mut checks = Vec::new();
    for (identity, answer) in served {
        let repos = match answer {
            Ok(repos) => repos,
            Err(e) if e == NOT_SERVED => {
                checks.push(warn(
                    identity,
                    format!(
                        "identity {identity} is added: its store, ingest and loops take effect \
                         at the next daemon start"
                    ),
                ));
                continue;
            }
            Err(e) => {
                checks.push(
                    Check::warn(
                        "served_config",
                        format!("identity {identity}: the daemon's overview cannot be read: {e}"),
                        "run bosun doctor again; the daemon's log says why",
                    )
                    .of(identity),
                );
                continue;
            }
        };
        for (name, repo) in config.repos.iter().filter(|(_, r)| r.identity == *identity) {
            let Some(found) = repos.iter().find(|r| r.repo == *name) else {
                let detail = if served_under(name).is_some() {
                    format!("repository {name}: its identity changed: {NEXT_START}")
                } else {
                    format!("repository {name} is added: its loop {NEXT_START}")
                };
                checks.push(warn(identity, detail));
                continue;
            };
            let queue = repo.queue.as_ref();
            for (field, changed) in [
                ("queue", found.queue != queue_kind(queue)),
                ("queue", found.queue_source != queue_source(queue)),
                ("path", found.path != repo.path.display().to_string()),
            ] {
                let detail = format!("repository {name}: its {field} changed: {NEXT_START}");
                if changed && !checks.iter().any(|c| c.detail == detail) {
                    checks.push(warn(identity, detail));
                }
            }
        }
        for found in repos {
            match config.repos.get(&found.repo) {
                None => checks.push(warn(
                    identity,
                    format!(
                        "repository {} is removed: its loop claims nothing new and its item in \
                         flight goes to Needs you; the rest {NEXT_START}",
                        found.repo
                    ),
                )),
                // Configured under another identity: named under that one above when it was
                // asked too, else (`--identity`) here.
                Some(repo) if repo.identity != *identity => {
                    let detail = format!(
                        "repository {}: its identity changed: {NEXT_START}",
                        found.repo
                    );
                    // Mutant (`mutant_doctor_misses_identity_move`): a served repository
                    // configured under another identity is not compared.
                    if !cfg!(mutant_doctor_misses_identity_move)
                        && !checks.iter().any(|c| c.detail == detail)
                    {
                        checks.push(warn(identity, detail));
                    }
                }
                Some(_) => {}
            }
        }
    }
    if checks.is_empty() {
        checks.push(Check::ok(
            "served_config",
            "the daemon serves the configuration as written",
        ));
    }
    checks
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
    fn auto_memory_names_the_repository_s_memory_directory() {
        let config = tempfile::tempdir().unwrap();
        let root = Path::new("/fixture/repo-a");
        assert_eq!(project_dir_name(Path::new("C:/src/app")), "C--src-app");
        let absent = auto_memory("a", config.path(), root);
        assert_eq!(absent.status, Status::Ok);
        assert!(absent.detail.contains("no auto memory"), "{absent:?}");
        assert!(absent.fix.is_none());
        let dir = config
            .path()
            .join("projects")
            .join(project_dir_name(root))
            .join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let present = auto_memory("a", config.path(), root);
        assert!(
            present.detail.contains(&dir.display().to_string()),
            "{present:?}"
        );
        assert!(present.fix.is_some(), "{present:?}");
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

    /// #9: `notify` names each channel of the identity, its kind and whether its secret is
    /// present, and fails a channel whose secret is missing, with the fix; no channel is ok,
    /// and a daemon that did not answer skips it (`mutant_doctor_notify_secret_unchecked`
    /// reports every channel ok).
    #[test]
    fn the_notify_check_names_each_channel_and_whether_its_secret_is_present() {
        let channel = |name: &str, present: bool| bosun_api::ChannelView {
            name: name.to_owned(),
            kind: "fake".to_owned(),
            params: BTreeMap::new(),
            events: vec!["needs_you".to_owned()],
            min_gap_s: 60,
            secret_entry: format!("notify/a/{name}"),
            secret_present: present,
            last_sent_ms: None,
        };
        let list = Channels {
            identity: "a".to_owned(),
            channels: vec![channel("ops", true), channel("pager", false)],
        };
        let checks = notify("a", Some(Ok(list)));
        assert_eq!(checks.len(), 2, "{checks:?}");
        let (ops, pager) = (&checks[0], &checks[1]);
        assert_eq!(ops.status, Status::Ok, "{ops:?}");
        assert_eq!(ops.identity.as_deref(), Some("a"));
        assert!(
            ops.detail.contains("ops") && ops.detail.contains("fake"),
            "{ops:?}"
        );
        assert!(ops.detail.contains("secret present"), "{ops:?}");
        assert_eq!(pager.status, Status::Fail, "{pager:?}");
        assert!(pager.detail.contains("pager"), "{pager:?}");
        assert!(
            pager
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("bosun notify remove pager")
                    && f.contains("bosun notify add pager")),
            "{pager:?}"
        );

        let none = notify(
            "a",
            Some(Ok(Channels {
                identity: "a".to_owned(),
                channels: Vec::new(),
            })),
        );
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].status, Status::Ok);
        assert_eq!(none[0].detail, "no channels");

        let down = notify("a", None);
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].status, Status::Skip);
        assert_eq!(down[0].identity.as_deref(), Some("a"));
    }

    /// #131: `daemon_gh` fails, with the identity, the daemon's reason and `bosun install` as
    /// the fix, when the daemon could not start its `gh`; it warns for a daemon that does not
    /// know `doctor.probe` (`mutant_doctor_daemon_gh_ignored` makes it ok whatever the answer).
    /// `ingest_format` (#6) warns when more than 1% of the usage lines billed nothing or of
    /// the lines are of unknown record types, and says to reingest with the newest bosun
    /// (`mutant_doctor_ingest_format_ignored` makes it ok whatever the counts).
    #[test]
    fn the_ingest_format_check_warns_above_the_share() {
        let status = |lines: u64, unknown: u64, usage: u64, unbilled: u64| IngestStatus {
            identity: "a".to_owned(),
            database: "bosun.db".to_owned(),
            transcripts: "projects".to_owned(),
            files: 1,
            size: bosun_api::Wide(0),
            parsed: bosun_api::Wide(0),
            parsed_since_start: bosun_api::Wide(0),
            last_pass: bosun_api::LastPass::Pending,
            drift: bosun_api::IngestDrift {
                counted_files: 1,
                lines,
                unknown,
                usage,
                unbilled,
                ..bosun_api::IngestDrift::default()
            },
        };
        let clean = ingest_format("a", &status(1000, 0, 400, 0));
        assert_eq!(clean.status, Status::Ok, "{clean:?}");
        assert_eq!(clean.identity.as_deref(), Some("a"));
        let at = ingest_format("a", &status(1000, 0, 400, 4));
        assert_eq!(at.status, Status::Ok, "exactly 1% is ok: {at:?}");

        let above = ingest_format("a", &status(1000, 0, 400, 5));
        assert_eq!(above.status, Status::Warn, "{above:?}");
        assert!(above.detail.contains("5 of 400"), "{above:?}");
        assert!(
            above
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("bosun reingest")),
            "{above:?}"
        );
        let unknown = ingest_format("a", &status(100, 2, 0, 0));
        assert_eq!(unknown.status, Status::Warn, "{unknown:?}");
        assert!(unknown.detail.contains("2 of 100"), "{unknown:?}");

        let mut none = status(0, 0, 0, 0);
        none.drift = bosun_api::IngestDrift::default();
        let none = ingest_format("a", &none);
        assert_eq!(none.status, Status::Ok, "{none:?}");
        assert!(none.detail.contains("nothing counted yet"), "{none:?}");
    }

    #[test]
    fn the_daemon_gh_check_fails_when_the_daemon_cannot_start_gh() {
        let probe = |ok: bool, detail: &str| ProbeResult {
            identity: "a".to_owned(),
            program: "gh".to_owned(),
            ok,
            detail: detail.to_owned(),
        };
        let failed = daemon_gh(
            "a",
            Ok(probe(false, "gh --version ended with exit code 3: gone")),
        );
        assert_eq!(failed.status, Status::Fail, "{failed:?}");
        assert_eq!(failed.identity.as_deref(), Some("a"));
        assert!(failed.detail.contains("exit code 3: gone"), "{failed:?}");
        assert!(
            failed
                .fix
                .as_deref()
                .is_some_and(|f| f.contains("bosun install")),
            "{failed:?}"
        );

        let ran = daemon_gh("a", Ok(probe(true, "gh version 2.0.0 (demo)")));
        assert_eq!(ran.status, Status::Ok, "{ran:?}");
        assert!(ran.detail.contains("gh version 2.0.0 (demo)"), "{ran:?}");

        let unknown = daemon_gh(
            "a",
            Err(Error::Rpc(bosun_api::rpc::ErrorObject::new(
                code::METHOD_NOT_FOUND,
                "no method doctor.probe",
            ))),
        );
        assert_eq!(unknown.status, Status::Warn, "{unknown:?}");

        assert_eq!(overall(&[ran, unknown, failed]), Status::Fail);
    }

    /// #186: what the daemon serves against the file, through `served_config` alone (no
    /// endpoint is opened).
    #[test]
    fn the_doctor_names_what_waits_for_the_next_daemon_start() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for sub in ["claude", "claude-other", "demo", "beta"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let board = d.join("board.toml");
        let text = format!(
            "[identity.test]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\n\
             [identity.other]\nclaude_config_dir = '{}'\ndatabase = '{}'\n\n\
             [repo.demo]\npath = '{}'\nidentity = 'test'\n\
             queue = {{ kind = 'local_file', file = '{}' }}\nmodel = 'opus'\n\n\
             [repo.beta]\npath = '{}'\nidentity = 'test'\n",
            d.join("claude").display(),
            d.join("test.db").display(),
            d.join("claude-other").display(),
            d.join("other.db").display(),
            d.join("demo").display(),
            board.display(),
            d.join("beta").display(),
        );
        let config = bosun_config::parse(&text, &NoProbe).unwrap();
        let overview = |repo: &str, source: Option<String>| RepoOverview {
            repo: repo.to_owned(),
            path: config.repos[repo].path.display().to_string(),
            queue: queue_kind(config.repos[repo].queue.as_ref()).to_owned(),
            queue_source: source,
            state: "idle".to_owned(),
            hold: None,
            tripped: None,
            current: None,
            last_verdict: None,
            waiting: Vec::new(),
        };
        let moved = d.join("moved.toml").display().to_string();
        let served = BTreeMap::from([
            ("other".to_owned(), Err(NOT_SERVED.to_owned())),
            ("test".to_owned(), Ok(vec![overview("demo", Some(moved))])),
        ]);
        let checks = served_config(&config, &served);
        assert!(
            checks.iter().all(|c| c.status == Status::Warn
                && c.detail.contains("next daemon start")
                && c.fix.is_some()),
            "{checks:#?}"
        );
        let details: Vec<&str> = checks.iter().map(|c| c.detail.as_str()).collect();
        assert_eq!(checks.len(), 3, "{details:#?}");
        for named in [
            "repository beta is added",
            "repository demo: its queue changed",
            "identity other is added",
        ] {
            assert!(
                details.iter().any(|d| d.starts_with(named)),
                "{named}: {details:#?}"
            );
        }

        let matching = BTreeMap::from([
            ("other".to_owned(), Ok(Vec::new())),
            (
                "test".to_owned(),
                Ok(vec![
                    overview("beta", None),
                    overview("demo", Some(board.display().to_string())),
                ]),
            ),
        ]);
        let checks = served_config(&config, &matching);
        assert_eq!(checks.len(), 1, "{checks:#?}");
        assert_eq!(checks[0].status, Status::Ok, "{checks:#?}");

        // With `--identity test` only `test` is asked: a repository it serves that the file
        // now gives to `other` is named all the same, once.
        let beta = d.join("beta").display().to_string();
        let moved_away = text.replace(
            &format!("path = '{beta}'\nidentity = 'test'\n"),
            &format!("path = '{beta}'\nidentity = 'other'\n"),
        );
        let config = bosun_config::parse(&moved_away, &NoProbe).unwrap();
        assert_eq!(config.repos["beta"].identity, "other");
        let only_test = BTreeMap::from([(
            "test".to_owned(),
            Ok(vec![
                overview("beta", None),
                overview("demo", Some(board.display().to_string())),
            ]),
        )]);
        let checks = served_config(&config, &only_test);
        let details: Vec<&str> = checks.iter().map(|c| c.detail.as_str()).collect();
        assert_eq!(
            details,
            ["repository beta: its identity changed: takes effect at the next daemon start"],
            "{checks:#?}"
        );
        // Both asked: it is named once.
        let both = BTreeMap::from([
            ("other".to_owned(), Ok(Vec::new())),
            ("test".to_owned(), only_test["test"].clone()),
        ]);
        assert_eq!(served_config(&config, &both).len(), 1);
    }

    /// #192: a call that times out leaves its reply unread on the connection, so the client
    /// is dropped and nothing more is asked on it (the next call would read that reply as its
    /// own and fail).
    #[tokio::test]
    async fn a_call_that_times_out_drops_its_connection() {
        use bosun_api::rpc::{Request, Response};
        use bosun_api::{API_VERSION, HelloResult};
        use bosun_client::framing;

        let (near, far) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(far);
            let mut read = tokio::io::BufReader::new(read);
            while let Ok(Some(line)) = framing::read_line(&mut read).await {
                let request: Request = serde_json::from_str(&line).unwrap();
                let id = request.id.clone().unwrap();
                let result = if request.method == "hello" {
                    serde_json::to_value(HelloResult {
                        api_version: API_VERSION,
                        daemon_version: "0.0.0".to_owned(),
                        pid: 1,
                    })
                    .unwrap()
                } else {
                    // Every probe is answered late.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    serde_json::to_value(ProbeResult {
                        identity: "test".to_owned(),
                        ok: true,
                        program: "gh".to_owned(),
                        detail: "gh version 0.0.0".to_owned(),
                    })
                    .unwrap()
                };
                let response = Response::result(id, result);
                if framing::write_message(&mut write, &response).await.is_err() {
                    return;
                }
            }
        });
        let mut client = Some(Client::over(Box::new(near)).await.unwrap());
        let params = IdentityParams {
            identity: "test".to_owned(),
        };
        let limit = Duration::from_millis(100);
        let first = ask::<method::DoctorProbe>(&mut client, limit, &params).await;
        assert!(matches!(first, Some(Err(Error::Timeout))), "{first:?}");
        assert!(client.is_none(), "the connection is dropped");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let second = ask::<method::DoctorProbe>(&mut client, limit, &params).await;
        assert!(second.is_none(), "nothing more is asked: {second:?}");
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
