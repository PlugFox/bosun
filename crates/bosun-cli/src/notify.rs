//! `bosun notify` (#9, the decision on channels): an identity's notification channels, through
//! the `notify.channel.*` methods.
//!
//! ```text
//! bosun notify add <name> --kind <kind> [--param key=value]... [--events <e>,<e>...]
//!                         [--min-gap <seconds>] --identity <name>   < secret
//! bosun notify list [--identity <name>]
//! bosun notify remove <name> --identity <name>
//! bosun notify test <name> --identity <name>
//! ```
//!
//! - `add` adds a channel to the identity: its kind, the kind's non-secret parameters, the
//!   events it gets (default all of `needs_you`, `repo_paused`, `ready_drained`,
//!   `resource_unhealthy`) and the least time between two of its messages (default 60 s). The
//!   daemon keeps the secret in the OS credential store and the rest in the identity's
//!   database.
//! - `list` shows each live channel, its kind, parameters, events, gap, credential-store entry
//!   and whether the entry is present; never a secret. Without `--identity`, every identity of
//!   the configuration, each in a section of its own.
//! - `remove` deletes the channel's credential-store entry and removes it.
//! - `test` sends a test message on the channel now and says whether it was accepted.
//!
//! `add`, `remove` and `test` change or send something: they need `--identity` (exit 2
//! without), and they send the daemon's operator token (`control::connect_operator`).
//!
//! # The secret
//!
//! `add` reads the secret from standard input, one line, its line end trimmed; there is no
//! argument or option for it, so it never shows in a process list or a shell's history. When
//! standard input is a terminal, `add` refuses (exit 2) and says to pipe it
//! (`Get-Clipboard | bosun notify add ...`, `pbpaste | bosun notify add ...`): the CLI cannot
//! turn the terminal's echo off without `unsafe` (#12), and an echoed token stays on the screen
//! and in the scrollback. A hidden prompt (echo off, through a small audited crate or
//! `bosun-proc`) is a follow-up.
//!
//! # JSON
//!
//! With `--format json` each command prints its method's result (`notify.channel.add`,
//! `.list`, `.remove`, `.test`); `list` without `--identity` prints an object keyed by identity.
//! A test message that was not accepted exits 1, after the result is printed.

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal};

use bosun_api::{ChannelAddParams, ChannelParams, ChannelView, Channels, IdentityParams, method};
use clap::{Args, Subcommand};

use crate::control::connect_operator;
use crate::{Failure, Format, Global, Section, connect, exit, identities, output};

/// The events a channel gets when `--events` is not given.
const ALL_EVENTS: &[&str] = bosun_api::notify::EVENTS;

#[derive(Debug, Subcommand)]
pub(crate) enum NotifyCommand {
    /// Adds a channel; its secret is read from standard input (pipe it), never an argument.
    #[command(after_help = crate::EXIT_CODES)]
    Add(AddArgs),
    /// The identity's channels and whether each one's secret is present.
    #[command(after_help = crate::EXIT_CODES)]
    List,
    /// Removes a channel and deletes its secret from the credential store.
    #[command(after_help = crate::EXIT_CODES)]
    Remove(NameArgs),
    /// Sends a test message on a channel now.
    #[command(after_help = crate::EXIT_CODES)]
    Test(NameArgs),
}

#[derive(Debug, Args)]
pub(crate) struct AddArgs {
    /// The channel's name within the identity: 1 to 32 of a-z, 0-9 and -.
    #[arg(value_name = "NAME")]
    name: String,
    /// The channel's kind.
    #[arg(long, value_name = "KIND")]
    kind: String,
    /// A non-secret parameter of the kind (a chat id, a URL); repeat for more.
    #[arg(long = "param", value_name = "KEY=VALUE", value_parser = parse_param)]
    params: Vec<(String, String)>,
    /// The events the channel gets, comma-separated.
    #[arg(long, value_name = "EVENTS", value_delimiter = ',', default_values_t = ALL_EVENTS.iter().map(|e| (*e).to_owned()))]
    events: Vec<String>,
    /// The least time between two of its messages, in seconds.
    #[arg(long = "min-gap", value_name = "SECONDS", default_value_t = 60)]
    min_gap: u64,
    /// Mutant `mutant_cli_notify_secret_argument` (#9): the secret as an option, where every
    /// process list and shell history shows it.
    #[cfg(mutant_cli_notify_secret_argument)]
    #[arg(long, value_name = "SECRET")]
    secret: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct NameArgs {
    /// The channel's name.
    #[arg(value_name = "NAME")]
    name: String,
}

fn parse_param(text: &str) -> Result<(String, String), String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not KEY=VALUE"))?;
    if key.is_empty() {
        return Err(format!("{text:?} has no key"));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// The secret: one line of `input`, its line end trimmed. An empty line, or none, is refused.
pub(crate) fn read_secret(mut input: impl BufRead) -> Result<String, Failure> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| Failure::new(exit::USAGE, format!("reading the secret: {e}")))?;
    let secret = line.strip_suffix('\n').unwrap_or(&line);
    let secret = secret.strip_suffix('\r').unwrap_or(secret);
    if secret.is_empty() {
        return Err(Failure::new(
            exit::USAGE,
            "no secret on standard input: pipe it, one line",
        ));
    }
    Ok(secret.to_owned())
}

/// The secret given as an argument: never, but under the mutant.
#[allow(clippy::unnecessary_wraps, reason = "the mutant gives one")]
fn argument(args: &AddArgs) -> Option<String> {
    #[cfg(mutant_cli_notify_secret_argument)]
    return args.secret.clone();
    #[cfg(not(mutant_cli_notify_secret_argument))]
    {
        let _ = args;
        None
    }
}

/// The identity a write names: `--identity`, required.
fn identity(global: &Global, command: &str) -> Result<String, Failure> {
    global.identity.clone().ok_or_else(|| {
        Failure::new(
            exit::USAGE,
            format!("notify {command} changes one identity's channels: pass --identity"),
        )
    })
}

fn json(value: &impl serde::Serialize) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

/// Runs `notify <command>`.
pub(crate) async fn run(global: &Global, command: &NotifyCommand) -> Result<String, Failure> {
    match command {
        NotifyCommand::Add(args) => add(global, args).await,
        NotifyCommand::List => list(global).await,
        NotifyCommand::Remove(args) => {
            let identity = identity(global, "remove")?;
            let (mut client, operator) = connect_operator(global).await?;
            let removed = client
                .call::<method::NotifyChannelRemove>(&ChannelParams {
                    operator,
                    identity,
                    name: args.name.clone(),
                })
                .await?;
            Ok(match global.format {
                Format::Json => json(&removed),
                _ => format!(
                    "{}: channel {} removed, and its secret deleted\n",
                    removed.identity, removed.name
                ),
            })
        }
        NotifyCommand::Test(args) => {
            let identity = identity(global, "test")?;
            let (mut client, operator) = connect_operator(global).await?;
            let tested = client
                .call::<method::NotifyChannelTest>(&ChannelParams {
                    operator,
                    identity,
                    name: args.name.clone(),
                })
                .await?;
            let text = match global.format {
                Format::Json => json(&tested),
                _ if tested.accepted => format!(
                    "{}: channel {} accepted the test message\n",
                    tested.identity, tested.name
                ),
                _ => String::new(),
            };
            if tested.accepted {
                Ok(text)
            } else {
                Err(Failure::new(
                    exit::FAILED,
                    format!(
                        "{}: channel {} did not accept the test message: {}",
                        tested.identity,
                        tested.name,
                        tested.error.as_deref().unwrap_or("no reason given")
                    ),
                )
                .with_stdout(text))
            }
        }
    }
}

async fn add(global: &Global, args: &AddArgs) -> Result<String, Failure> {
    let identity = identity(global, "add")?;
    let secret = if let Some(secret) = argument(args) {
        secret
    } else {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            return Err(Failure::new(
                exit::USAGE,
                "notify add reads the secret from standard input and will not echo it on a \
                 terminal: pipe it (`Get-Clipboard | bosun notify add ...`, \
                 `pbpaste | bosun notify add ...`)",
            ));
        }
        read_secret(stdin.lock())?
    };
    let (mut client, operator) = connect_operator(global).await?;
    let added = client
        .call::<method::NotifyChannelAdd>(&ChannelAddParams {
            operator,
            identity,
            name: args.name.clone(),
            kind: args.kind.clone(),
            params: args.params.iter().cloned().collect(),
            events: args.events.clone(),
            min_gap_s: args.min_gap,
            secret: Some(secret),
        })
        .await?;
    if global.format == Format::Json {
        return Ok(json(&added));
    }
    let mut out = format!("{}: channel added\n", added.identity);
    channel(&added.channel, &mut out);
    Ok(out)
}

async fn list(global: &Global) -> Result<String, Failure> {
    let names = identities(global, None)?;
    let mut client = connect(global).await?;
    let mut sections = Vec::new();
    for name in names {
        let channels = client
            .call::<method::NotifyChannelList>(&IdentityParams { identity: name })
            .await?;
        sections.push(Section::of(&channels.identity.clone(), &channels, render)?);
    }
    Ok(output(global, sections))
}

/// One identity's channels as text.
fn render(channels: &Channels, out: &mut String) {
    out.push_str(&crate::render::header(&channels.identity));
    if channels.channels.is_empty() {
        out.push_str("no channels\n");
    }
    for view in &channels.channels {
        channel(view, out);
    }
}

fn channel(view: &ChannelView, out: &mut String) {
    let params = view
        .params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(
        out,
        "{} ({}): events {}; min gap {} s; secret {} ({}){}{}",
        view.name,
        view.kind,
        view.events.join(","),
        view.min_gap_s,
        if view.secret_present {
            "present"
        } else {
            "MISSING"
        },
        view.secret_entry,
        if params.is_empty() { "" } else { "; " },
        params,
    );
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{Cli, Command};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("bosun").chain(args.iter().copied()))
    }

    /// #9: the secret has no argument or option (`mutant_cli_notify_secret_argument` adds
    /// `--secret`); the reader takes one line from standard input, its line end trimmed, and
    /// refuses an empty one.
    #[test]
    fn the_secret_is_read_from_stdin_never_from_the_arguments() {
        assert!(parse(&["notify", "add", "ops", "--kind", "fake", "--secret", "x"]).is_err());
        assert!(parse(&["notify", "add", "ops", "x", "--kind", "fake"]).is_err());
        assert!(parse(&["notify", "add", "ops", "--kind", "fake", "x"]).is_err());
        let cli = parse(&[
            "notify",
            "add",
            "ops",
            "--kind",
            "fake",
            "--param",
            "sink=s",
            "--events",
            "needs_you,repo_paused",
        ])
        .unwrap();
        let Command::Notify(NotifyCommand::Add(args)) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.params, [("sink".to_owned(), "s".to_owned())]);
        assert_eq!(args.events, ["needs_you", "repo_paused"]);
        assert_eq!(args.min_gap, 60);
        let all = parse(&["notify", "add", "ops", "--kind", "fake"]).unwrap();
        let Command::Notify(NotifyCommand::Add(all)) = &all.command else {
            panic!("{all:?}");
        };
        assert_eq!(all.events, ALL_EVENTS);

        assert_eq!(read_secret(&b"tok-123\r\n"[..]).unwrap(), "tok-123");
        assert_eq!(read_secret(&b"tok-123\nsecond"[..]).unwrap(), "tok-123");
        assert_eq!(read_secret(&b"tok-123"[..]).unwrap(), "tok-123");
        assert_eq!(read_secret(&b"\r\n"[..]).unwrap_err().code, exit::USAGE);
        assert_eq!(read_secret(&b""[..]).unwrap_err().code, exit::USAGE);
    }
}
