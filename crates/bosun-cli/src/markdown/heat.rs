//! `bosun heat metrics` and `bosun heat` as Markdown (#11, part D).
//!
//! `heat metrics`: one document per identity, the metrics of `heat.metrics` in one table, in
//! the order the daemon gives them. Each row says whether it is exact or an estimate (with the
//! ratio of bytes to tokens it was made with) and whether it compares across runners. It prints
//! numbers and draws no conclusions. A share in parts per million prints as a percent with one
//! decimal; every other value is printed in full in its own unit.
//!
//! `heat`: one document per identity, its proposals of `heat.list` in one table, one row each:
//! its metric and unit, current value, threshold, target, window (dates, UTC) and sample, all
//! exact, and apart, in a column whose header says so, the estimated impact in eqv tokens
//! (empty when there is none). Below the table, the first [`EVIDENCE_LINES`] evidence lines of
//! each proposal, as stored (redacted).

use std::fmt::Write as _;

use bosun_api::{HeatMetric, HeatMetrics, HeatProposals};

use super::{Table, repo};
use crate::time;

/// A value in its unit: a share in ppm as a percent with one decimal, else the integer.
fn value(metric: &HeatMetric) -> String {
    if metric.unit == "ppm" {
        // Integer arithmetic: tenths of a percent are ppm / 1000, rounded half up.
        let tenths = (metric.value + 500) / 1000;
        format!("{}.{}%", tenths / 10, tenths % 10)
    } else {
        metric.value.to_string()
    }
}

/// Whether the figure is exact or an estimate, with the estimate's ratio.
fn exactness(metric: &HeatMetric) -> String {
    if cfg!(mutant_heat_metrics_md_estimate_unlabelled) {
        // Mutant (`mutant_heat_metrics_md_estimate_unlabelled`): every row says `exact`.
        return "exact".to_owned();
    }
    if metric.estimate {
        match metric.bytes_per_token {
            Some(ratio) => format!("estimate (bytes_per_token = {ratio})"),
            None => "estimate".to_owned(),
        }
    } else {
        "exact".to_owned()
    }
}

/// One identity's heat metrics.
pub(crate) fn heat_metrics(metrics: &HeatMetrics, out: &mut String) {
    let _ = writeln!(out, "# bosun heat metrics: {}\n", metrics.identity);
    let scope = metrics
        .repo
        .as_ref()
        .map_or_else(|| "every repository".to_owned(), |r| repo(Some(r)));
    let _ = writeln!(
        out,
        "Range: {} to {} (end excluded). Scope: {scope}.\n",
        time::format(metrics.from_ms),
        time::format(metrics.to_ms)
    );
    if metrics.metrics.is_empty() {
        out.push_str("Nothing in the range: no metric has a sample.\n");
        return;
    }
    let mut table = Table::new(&[
        ("repository", false),
        ("provider", false),
        ("kind", false),
        ("metric", false),
        ("value", true),
        ("unit", false),
        ("total", true),
        ("sample", true),
        ("exact or estimate", false),
        ("comparable across runners", false),
    ]);
    for metric in &metrics.metrics {
        table.row(vec![
            repo(Some(&metric.repo)),
            metric.provider.clone(),
            metric.kind.clone().unwrap_or_default(),
            metric.id.clone(),
            value(metric),
            metric.unit.clone(),
            metric.total.to_string(),
            format!("{} {}", metric.sample, metric.sample_unit),
            exactness(metric),
            if metric.runner_comparable {
                "yes"
            } else {
                "no, within one provider"
            }
            .to_owned(),
        ]);
    }
    table.render(out);
}

/// The evidence lines printed per proposal.
const EVIDENCE_LINES: usize = 3;

/// A time as its date, UTC.
fn date(ms: i64) -> String {
    time::format(ms).chars().take(10).collect()
}

/// One identity's heat proposals.
pub(crate) fn heat_proposals(list: &HeatProposals, out: &mut String) {
    let _ = writeln!(out, "# bosun heat: {}\n", list.identity);
    if list.proposals.is_empty() {
        out.push_str("No proposal.\n");
        return;
    }
    // Mutant (`mutant_heat_list_drops_target`): the target column is left out.
    let target = !cfg!(mutant_heat_list_drops_target);
    let mut columns = vec![
        ("key", false),
        ("repository", false),
        ("provider", false),
        ("detector", false),
        ("subject", false),
        ("metric", false),
        ("unit", false),
        ("value", true),
        ("threshold", true),
    ];
    if target {
        columns.push(("target", true));
    }
    columns.extend([
        ("window from", false),
        ("window to", false),
        ("sample", true),
        ("state", false),
        ("impact, eqv tokens (estimate)", true),
    ]);
    let mut table = Table::new(&columns);
    for p in &list.proposals {
        let mut cells = vec![
            p.key.clone(),
            repo(Some(&p.repo)),
            p.provider.clone(),
            p.detector.clone(),
            p.subject.clone(),
            p.metric.clone(),
            p.unit.clone(),
            p.baseline_value.to_string(),
            p.threshold.to_string(),
        ];
        if target {
            cells.push(p.target_value.to_string());
        }
        cells.extend([
            date(p.window_from_ms),
            date(p.window_to_ms),
            p.sample.to_string(),
            p.state.clone(),
            p.impact_eqv_estimate
                .map_or_else(String::new, |i| i.to_string()),
        ]);
        table.row(cells);
    }
    table.render(out);
    for p in &list.proposals {
        if p.evidence.is_empty() {
            continue;
        }
        let _ = writeln!(out, "Evidence of {}:\n", p.key);
        for line in p.evidence.iter().take(EVIDENCE_LINES) {
            let _ = writeln!(out, "- {}", super::escape(line));
        }
        if p.evidence.len() > EVIDENCE_LINES {
            let _ = writeln!(out, "- ({} more)", p.evidence.len() - EVIDENCE_LINES);
        }
        out.push('\n');
    }
}
