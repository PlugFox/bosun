//! The report as Markdown (#26): one document per identity, the sections of `report.exact` in
//! order, as GitHub-flavoured tables. It prints numbers and draws no conclusions.
//!
//! Token and message counts are printed in full; eqv is rounded to a whole token, the cost in
//! USD to 1/100 of a cent, shares and changes to a tenth of a percent.

use std::fmt::Write as _;

use bosun_api::{CheckTokens, CommitPeriod, Comparison, RepoRef, Report, ScanState, TokenSums};

use crate::render::usd;
use crate::time;

/// A Markdown table: a header row, an alignment row, the body.
struct Table {
    header: Vec<&'static str>,
    /// `true` for a right-aligned (numeric) column.
    right: Vec<bool>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn new(columns: &[(&'static str, bool)]) -> Self {
        Self {
            header: columns.iter().map(|(h, _)| *h).collect(),
            right: columns.iter().map(|(_, r)| *r).collect(),
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn render(&self, out: &mut String) {
        let line = |out: &mut String, cells: &mut dyn Iterator<Item = String>| {
            out.push('|');
            for cell in cells {
                let _ = write!(out, " {} |", escape(&cell));
            }
            out.push('\n');
        };
        line(out, &mut self.header.iter().map(|h| (*h).to_owned()));
        line(
            out,
            &mut self
                .right
                .iter()
                .map(|r| if *r { "---:" } else { "---" }.to_owned()),
        );
        for row in &self.rows {
            line(out, &mut row.iter().cloned());
        }
        out.push('\n');
    }
}

/// A cell's text with the column separator escaped and angle brackets as entities, so that a
/// model named `<synthetic>` is not read as an HTML tag.
fn escape(cell: &str) -> String {
    cell.replace('|', "\\|")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn eqv(value: f64) -> String {
    format!("{value:.0}")
}

/// `part / whole` as a percentage; empty when `whole` is zero.
fn share(part: f64, whole: f64) -> String {
    if whole > 0.0 {
        format!("{:.1}%", part / whole * 100.0)
    } else {
        String::new()
    }
}

fn change(value: Option<f64>) -> String {
    value.map_or_else(String::new, |c| format!("{:+.1}%", c * 100.0))
}

fn repo(repo: Option<&RepoRef>) -> String {
    match repo {
        Some(RepoRef {
            name: Some(name), ..
        }) => name.clone(),
        Some(RepoRef { name: None, path }) => path.clone(),
        None => "(no repository)".to_owned(),
    }
}

fn sum_cells(sums: &TokenSums) -> Vec<String> {
    vec![
        sums.messages.to_string(),
        sums.input.to_string(),
        sums.cache_write_5m.to_string(),
        sums.cache_write_1h.to_string(),
        sums.cache_read.to_string(),
        sums.output.to_string(),
        eqv(sums.eqv),
        usd(sums.cost),
    ]
}

const SUM_COLUMNS: [(&str, bool); 8] = [
    ("messages", true),
    ("input", true),
    ("cache write 5m", true),
    ("cache write 1h", true),
    ("cache read", true),
    ("output", true),
    ("eqv", true),
    ("cost USD", true),
];

/// One identity's report.
pub(crate) fn report(report: &Report, out: &mut String) {
    let _ = writeln!(out, "# bosun report: {}\n", report.identity);
    let scope = report
        .repo
        .as_ref()
        .map_or_else(|| "every repository".to_owned(), |r| repo(Some(r)));
    let _ = writeln!(
        out,
        "Range: {} to {} (end excluded). Scope: {scope}.\n",
        time::format(report.from_ms),
        time::format(report.to_ms)
    );
    let _ = writeln!(
        out,
        "Every number is exact: raw token counts from the transcripts, priced at the rates \
         valid at each message's time. Eqv is input-equivalent tokens of {}.\n",
        report.reference_model
    );
    total(report, out);
    categories(report, out);
    models(report, out);
    kinds(report, out);
    per_commit(report, out);
    health(report, out);
    unpriced(report, out);
}

fn total(report: &Report, out: &mut String) {
    out.push_str("## Total\n\n");
    let mut table = Table::new(&[
        ("", false),
        ("sessions", true),
        ("messages", true),
        ("eqv", true),
        ("cost USD", true),
    ]);
    let sessions: u64 = report.by_kind.iter().map(|k| k.sessions).sum();
    let real = &report.total;
    table.row(vec![
        "real total".to_owned(),
        sessions.to_string(),
        real.messages.to_string(),
        eqv(real.eqv),
        usd(real.cost),
    ]);
    let old = &report.old_method;
    table.row(vec![
        "old method: the last usage block per session".to_owned(),
        old.sessions.to_string(),
        old.sums.messages.to_string(),
        eqv(old.sums.eqv),
        usd(old.sums.cost),
    ]);
    table.render(out);
    if old.sums.eqv > 0.0 {
        let _ = writeln!(
            out,
            "The real total is {:.1}x the old method's number.\n",
            real.eqv / old.sums.eqv
        );
    }
}

fn categories(report: &Report, out: &mut String) {
    out.push_str("## By category\n\n");
    let mut table = Table::new(&[
        ("category", false),
        ("tokens", true),
        ("eqv", true),
        ("share of eqv", true),
        ("cost USD", true),
    ]);
    for row in &report.by_category {
        table.row(vec![
            row.category.replace('_', " "),
            row.tokens.to_string(),
            eqv(row.eqv),
            share(row.eqv, report.total.eqv),
            usd(row.cost),
        ]);
    }
    table.render(out);
}

fn models(report: &Report, out: &mut String) {
    out.push_str("## By model\n\n");
    let mut columns = vec![("model", false)];
    columns.extend(SUM_COLUMNS);
    let mut table = Table::new(&columns);
    for row in &report.by_model {
        let mut cells = vec![row.model.clone()];
        cells.extend(sum_cells(&row.sums));
        table.row(cells);
    }
    let mut cells = vec!["total".to_owned()];
    cells.extend(sum_cells(&report.total));
    table.row(cells);
    table.render(out);
}

fn kinds(report: &Report, out: &mut String) {
    out.push_str("## By session kind\n\n");
    let mut table = Table::new(&[
        ("kind", false),
        ("sessions", true),
        ("messages", true),
        ("eqv", true),
        ("share of eqv", true),
        ("cost USD", true),
    ]);
    for row in &report.by_kind {
        table.row(vec![
            row.kind.clone(),
            row.sessions.to_string(),
            row.sums.messages.to_string(),
            eqv(row.sums.eqv),
            share(row.sums.eqv, report.total.eqv),
            usd(row.sums.cost),
        ]);
    }
    table.render(out);
}

fn period_cells(period: &CommitPeriod) -> [String; 3] {
    [
        period.commits.to_string(),
        eqv(period.eqv),
        period.eqv_per_commit.map_or_else(String::new, eqv),
    ]
}

fn per_commit(report: &Report, out: &mut String) {
    let section = &report.per_commit;
    out.push_str("## Eqv per commit\n\n");
    let _ = writeln!(
        out,
        "Eqv of the messages in a repository over its commits, both in the period; the \
         previous period is {} to {}.\n",
        time::format(section.previous_from_ms),
        time::format(section.previous_to_ms)
    );
    let mut table = Table::new(&[
        ("repository", false),
        ("commits", true),
        ("eqv", true),
        ("eqv per commit", true),
        ("previous commits", true),
        ("previous eqv", true),
        ("previous eqv per commit", true),
        ("change", true),
    ]);
    let mut add = |name: String, trend: &bosun_api::Trend| {
        let mut cells = vec![name];
        cells.extend(period_cells(&trend.current));
        cells.extend(period_cells(&trend.previous));
        cells.push(change(trend.change));
        table.row(cells);
    };
    for row in &section.rows {
        add(repo(row.repo.as_ref()), &row.trend);
    }
    add("in repositories".to_owned(), &section.attributed);
    table.render(out);
    if !section.scans.is_empty() {
        out.push_str("Commit reader:\n\n");
        for scan in &section.scans {
            let state = match &scan.state {
                ScanState::Pending => "not scanned yet".to_owned(),
                ScanState::Done => "scanned".to_owned(),
                ScanState::Failed { error } => format!("failed: {error}"),
            };
            let _ = writeln!(out, "- {}: {state}", repo(Some(&scan.repo)));
        }
        out.push('\n');
    }
}

fn check_cells(label: &str, tokens: &CheckTokens) -> Vec<String> {
    vec![
        label.to_owned(),
        tokens.input.to_string(),
        tokens.cache_write.to_string(),
        tokens.cache_read.to_string(),
        tokens.output.to_string(),
        tokens.total.to_string(),
    ]
}

/// `(ours - theirs) / theirs` as a signed percentage; empty when `theirs` is zero.
fn difference(ours: u64, theirs: u64) -> String {
    if theirs == 0 {
        return String::new();
    }
    #[allow(clippy::cast_precision_loss)] // Display only.
    let relative = (ours as f64 - theirs as f64) / theirs as f64 * 100.0;
    format!("{relative:+.1}%")
}

fn health(report: &Report, out: &mut String) {
    let health = &report.health;
    out.push_str("## Ingest health\n\n");
    out.push_str("Over the whole store, whatever the range.\n\n");
    let mut table = Table::new(&[
        ("parser version", false),
        ("files", true),
        ("files with a line count", true),
        ("lines", true),
        ("gaps", true),
        ("gaps per line", true),
    ]);
    let per_line = |gaps: u64, lines: u64| {
        #[allow(clippy::cast_precision_loss)] // Display only.
        let value = share(gaps as f64, lines as f64);
        value
    };
    for row in &health.versions {
        table.row(vec![
            row.parser_version.to_string(),
            row.files.to_string(),
            row.files_counted.to_string(),
            row.lines.to_string(),
            row.gaps.to_string(),
            per_line(row.gaps, row.lines),
        ]);
    }
    table.row(vec![
        "total".to_owned(),
        health.files.to_string(),
        health.files_counted.to_string(),
        health.lines.to_string(),
        health.gaps.to_string(),
        per_line(health.gaps, health.lines),
    ]);
    table.render(out);
    if health.files_counted < health.files {
        let _ = writeln!(
            out,
            "{} files were parsed before lines were counted; `bosun reingest` counts them.\n",
            health.files - health.files_counted
        );
    }

    let agreement = &health.agreement;
    out.push_str("### Agreement with the provider's own totals\n\n");
    let _ = writeln!(
        out,
        "Sessions with the provider's totals (the last `cost-state` record) and a message in \
         the range, main transcript plus sub-agents, over all time: {}; within {}% on the \
         total: {}.\n",
        agreement.sessions, agreement.tolerance_percent, agreement.within
    );
    if agreement.sessions == 0 {
        return;
    }
    let total: &Comparison = &agreement.total;
    let mut table = Table::new(&[
        ("all sessions", false),
        ("input", true),
        ("cache write", true),
        ("cache read", true),
        ("output", true),
        ("total", true),
    ]);
    table.row(check_cells("transcripts", &total.transcripts));
    table.row(check_cells("provider", &total.provider));
    let (ours, theirs) = (&total.transcripts, &total.provider);
    table.row(vec![
        "difference".to_owned(),
        difference(ours.input, theirs.input),
        difference(ours.cache_write, theirs.cache_write),
        difference(ours.cache_read, theirs.cache_read),
        difference(ours.output, theirs.output),
        difference(ours.total, theirs.total),
    ]);
    table.render(out);

    let mut table = Table::new(&[
        ("session", false),
        ("windows", true),
        ("messages", true),
        ("transcripts", true),
        ("provider", true),
        ("difference", true),
        ("within", false),
    ]);
    for row in &agreement.rows {
        let c = &row.comparison;
        table.row(vec![
            row.session.clone(),
            row.windows.to_string(),
            row.messages.to_string(),
            c.transcripts.total.to_string(),
            c.provider.total.to_string(),
            difference(c.transcripts.total, c.provider.total),
            if c.within { "yes" } else { "no" }.to_owned(),
        ]);
    }
    table.render(out);
}

fn unpriced(report: &Report, out: &mut String) {
    if report.unpriced.is_empty() {
        return;
    }
    out.push_str("## Unpriced\n\n");
    out.push_str(
        "Models with no price at the time of some messages: their tokens are counted, their \
         cost and eqv are not.\n\n",
    );
    let mut table = Table::new(&[("model", false), ("messages", true)]);
    for model in &report.unpriced {
        table.row(vec![model.model.clone(), model.messages.to_string()]);
    }
    table.render(out);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn golden(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    /// The Markdown snapshot of the golden report of `bosun-heat` (the result of `report.exact`
    /// over the synthetic corpus). Run with `BOSUN_BLESS=1` to rewrite it.
    #[test]
    fn the_golden_report_renders_as_the_snapshot() {
        let json =
            std::fs::read_to_string(golden("../bosun-heat/tests/golden/report.json")).unwrap();
        let data: Report = serde_json::from_str(&json).unwrap();
        let mut text = String::new();
        report(&data, &mut text);
        let snapshot = golden("tests/golden/report.md");
        if std::env::var_os("BOSUN_BLESS").is_some() {
            std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
            std::fs::write(&snapshot, &text).unwrap();
        }
        let expected = std::fs::read_to_string(&snapshot)
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            text == expected,
            "the Markdown differs from {}; rerun with BOSUN_BLESS=1 and review the diff:\n{text}",
            snapshot.display()
        );
    }

    #[test]
    fn cells_escape_the_column_separator_and_tags() {
        let mut table = Table::new(&[("a", false)]);
        table.row(vec!["x|y <b>".to_owned()]);
        let mut out = String::new();
        table.render(&mut out);
        assert_eq!(out, "| a |\n| --- |\n| x\\|y &lt;b&gt; |\n\n");
    }
}
