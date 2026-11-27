//! `bosun heat metrics` as Markdown (#11, part D): one document per identity, the metrics of
//! `heat.metrics` in one table, in the order the daemon gives them. Each row says whether it
//! is exact or an estimate (with the ratio of bytes to tokens it was made with) and whether it
//! compares across runners. It prints numbers and draws no conclusions.
//!
//! A share in parts per million prints as a percent with one decimal; every other value is
//! printed in full in its own unit.

use std::fmt::Write as _;

use bosun_api::{HeatMetric, HeatMetrics};

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
