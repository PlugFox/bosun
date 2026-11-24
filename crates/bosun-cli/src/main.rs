//! `bosun`, the command line of bosun (#4, #25): a client of the daemon's local API.
//!
//! ```text
//! bosun ingest status [--identity <name>]
//! bosun tokens [--by session|repo|model] [--since <t>] [--until <t>] [--identity <name>]
//! bosun reingest [--identity <name>]
//! bosun install [--daemon <path>] [--name <name>]
//! bosun uninstall [--name <name>]
//! bosun doctor [--identity <name>] [--name <name>]
//! bosun report [--since <t>] [--until <t>] [--repo <name>] [--identity <name>]
//! bosun pause [--repo <name>] [--identity <name>]
//! bosun resume [--repo <name>] [--identity <name>]
//! bosun kill [--release]
//! bosun restart-item <repo>#<card>
//! bosun status [--identity <name>]
//! bosun queue [--repo <name>] [--identity <name>]
//! bosun item <repo>#<card>
//! bosun lint <repo>#<card> | bosun lint --repo <name> <card>
//! bosun session tail <repo>#<card> | <session id>
//! ```
//!
//! Every command also takes `--format text|json` (default `text`; `report` also takes `md`,
//! and its text is Markdown either way), `--config <file>` (default
//! `bosun_config::default_path`: `BOSUN_CONFIG`, else the platform's location) and
//! `--endpoint <socket or pipe>` (default `bosun_client::Endpoint::resolve`: `BOSUN_ENDPOINT`,
//! else per user).
//!
//! # Identities
//!
//! With `--identity`, the command asks about that identity only; the configuration is not
//! read, and the daemon decides whether it serves the name. Without it, the command asks about
//! every identity of the configuration, in name order, one request each, and shows each in a
//! section of its own: identities are never summed (#2, rule 4). A configuration with no
//! identity prints nothing (`{}` in JSON) and says so on stderr.
//!
//! # Commands
//!
//! - `ingest status`: the identity's database and transcript directory, its source files,
//!   bytes seen and parsed, and the last ingest pass (`ingest.status`).
//! - `tokens`: token sums of the identity's messages per context window (`--by session`), per
//!   repository (`--by repo`, the default) or per model (`--by model`), with their eqv and
//!   cost. `--since` is inclusive, `--until` exclusive; both are an RFC 3339 date-time, a date
//!   (midnight UTC) or a duration before now such as `7d` (see `time.rs`).
//! - `reingest`: re-reads every transcript file of the identity from zero with the current
//!   parser and answers when the pass is done (`ingest.reingest`). Nothing is deleted. Files
//!   that could not be read are listed; they do not change the exit code.
//! - `report`: the exact sections of the report (`report.exact`, #6, #26): eqv by category,
//!   model and session kind, eqv per commit against the previous period of the same length,
//!   the old method's number next to the real total, the context curve per context window,
//!   and ingest health with the agreement with the provider's own totals; then, apart, its
//!   estimate sections (`report.estimates`, #27): the cost of tool results by tool and by file
//!   with cache carry, repeated reads, and the cost of finding. `--until` defaults to now and `--since` to seven days
//!   before `--until`. `--repo` limits the range sections to one configured repository; without
//!   `--identity`, only the identity that owns it is asked.
//! - `install`: registers the logon process that starts `bosund` for the current user, and
//!   starts it now (`install.rs`). `bosund` is the one next to `bosun` unless `--daemon` names
//!   another; `--config` and `--endpoint` (or `BOSUN_CONFIG` and `BOSUN_ENDPOINT`), when set,
//!   are passed to it. Idempotent. When the daemon does not answer after the start, install
//!   still succeeds and says so on stderr: the registration is in place.
//! - `uninstall`: stops that daemon and removes everything install created.
//! - `doctor`: checks the registration, the configuration, each identity's directories,
//!   Claude Code's transcript retention, the tools on the daemon's PATH (`git`, `sh`, `claude`,
//!   `gh`), each identity's GitHub login, and the endpoint and who may open it, with the daemon
//!   up or down (`doctor.rs`). It prints every check, failed or not; it exits 0 when none failed
//!   (warnings allowed) and 7 when one did.
//!
//! - `pause` and `resume`: pause one repository (`--repo`) or every repository of each
//!   identity, and lift it (`control.pause`, `control.resume`); the attempt in flight finishes
//!   and no new session starts. `kill` throws the daemon-wide kill switch (`control.kill`):
//!   every child is killed, every item parks, no board is written; `kill --release` releases
//!   it. `restart-item` puts the `restart` label on the card of an item in flight
//!   (`item.restart`): the loop kills its session, takes a new snapshot and starts again. All
//!   four send the daemon's operator token, read from its file, and are the rows of the action
//!   table the TUI shares (`bosun_client::actions`, `control.rs`).
//! - `status`: per repository, its state, the item in flight with its attempt and time in
//!   state, the last verdict and the hold (`status.overview`). `queue`: each board's columns
//!   with each card's state, snapshot hash, a pending edit and the lint findings of Ready
//!   cards (`queue.get`). `item`: one item's state, snapshot, attempts, sessions, verdict runs,
//!   agent calls and transitions (`item.get`). `lint`: the static lint of one card, through
//!   the daemon (`queue.lint`); a card that is not claimable exits 1 with its findings on
//!   stdout. `session tail`: follows a session's output lines until it ends (`session.get`,
//!   then the event stream's live tail). See `work.rs`.
//!
//! `--name` names the registration (default `bosun`): a second daemon, or a test.
//!
//! # JSON
//!
//! With `--identity`, `--format json` prints one document:
//!
//! | Command | Document |
//! | --- | --- |
//! | `ingest status` | the result of `ingest.status` |
//! | `reingest` | the result of `ingest.reingest` |
//! | `tokens --by session` | `recount`'s compare shape version 1 (`shape.rs`), so that `recount --compare <file>` reads it |
//! | `tokens --by repo` | the result of `tokens.by_repo` |
//! | `tokens --by model` | the result of `tokens.by_model` |
//! | `report` | `{"exact": <the result of report.exact>, "estimates": <the result of report.estimates>}` |
//! | `status` | the result of `status.overview` |
//! | `queue` | the result of `queue.get` |
//! | `pause`, `resume` | the result of `control.pause`, `control.resume` |
//!
//! The results are those of `schema/bosun-api.schema.json`. Without `--identity`, the output
//! is an object keyed by identity name whose values are those documents:
//! `{"personal": {...}, "work": {...}}`.
//!
//! These name one item, card or the daemon, and print one document whatever `--identity`:
//!
//! | Command | Document |
//! | --- | --- |
//! | `item` | the result of `item.get` |
//! | `lint` | the result of `queue.lint` |
//! | `restart-item` | the result of `item.restart` |
//! | `kill` | the result of `control.kill` |
//! | `session tail` | one `Event` (the params of `events.event`) per line, as they come |
//!
//! # Exit codes
//!
//! They are stable; [`EXIT_CODES`] is the text `--help` prints, and the tests hold each code
//! to it.
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | success |
//! | 1 | failed: the daemon refused the request, or the connection failed |
//! | 2 | bad arguments |
//! | 3 | the configuration could not be read or is invalid |
//! | 4 | the daemon is not running |
//! | 5 | the identity is unknown to the daemon |
//! | 6 | the daemon speaks an API version this `bosun` does not know |
//! | 7 | `doctor`: a check did not pass |
//!
//! Errors go to stderr, prefixed `bosun: `; nothing is printed on stdout then, except by
//! `doctor`, which prints its checks with 0 and with 7 alike, and `lint`, which prints the
//! findings of a card that is not claimable with 1.

mod admin;
mod artefact;
mod control;
mod doctor;
mod install;
mod markdown;
mod render;
mod shape;
mod time;
mod work;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use bosun_api::rpc::code;
use bosun_api::{
    IdentityParams, IngestStatus, ModelRow, ReingestResult, RepoRow, ReportParams, SessionRow,
    TokensParams, TokensSummary, method,
};
use bosun_client::{Client, Endpoint, Error};
use bosun_config::SystemProbe;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

use crate::time::Bound;

/// Exit codes; see the crate documentation and [`EXIT_CODES`].
mod exit {
    pub(crate) const FAILED: u8 = 1;
    pub(crate) const USAGE: u8 = 2;
    pub(crate) const CONFIG: u8 = 3;
    pub(crate) const NOT_RUNNING: u8 = 4;
    pub(crate) const UNKNOWN_IDENTITY: u8 = 5;
    pub(crate) const VERSION: u8 = 6;
    pub(crate) const CHECK_FAILED: u8 = 7;
}

/// The exit codes as `--help` prints them.
const EXIT_CODES: &str = "\
Exit codes:
  0  success
  1  failed: the daemon refused the request, or the connection failed
  2  bad arguments
  3  the configuration could not be read or is invalid
  4  the daemon is not running
  5  the identity is unknown to the daemon
  6  the daemon speaks an API version this bosun does not know
  7  doctor: a check did not pass";

/// The command line of bosun: token sums, the report, ingest status and reingest, asked of
/// the daemon.
#[derive(Debug, Parser)]
#[command(name = "bosun", version, after_help = EXIT_CODES)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    global: Global,
}

#[derive(Debug, Args)]
struct Global {
    /// Output format. JSON with --identity is one document; without it, an object keyed by
    /// identity name. md is for report only, whose text is Markdown either way.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// The identity to ask about. Without it, every identity of the configuration, each in a
    /// section of its own; identities are never summed.
    #[arg(long, global = true, value_name = "NAME")]
    identity: Option<String>,
    /// The configuration file [default: `BOSUN_CONFIG`, else the platform's location].
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The daemon's socket or pipe [default: `BOSUN_ENDPOINT`, else per user].
    #[arg(long, global = true, value_name = "ENDPOINT")]
    endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
    /// Markdown: `report` only, whose text is Markdown too.
    Md,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The ingest of transcripts.
    #[command(subcommand, after_help = EXIT_CODES)]
    Ingest(IngestCommand),
    /// Token sums of each identity's messages, with eqv and cost.
    #[command(after_help = EXIT_CODES)]
    Tokens(TokensArgs),
    /// Re-reads every transcript file from zero with the current parser; deletes nothing.
    #[command(after_help = EXIT_CODES)]
    Reingest,
    /// The report: exact eqv by category, model, session kind and commit, the old method's
    /// number, the context curve and ingest health; then, apart, the estimates of tool and
    /// file cost.
    #[command(after_help = EXIT_CODES)]
    Report(ReportArgs),
    /// Registers the logon process that starts bosund for you, and starts it now.
    #[command(after_help = EXIT_CODES)]
    Install(InstallArgs),
    /// Stops the daemon and removes the logon registration.
    #[command(after_help = EXIT_CODES)]
    Uninstall(NameArgs),
    /// Checks the registration, configuration, directories, retention, git and the endpoint.
    #[command(after_help = EXIT_CODES)]
    Doctor(NameArgs),
    /// Pauses a repository, or every repository: the current attempt finishes, nothing new
    /// starts.
    #[command(after_help = EXIT_CODES)]
    Pause(control::PauseArgs),
    /// Lifts a pause.
    #[command(after_help = EXIT_CODES)]
    Resume(control::PauseArgs),
    /// The kill switch: kills every child, parks every item, stops every board write.
    #[command(after_help = EXIT_CODES)]
    Kill(control::KillArgs),
    /// Restarts an item in flight: kills its session, takes a new snapshot, starts again.
    #[cfg(not(mutant_cli_action_without_command))]
    #[command(after_help = EXIT_CODES)]
    RestartItem(control::RestartArgs),
    /// Per repository: its state, the item in flight, the last verdict and the hold.
    #[command(after_help = EXIT_CODES)]
    Status,
    /// Each board's columns, with each card's state, pending edit and lint findings.
    #[command(after_help = EXIT_CODES)]
    Queue(work::QueueArgs),
    /// One item: its state, snapshot, attempts, sessions, verdict runs and transitions.
    #[command(after_help = EXIT_CODES)]
    Item(work::ItemArgs),
    /// The static lint of one card, through the daemon; exits 1 when it is not claimable.
    #[command(after_help = EXIT_CODES)]
    Lint(work::LintArgs),
    /// The sessions bosun spawned.
    #[command(subcommand, after_help = EXIT_CODES)]
    Session(work::SessionCommand),
}

#[derive(Debug, Args)]
struct NameArgs {
    /// The registration's name: the task, the label's last part, the unit.
    #[arg(long, value_name = "NAME", default_value = install::DEFAULT_NAME, value_parser = install::parse_name)]
    name: String,
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// The daemon to register [default: bosund next to bosun].
    #[arg(long, value_name = "PATH")]
    daemon: Option<PathBuf>,
    #[command(flatten)]
    name: NameArgs,
}

#[derive(Debug, Subcommand)]
enum IngestCommand {
    /// Transcript files, bytes parsed and the last ingest pass.
    #[command(after_help = EXIT_CODES)]
    Status,
}

#[derive(Debug, Args)]
struct TokensArgs {
    /// Rows per context window (session), repository or model. JSON of `--by session` is
    /// recount's compare shape version 1.
    #[arg(long, value_enum, default_value_t = By::Repo)]
    by: By,
    /// Messages at or after this time: RFC 3339 (2026-11-01T09:30:00Z), a date (2026-11-01,
    /// midnight UTC) or a duration before now (90m, 36h, 7d, 2w).
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    since: Option<Bound>,
    /// Messages before this time; the same forms as --since.
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    until: Option<Bound>,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Messages at or after this time [default: seven days before --until]. The previous
    /// period of the per-commit trend has the same length and ends here. The same forms as
    /// tokens --since.
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    since: Option<Bound>,
    /// Messages before this time [default: now].
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    until: Option<Bound>,
    /// Only this repository's messages and commits (its name in the configuration).
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
}

/// The default length of a report's range.
const WEEK_MS: i64 = 7 * 24 * 3_600_000;

impl ReportArgs {
    /// The range `[since, until)`: `until` defaults to now, `since` to a week before `until`.
    fn range(&self, now_ms: i64) -> (i64, i64) {
        let until = self.until.map_or(now_ms, |b| b.resolve(now_ms));
        let since = self
            .since
            .map_or_else(|| until.saturating_sub(WEEK_MS), |b| b.resolve(now_ms));
        (since, until)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum By {
    Session,
    Repo,
    Model,
}

/// A command that failed: its exit code and what to say.
#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
    /// What to print on stdout all the same: the findings of `lint`.
    stdout: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            stdout: String::new(),
        }
    }

    fn with_stdout(mut self, stdout: String) -> Self {
        self.stdout = stdout;
        self
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        let code = match &e {
            Error::NotRunning { .. } => exit::NOT_RUNNING,
            Error::UnsupportedVersion { .. } => exit::VERSION,
            Error::Rpc(rpc) if rpc.code == code::UNKNOWN_IDENTITY => exit::UNKNOWN_IDENTITY,
            _ => exit::FAILED,
        };
        Self::new(code, e.to_string())
    }
}

/// The identities to ask about: `--identity`, else every identity of the configuration, or
/// with `repo` the one that owns that repository.
fn identities(global: &Global, repo: Option<&str>) -> Result<Vec<String>, Failure> {
    if let Some(name) = &global.identity {
        return Ok(vec![name.clone()]);
    }
    let path = global
        .config
        .clone()
        .or_else(bosun_config::default_path)
        .ok_or_else(|| {
            Failure::new(
                exit::CONFIG,
                "no configuration file: pass --config or set BOSUN_CONFIG",
            )
        })?;
    let config = bosun_config::load(&path, &SystemProbe::new())
        .map_err(|e| Failure::new(exit::CONFIG, format!("{}: {e}", path.display())))?;
    if let Some(repo) = repo {
        let owner = config.repos.get(repo).ok_or_else(|| {
            Failure::new(
                exit::USAGE,
                format!("{}: no repository named {repo:?}", path.display()),
            )
        })?;
        return Ok(vec![owner.identity.clone()]);
    }
    Ok(config.identities.into_keys().collect())
}

async fn connect(global: &Global) -> Result<Client, Failure> {
    let endpoint = match &global.endpoint {
        Some(name) => Endpoint::new(name),
        None => Endpoint::resolve().map_err(|e| Failure::new(exit::FAILED, e))?,
    };
    Ok(Client::connect(&endpoint).await?)
}

/// The rows of one identity's `tokens` section.
#[derive(Debug)]
enum Rows {
    /// `repos` is fetched for JSON only, which needs it for recount's shape.
    Sessions {
        rows: Vec<SessionRow>,
        repos: Vec<RepoRow>,
    },
    Repos(Vec<RepoRow>),
    Models(Vec<ModelRow>),
}

/// One identity's `tokens` section.
#[derive(Debug)]
struct Tokens {
    summary: TokensSummary,
    rows: Rows,
}

impl Tokens {
    fn json(self) -> Result<Value, Failure> {
        let value = match self.rows {
            Rows::Sessions { rows, repos } => shape::v1(&rows, &repos, &self.summary.total),
            Rows::Repos(rows) => serde_json::to_value(bosun_api::TokensByRepo {
                summary: self.summary,
                rows,
            })
            .map_err(|e| Failure::new(exit::FAILED, e.to_string()))?,
            Rows::Models(rows) => serde_json::to_value(bosun_api::TokensByModel {
                summary: self.summary,
                rows,
            })
            .map_err(|e| Failure::new(exit::FAILED, e.to_string()))?,
        };
        Ok(value)
    }

    fn text(&self, out: &mut String) {
        match &self.rows {
            Rows::Sessions { rows, .. } => render::sessions(&self.summary, rows, out),
            Rows::Repos(rows) => render::repos(&self.summary, rows, out),
            Rows::Models(rows) => render::models(&self.summary, rows, out),
        }
    }

    /// Mutant `mutant_cli_sum_identities` (#25): the next identity's rows and sums are added
    /// to this section instead of getting one of their own.
    #[cfg(mutant_cli_sum_identities)]
    fn absorb(&mut self, next: Self) {
        let (a, b) = (&mut self.summary.total, &next.summary.total);
        a.messages += b.messages;
        a.input += b.input;
        a.cache_write_5m += b.cache_write_5m;
        a.cache_write_1h += b.cache_write_1h;
        a.cache_read += b.cache_read;
        a.output += b.output;
        a.cost.0 += b.cost.0;
        a.eqv += b.eqv;
        a.unpriced_messages += b.unpriced_messages;
        self.summary.identity = format!("{}+{}", self.summary.identity, next.summary.identity);
        match (&mut self.rows, next.rows) {
            (
                Rows::Sessions { rows, repos },
                Rows::Sessions {
                    rows: more,
                    repos: more_repos,
                },
            ) => {
                rows.extend(more);
                repos.extend(more_repos);
            }
            (Rows::Repos(rows), Rows::Repos(more)) => rows.extend(more),
            (Rows::Models(rows), Rows::Models(more)) => rows.extend(more),
            _ => {}
        }
    }
}

async fn tokens(
    client: &mut Client,
    identity: &str,
    args: &TokensArgs,
    format: Format,
    now_ms: i64,
) -> Result<Tokens, Failure> {
    let range = TokensParams {
        identity: identity.to_owned(),
        from_ms: args.since.map(|b| b.resolve(now_ms)),
        to_ms: args.until.map(|b| b.resolve(now_ms)),
    };
    Ok(match args.by {
        By::Session => {
            let sessions = client.call::<method::TokensBySession>(&range).await?;
            let repos = if format == Format::Json {
                client.call::<method::TokensByRepo>(&range).await?.rows
            } else {
                Vec::new()
            };
            Tokens {
                summary: sessions.summary,
                rows: Rows::Sessions {
                    rows: sessions.rows,
                    repos,
                },
            }
        }
        By::Repo => {
            let result = client.call::<method::TokensByRepo>(&range).await?;
            Tokens {
                summary: result.summary,
                rows: Rows::Repos(result.rows),
            }
        }
        By::Model => {
            let result = client.call::<method::TokensByModel>(&range).await?;
            Tokens {
                summary: result.summary,
                rows: Rows::Models(result.rows),
            }
        }
    })
}

/// One section of output: the identity it belongs to, as text and as JSON.
#[derive(Debug)]
struct Section {
    identity: String,
    text: String,
    json: Value,
}

impl Section {
    fn of<T: serde::Serialize>(
        identity: &str,
        value: &T,
        text: impl FnOnce(&T, &mut String),
    ) -> Result<Self, Failure> {
        let mut out = String::new();
        text(value, &mut out);
        Ok(Self {
            identity: identity.to_owned(),
            text: out,
            json: serde_json::to_value(value)
                .map_err(|e| Failure::new(exit::FAILED, e.to_string()))?,
        })
    }
}

/// The usage errors of `report` and of `--format md`, which only `report` takes.
fn check_report_args(cli: &Cli, now_ms: i64) -> Result<(), Failure> {
    if let Command::Report(args) = &cli.command {
        let (since, until) = args.range(now_ms);
        if since >= until {
            return Err(Failure::new(
                exit::USAGE,
                "--since must be before --until: the range is empty",
            ));
        }
    } else if cli.global.format == Format::Md {
        return Err(Failure::new(
            exit::USAGE,
            "--format md applies to report only; use text or json",
        ));
    }
    Ok(())
}

/// One identity's `report` section.
async fn report(
    client: &mut Client,
    identity: &str,
    args: &ReportArgs,
    now_ms: i64,
) -> Result<Section, Failure> {
    let (from_ms, to_ms) = args.range(now_ms);
    let ask = ReportParams {
        identity: identity.to_owned(),
        from_ms,
        to_ms,
        repo: args.repo.clone(),
    };
    let exact = client.call::<method::ReportExact>(&ask).await?;
    let estimates = client.call::<method::ReportEstimates>(&ask).await?;
    let document = markdown::Document { exact, estimates };
    Section::of(
        &document.exact.identity.clone(),
        &document,
        markdown::document,
    )
}

/// Runs the commands that resolve their identities themselves; `None` for the others.
async fn own_terms(global: &Global, command: &Command) -> Option<Result<String, Failure>> {
    Some(match command {
        Command::Install(args) => admin::install(global, args).await,
        Command::Uninstall(args) => admin::uninstall(global, args).await,
        Command::Doctor(args) => admin::doctor(global, args).await,
        Command::Pause(args) => control::pause(global, args, true).await,
        Command::Resume(args) => control::pause(global, args, false).await,
        Command::Kill(args) => control::kill(global, args).await,
        #[cfg(not(mutant_cli_action_without_command))]
        Command::RestartItem(args) => control::restart_item(global, args).await,
        Command::Status => work::status(global).await,
        Command::Queue(args) => work::queue(global, args).await,
        Command::Item(args) => work::item(global, args).await,
        Command::Lint(args) => work::lint(global, args).await,
        Command::Session(work::SessionCommand::Tail(args)) => work::tail(global, args).await,
        _ => return None,
    })
}

/// Runs the command; the text to print on stdout.
async fn run(cli: Cli, now_ms: i64) -> Result<String, Failure> {
    let global = &cli.global;
    if let Command::Tokens(TokensArgs {
        since: Some(since),
        until: Some(until),
        ..
    }) = &cli.command
        && since.resolve(now_ms) >= until.resolve(now_ms)
    {
        return Err(Failure::new(
            exit::USAGE,
            "--since must be before --until: the range is empty",
        ));
    }
    // Refuses `--format md` outside `report` before any command runs.
    check_report_args(&cli, now_ms)?;
    if let Some(done) = own_terms(global, &cli.command).await {
        return done;
    }
    let repo = match &cli.command {
        Command::Report(args) => args.repo.as_deref(),
        _ => None,
    };
    let names = identities(global, repo)?;
    if names.is_empty() {
        eprintln!("bosun: the configuration names no identity");
        return Ok(match global.format {
            Format::Text | Format::Md => String::new(),
            Format::Json => "{}\n".to_owned(),
        });
    }
    let mut client = connect(global).await?;
    let mut sections = Vec::new();
    match &cli.command {
        Command::Ingest(IngestCommand::Status) => {
            for name in &names {
                let ask = IdentityParams {
                    identity: name.clone(),
                };
                let status = client.call::<method::IngestStatus>(&ask).await?;
                sections.push(Section::of::<IngestStatus>(
                    &status.identity,
                    &status,
                    render::ingest_status,
                )?);
            }
        }
        Command::Reingest => {
            for name in &names {
                let ask = IdentityParams {
                    identity: name.clone(),
                };
                let result = client.call::<method::IngestReingest>(&ask).await?;
                sections.push(Section::of::<ReingestResult>(
                    &result.identity,
                    &result,
                    render::reingest,
                )?);
            }
        }
        Command::Report(args) => {
            for name in &names {
                sections.push(report(&mut client, name, args, now_ms).await?);
            }
        }
        Command::Tokens(args) => {
            let mut all = Vec::new();
            for name in &names {
                all.push(tokens(&mut client, name, args, global.format, now_ms).await?);
            }
            #[cfg(mutant_cli_sum_identities)]
            let all: Vec<Tokens> = all
                .into_iter()
                .reduce(|mut first, next| {
                    first.absorb(next);
                    first
                })
                .into_iter()
                .collect();
            for section in all {
                let mut text = String::new();
                section.text(&mut text);
                sections.push(Section {
                    identity: section.summary.identity.clone(),
                    text,
                    json: section.json()?,
                });
            }
        }
        // Answered above, before the configuration is required.
        _ => {}
    }
    Ok(output(global, sections))
}

/// Joins the sections: text one after the other, JSON as one document with `--identity` and
/// as an object keyed by identity without it.
fn output(global: &Global, sections: Vec<Section>) -> String {
    match global.format {
        Format::Text | Format::Md => sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Format::Json => {
            let value = if global.identity.is_some() && sections.len() == 1 {
                sections
                    .into_iter()
                    .map(|s| s.json)
                    .next()
                    .unwrap_or_default()
            } else {
                Value::Object(sections.into_iter().map(|s| (s.identity, s.json)).collect())
            };
            let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
            text.push('\n');
            text
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version go to stdout with 0; a usage error to stderr with 2.
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(exit::USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("bosun: cannot start the runtime: {e}");
            return ExitCode::from(exit::FAILED);
        }
    };
    match runtime.block_on(run(cli, now_ms())) {
        Ok(text) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(failure.stdout.as_bytes());
            let _ = stdout.flush();
            drop(stdout);
            eprintln!("bosun: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("bosun").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn arguments_are_parsed() {
        let cli = parse(&[
            "tokens",
            "--by",
            "session",
            "--since",
            "7d",
            "--until",
            "2026-11-01",
            "--format",
            "json",
            "--identity",
            "work",
        ])
        .unwrap();
        let Command::Tokens(args) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.by, By::Session);
        assert_eq!(args.since, Some(Bound::Ago(7 * 24 * 3_600_000)));
        assert_eq!(args.until, Some(Bound::At(1_793_491_200_000)));
        assert_eq!(cli.global.format, Format::Json);
        assert_eq!(cli.global.identity.as_deref(), Some("work"));

        let cli = parse(&["--format", "json", "ingest", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Ingest(IngestCommand::Status)
        ));
        let cli = parse(&["tokens"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Tokens(TokensArgs { by: By::Repo, .. })
        ));

        for bad in [
            &["tokens", "--by", "day"][..],
            &["tokens", "--since", "yesterday"],
            &["reingest", "--format", "yaml"],
            &["ingest"],
            &[],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    /// Every operator action of the table the TUI shares has its CLI command (#70); with
    /// `mutant_cli_action_without_command`, restart-item has none.
    #[test]
    fn every_action_has_a_command() {
        let cli = Cli::command();
        for spec in bosun_client::actions::ACTIONS {
            assert!(
                cli.find_subcommand(spec.command).is_some(),
                "the action {:?} has no command `bosun {}`",
                spec.action,
                spec.command
            );
        }
    }

    #[test]
    fn the_loop_commands_are_parsed() {
        let cli = parse(&["session", "tail", "demo#3", "--format", "json"]).unwrap();
        assert!(matches!(cli.command, Command::Session(_)));
        assert!(matches!(
            parse(&["lint", "4", "--repo", "demo"]).unwrap().command,
            Command::Lint(_)
        ));
        assert!(matches!(
            parse(&["status"]).unwrap().command,
            Command::Status
        ));
        for bad in [&["item"][..], &["session"], &["lint"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn every_exit_code_is_in_the_help() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains(EXIT_CODES), "{help}");
        for code in [
            0,
            exit::FAILED,
            exit::USAGE,
            exit::CONFIG,
            exit::NOT_RUNNING,
            exit::UNKNOWN_IDENTITY,
            exit::VERSION,
            exit::CHECK_FAILED,
        ] {
            assert!(EXIT_CODES.contains(&format!("\n  {code}  ")), "{code}");
        }
    }
}
