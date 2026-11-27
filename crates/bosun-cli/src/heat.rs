//! `bosun heat` (#11, part D): heat's proposals of each identity (`heat.list`), active ones by
//! default and every state with `--all`; `bosun heat dismiss <key>` dismisses one of them
//! (`heat.dismiss`, with the operator token); `bosun heat metrics` the generic metrics the heat
//! detectors test, as the daemon serves them (`heat.metrics`). It prints numbers and draws no
//! conclusions.

use bosun_api::{HeatDismissParams, HeatListParams, ReportParams, method};
use bosun_client::Client;
use clap::{Args, Subcommand};

use crate::control::connect_operator;
use crate::time::{self, Bound};
use crate::{EXIT_CODES, Failure, Format, Global, Section, exit, markdown};

/// `bosun heat`: without a subcommand, the proposals.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct HeatArgs {
    #[command(subcommand)]
    pub(crate) command: Option<HeatCommand>,
    /// Only this repository's proposals (its name in the configuration); only the identity
    /// that owns it is asked.
    #[arg(long, value_name = "NAME")]
    pub(crate) repo: Option<String>,
    /// Every state, not only the active proposals (open, published, landed).
    #[arg(long)]
    pub(crate) all: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum HeatCommand {
    /// The generic metrics per repository, provider and session kind: each says whether it is
    /// exact or an estimate and whether it compares across runners.
    #[command(after_help = EXIT_CODES)]
    Metrics(MetricsArgs),
    /// Dismisses an open or published proposal of one identity (--identity) by its key: its
    /// subject is not proposed again for 90 days. Sends the operator token.
    #[command(after_help = EXIT_CODES)]
    Dismiss(DismissArgs),
}

impl HeatArgs {
    /// The repository the command is limited to, which picks the identity that owns it.
    pub(crate) fn repo(&self) -> Option<&str> {
        match &self.command {
            Some(HeatCommand::Metrics(args)) => args.repo.as_deref(),
            Some(HeatCommand::Dismiss(_)) => None,
            None => self.repo.as_deref(),
        }
    }

    /// The range `[since, until)` of `heat metrics`; `None` for the others, which take none.
    pub(crate) fn range(&self, now_ms: i64) -> Option<(i64, i64)> {
        match &self.command {
            Some(HeatCommand::Metrics(args)) => Some(args.range(now_ms)),
            _ => None,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct MetricsArgs {
    /// Messages at or after this time [default: 14 days before --until]. The same forms as
    /// tokens --since.
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    pub(crate) since: Option<Bound>,
    /// Messages before this time [default: now].
    #[arg(long, value_name = "TIME", value_parser = time::parse_bound)]
    pub(crate) until: Option<Bound>,
    /// Only this repository (its name in the configuration).
    #[arg(long, value_name = "NAME")]
    pub(crate) repo: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DismissArgs {
    /// The proposal's key, the first column of `bosun heat`.
    #[arg(value_name = "KEY")]
    pub(crate) key: String,
    /// Why it is dismissed.
    #[arg(long, value_name = "TEXT")]
    pub(crate) reason: Option<String>,
}

/// The default length of the range of `heat metrics`.
const FORTNIGHT_MS: i64 = 14 * 24 * 3_600_000;

impl MetricsArgs {
    /// The range `[since, until)`: `until` defaults to now, `since` to 14 days before `until`.
    fn range(&self, now_ms: i64) -> (i64, i64) {
        let until = self.until.map_or(now_ms, |b| b.resolve(now_ms));
        let since = self
            .since
            .map_or_else(|| until.saturating_sub(FORTNIGHT_MS), |b| b.resolve(now_ms));
        (since, until)
    }
}

/// One identity's section of `heat` or `heat metrics`.
pub(crate) async fn section(
    client: &mut Client,
    identity: &str,
    args: &HeatArgs,
    now_ms: i64,
) -> Result<Section, Failure> {
    match &args.command {
        Some(HeatCommand::Metrics(metrics)) => {
            let (from_ms, to_ms) = metrics.range(now_ms);
            let ask = ReportParams {
                identity: identity.to_owned(),
                from_ms,
                to_ms,
                repo: metrics.repo.clone(),
            };
            let metrics = client.call::<method::HeatMetrics>(&ask).await?;
            Section::of(
                &metrics.identity.clone(),
                &metrics,
                markdown::heat::heat_metrics,
            )
        }
        None => {
            let ask = HeatListParams {
                identity: identity.to_owned(),
                repo: args.repo.clone(),
                all: args.all,
            };
            let proposals = client.call::<method::HeatList>(&ask).await?;
            Section::of(
                &proposals.identity.clone(),
                &proposals,
                markdown::heat::heat_proposals,
            )
        }
        // Answered by `dismiss`, before any identity is resolved.
        Some(HeatCommand::Dismiss(_)) => Err(Failure::new(
            exit::FAILED,
            "heat dismiss names one identity",
        )),
    }
}

/// `heat dismiss`: one identity, named by `--identity`, and the operator token.
pub(crate) async fn dismiss(global: &Global, args: &DismissArgs) -> Result<String, Failure> {
    let identity = global.identity.clone().ok_or_else(|| {
        Failure::new(
            exit::USAGE,
            "heat dismiss changes one identity's proposal: pass --identity",
        )
    })?;
    let (mut client, operator) = connect_operator(global).await?;
    let dismissed = client
        .call::<method::HeatDismiss>(&HeatDismissParams {
            operator,
            identity: identity.clone(),
            key: args.key.clone(),
            reason: args.reason.clone(),
        })
        .await?;
    Ok(match global.format {
        Format::Json => {
            let mut text = serde_json::to_string_pretty(&dismissed).unwrap_or_default();
            text.push('\n');
            text
        }
        _ => format!(
            "{identity}: proposal {} ({} on {}) dismissed\n",
            dismissed.key, dismissed.detector, dismissed.subject
        ),
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::HeatCommand;
    use crate::{Cli, Command};

    #[test]
    fn the_range_of_heat_metrics_is_fourteen_days_before_now_by_default() {
        let cli = Cli::try_parse_from(["bosun", "heat", "metrics", "--repo", "alpha"]).unwrap();
        let Command::Heat(command) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(command.repo(), Some("alpha"));
        let now = 2_000_000_000_000;
        assert_eq!(command.range(now), Some((now - 14 * 86_400_000, now)));
    }

    #[test]
    fn heat_without_a_subcommand_lists_and_dismiss_takes_a_key() {
        let cli = Cli::try_parse_from(["bosun", "heat", "--repo", "beta", "--all"]).unwrap();
        let Command::Heat(args) = &cli.command else {
            panic!("{cli:?}");
        };
        assert!(args.command.is_none() && args.all, "{args:?}");
        assert_eq!(args.repo(), Some("beta"));
        assert_eq!(args.range(0), None);

        let cli = Cli::try_parse_from([
            "bosun",
            "heat",
            "dismiss",
            "0123456789abcdef",
            "--identity",
            "work",
            "--reason",
            "not worth it",
        ])
        .unwrap();
        let Command::Heat(args) = &cli.command else {
            panic!("{cli:?}");
        };
        let Some(HeatCommand::Dismiss(dismiss)) = &args.command else {
            panic!("{args:?}");
        };
        assert_eq!(dismiss.key, "0123456789abcdef");
        assert_eq!(dismiss.reason.as_deref(), Some("not worth it"));
        assert_eq!(cli.global.identity.as_deref(), Some("work"));
    }
}
