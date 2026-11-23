//! `bosun`, the command line of bosun (#4, #25): a client of the daemon's local API.
//!
//! ```text
//! bosun ingest status [--identity <name>]
//! bosun tokens [--by session|repo|model] [--since <t>] [--until <t>] [--identity <name>]
//! bosun reingest [--identity <name>]
//! ```
//!
//! Every command also takes `--format text|json` (default `text`), `--config <file>` (default
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
//!
//! The results are those of `schema/bosun-api.schema.json`. Without `--identity`, the output
//! is an object keyed by identity name whose values are those documents:
//! `{"personal": {...}, "work": {...}}`.
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
//!
//! Errors go to stderr, prefixed `bosun: `; nothing is printed on stdout then.

mod render;
mod shape;
mod time;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use bosun_api::rpc::code;
use bosun_api::{
    IdentityParams, IngestStatus, ModelRow, ReingestResult, RepoRow, SessionRow, TokensParams,
    TokensSummary, method,
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
  6  the daemon speaks an API version this bosun does not know";

/// The command line of bosun: token sums, ingest status and reingest, asked of the daemon.
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
    /// identity name.
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
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
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

/// The identities to ask about: `--identity`, else every identity of the configuration.
fn identities(global: &Global) -> Result<Vec<String>, Failure> {
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
    let names = identities(global)?;
    if names.is_empty() {
        eprintln!("bosun: the configuration names no identity");
        return Ok(match global.format {
            Format::Text => String::new(),
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
    }
    Ok(output(global, sections))
}

/// Joins the sections: text one after the other, JSON as one document with `--identity` and
/// as an object keyed by identity without it.
fn output(global: &Global, sections: Vec<Section>) -> String {
    match global.format {
        Format::Text => sections
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
        ] {
            assert!(EXIT_CODES.contains(&format!("\n  {code}  ")), "{code}");
        }
    }
}
