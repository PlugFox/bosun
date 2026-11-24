//! `bosun pause`, `bosun resume` and `bosun kill` (#4, #67): the control switches.
//!
//! Each reads the daemon's operator token (`bosun_client::operator`, written by the daemon at
//! start, readable by this OS user only) and sends it with the request; without it the daemon
//! refuses. `pause` and `resume` act on one repository with `--repo` (and ask the identity that
//! owns it), else on every repository of each identity; each identity's control state is a
//! section of the output, as `control.pause` returns it. `kill` throws the daemon-wide kill
//! switch, `kill --release` releases it.

use std::fmt::Write as _;

use bosun_api::{ControlStatus, KillParams, KillResult, PauseParams, method};
use bosun_client::Endpoint;
use clap::Args;

use crate::{Failure, Global, Section, connect, exit, identities, output};

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

fn endpoint(global: &Global) -> Result<Endpoint, Failure> {
    match &global.endpoint {
        Some(name) => Ok(Endpoint::new(name)),
        None => Endpoint::resolve().map_err(|e| Failure::new(exit::FAILED, e)),
    }
}

fn token(global: &Global) -> Result<String, Failure> {
    bosun_client::operator::read(&endpoint(global)?).map_err(|e| Failure::new(exit::FAILED, e))
}

/// `pause` (`resume` with `pause` false).
pub(crate) async fn pause(
    global: &Global,
    args: &PauseArgs,
    pause: bool,
) -> Result<String, Failure> {
    let names = identities(global, args.repo.as_deref())?;
    let operator = token(global)?;
    let mut client = connect(global).await?;
    let mut sections = Vec::new();
    for name in &names {
        let ask = PauseParams {
            operator: operator.clone(),
            identity: name.clone(),
            repo: args.repo.clone(),
        };
        let status = if pause {
            client.call::<method::ControlPause>(&ask).await?
        } else {
            client.call::<method::ControlResume>(&ask).await?
        };
        sections.push(Section::of(&status.identity.clone(), &status, render)?);
    }
    Ok(output(global, sections))
}

/// `kill [--release]`.
pub(crate) async fn kill(global: &Global, args: &KillArgs) -> Result<String, Failure> {
    let operator = token(global)?;
    let mut client = connect(global).await?;
    let ask = KillParams {
        operator,
        release: args.release,
    };
    let result: KillResult = client.call::<method::ControlKill>(&ask).await?;
    Ok(match global.format {
        crate::Format::Json => {
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
