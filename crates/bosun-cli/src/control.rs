//! The operator actions (#4, #67, #70): `bosun pause`, `resume`, `kill` and `restart-item`,
//! performed from the table the TUI shares (`bosun_client::actions`).
//!
//! Each connects first (so a daemon that is down is exit code 4), then reads the daemon's
//! operator token (`bosun_client::operator`, written by the daemon at start, readable by this
//! OS user only) and sends it with the request; without it the daemon refuses. `pause` and
//! `resume` act on one repository with `--repo` (and ask the identity that owns it), else on
//! every repository of each identity; each identity's control state is a section of the
//! output, as `control.pause` returns it. `kill` throws the daemon-wide kill switch, `kill
//! --release` releases it. `restart-item <item>` puts the `restart` label on the card of an
//! item in flight (`item.restart`): the loop kills its session, takes a new snapshot and starts
//! again.

use std::fmt::Write as _;

use bosun_api::ControlStatus;
use bosun_client::actions::{self, Action, Outcome, Target};
use bosun_client::{Client, Endpoint};
use clap::Args;

use crate::work::split_item;
use crate::{Failure, Format, Global, Section, connect, exit, identities, output};

#[derive(Debug, Args)]
pub(crate) struct PauseArgs {
    /// Only this repository (its name in the configuration); every repository otherwise.
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct KillArgs {
    /// Release the kill switch: parked items resume.
    #[arg(long)]
    release: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RestartArgs {
    /// The item in flight: `<repo>#<card>`.
    #[arg(value_name = "ITEM")]
    item: String,
}

pub(crate) fn endpoint(global: &Global) -> Result<Endpoint, Failure> {
    match &global.endpoint {
        Some(name) => Ok(Endpoint::new(name)),
        None => Endpoint::resolve().map_err(|e| Failure::new(exit::FAILED, e)),
    }
}

/// Connects, then reads the operator token: a daemon that is down is exit code 4, whatever
/// token file an earlier run left.
async fn connect_operator(global: &Global) -> Result<(Client, String), Failure> {
    let client = connect(global).await?;
    let token = bosun_client::operator::read(&endpoint(global)?)
        .map_err(|e| Failure::new(exit::FAILED, e))?;
    Ok((client, token))
}

/// `pause` (`resume` with `pause` false).
pub(crate) async fn pause(
    global: &Global,
    args: &PauseArgs,
    pause: bool,
) -> Result<String, Failure> {
    let names = identities(global, args.repo.as_deref())?;
    let (mut client, operator) = connect_operator(global).await?;
    let action = if pause { Action::Pause } else { Action::Resume };
    let mut sections = Vec::new();
    for name in &names {
        let target = Target {
            identity: name.clone(),
            repo: args.repo.clone(),
            ..Target::default()
        };
        let Outcome::Control(status) =
            actions::perform(&mut client, action, &operator, &target).await?
        else {
            return Err(Failure::new(exit::FAILED, "an unexpected answer"));
        };
        sections.push(Section::of(&status.identity.clone(), &status, render)?);
    }
    Ok(output(global, sections))
}

/// `kill [--release]`.
pub(crate) async fn kill(global: &Global, args: &KillArgs) -> Result<String, Failure> {
    let (mut client, operator) = connect_operator(global).await?;
    let action = if args.release {
        Action::ReleaseKill
    } else {
        Action::Kill
    };
    let Outcome::Kill(result) =
        actions::perform(&mut client, action, &operator, &Target::default()).await?
    else {
        return Err(Failure::new(exit::FAILED, "an unexpected answer"));
    };
    Ok(match global.format {
        Format::Json => {
            let mut text = serde_json::to_string_pretty(&result).unwrap_or_default();
            text.push('\n');
            text
        }
        _ if result.killed => "the kill switch is thrown: every child is killed, every item \
                               parks, nothing is written to any board\n"
            .to_owned(),
        _ => "the kill switch is released: parked items resume\n".to_owned(),
    })
}

/// `restart-item <item>`: one document, of the identity that owns the item.
pub(crate) async fn restart_item(global: &Global, args: &RestartArgs) -> Result<String, Failure> {
    let (repo, _) = split_item(&args.item).ok_or_else(|| {
        Failure::new(
            exit::USAGE,
            format!("{:?} is not an item: an item is `<repo>#<card>`", args.item),
        )
    })?;
    let identity = identities(global, Some(repo))?
        .into_iter()
        .next()
        .ok_or_else(|| Failure::new(exit::CONFIG, "the configuration names no identity"))?;
    let (mut client, operator) = connect_operator(global).await?;
    let target = Target {
        identity,
        item: Some(args.item.clone()),
        ..Target::default()
    };
    let Outcome::Restart(result) =
        actions::perform(&mut client, Action::RestartItem, &operator, &target).await?
    else {
        return Err(Failure::new(exit::FAILED, "an unexpected answer"));
    };
    Ok(match global.format {
        Format::Json => {
            let mut text = serde_json::to_string_pretty(&result).unwrap_or_default();
            text.push('\n');
            text
        }
        _ => format!(
            "{}: the `{}` label is on its card; the loop restarts it\n",
            result.item, result.label
        ),
    })
}

/// One identity's control state as text.
pub(crate) fn render(status: &ControlStatus, out: &mut String) {
    out.push_str(&crate::render::header(&status.identity));
    if status.killed {
        let _ = writeln!(out, "the kill switch is thrown");
    }
    if status.paused {
        let _ = writeln!(out, "every repository is paused by hand");
    }
    if let Some(until) = status.daily_cap_until_ms {
        let _ = writeln!(out, "the daily cap holds the queue until {until} (ms, UTC)");
    }
    for repo in &status.repos {
        let hold = repo.hold.as_deref().unwrap_or("none");
        let mut why = Vec::new();
        if repo.paused {
            why.push("paused by hand".to_owned());
        }
        if let Some(until) = repo.auto_paused_until_ms {
            why.push(format!(
                "an interactive session works there (until {until})"
            ));
        }
        let why = if why.is_empty() {
            String::new()
        } else {
            format!(": {}", why.join(", "))
        };
        let _ = writeln!(out, "repo {}: hold {hold}{why}", repo.repo);
    }
}
