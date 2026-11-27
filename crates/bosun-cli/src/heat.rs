//! `bosun heat` (#11, part D): the generic metrics the heat detectors test, as the daemon
//! serves them (`heat.metrics`), per identity. It prints numbers and draws no conclusions.

use bosun_api::{ReportParams, method};
use bosun_client::Client;
use clap::{Args, Subcommand};

use crate::time::{self, Bound};
use crate::{EXIT_CODES, Failure, Section, markdown};

#[derive(Debug, Subcommand)]
pub(crate) enum HeatCommand {
    /// The generic metrics per repository, provider and session kind: each says whether it is
    /// exact or an estimate and whether it compares across runners.
    #[command(after_help = EXIT_CODES)]
    Metrics(MetricsArgs),
}

impl HeatCommand {
    /// The repository the command is limited to, which picks the identity that owns it.
    pub(crate) fn repo(&self) -> Option<&str> {
        match self {
            Self::Metrics(args) => args.repo.as_deref(),
        }
    }

    /// The range `[since, until)` of the command.
    pub(crate) fn range(&self, now_ms: i64) -> (i64, i64) {
        match self {
            Self::Metrics(args) => args.range(now_ms),
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

/// One identity's section of `heat`.
pub(crate) async fn section(
    client: &mut Client,
    identity: &str,
    command: &HeatCommand,
    now_ms: i64,
) -> Result<Section, Failure> {
    match command {
        HeatCommand::Metrics(args) => {
            let (from_ms, to_ms) = args.range(now_ms);
            let ask = ReportParams {
                identity: identity.to_owned(),
                from_ms,
                to_ms,
                repo: args.repo.clone(),
            };
            let metrics = client.call::<method::HeatMetrics>(&ask).await?;
            Section::of(
                &metrics.identity.clone(),
                &metrics,
                markdown::heat::heat_metrics,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::{Cli, Command};

    #[test]
    fn the_range_of_heat_metrics_is_fourteen_days_before_now_by_default() {
        let cli = Cli::try_parse_from(["bosun", "heat", "metrics", "--repo", "alpha"]).unwrap();
        let Command::Heat(command) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(command.repo(), Some("alpha"));
        let now = 2_000_000_000_000;
        assert_eq!(command.range(now), (now - 14 * 86_400_000, now));
    }
}
