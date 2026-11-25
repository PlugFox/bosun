//! The report as Markdown (#26, #27): one document per identity, the sections of
//! `report.exact` in order, then, under a heading of their own, those of `report.estimates`,
//! as GitHub-flavoured tables. It prints numbers and draws no conclusions.
//!
//! Token and message counts are printed in full; eqv is rounded to a whole token, the cost in
//! USD to 1/100 of a cent, shares and changes to a tenth of a percent. Long lists (context
//! windows, files, sessions) print their first [`LIMIT`] rows; the JSON has them all.

use std::fmt::Write as _;

use bosun_api::{
    Agreement, CheckTokens, CommitPeriod, Comparison, Estimates, RepoRef, Report, ResultCost,
    ScanState, TokenSums,
};
use serde::Serialize;

use crate::render::usd;

mod items;
use crate::time;

/// The rows a long list prints.
const LIMIT: usize = 20;

/// The report of one identity, as `bosun report` prints it in JSON: the exact part and the
/// estimates, side by side and never mixed.
#[derive(Debug, Serialize)]
pub(crate) struct Document {
    pub(crate) exact: Report,
    pub(crate) estimates: Estimates,
}

/// One identity's report: the exact sections, then the estimates.
pub(crate) fn document(document: &Document, out: &mut String) {
    report(&document.exact, out);
    estimates(&document.estimates, out);
}

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
    context(report, out);
    health(report, out);
    unpriced(report, out);
    items::items(&report.items, out);
    items::lease_waits(&report.items, out);
    items::worktrees(&report.items, out);
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

/// A signed count with its sign.
fn signed(n: i64) -> String {
    format!("{n:+}")
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

    agreement(&health.agreement, out);
}

fn agreement(agreement: &Agreement, out: &mut String) {
    out.push_str("### Agreement with the provider's own totals\n\n");
    let _ = writeln!(
        out,
        "Sessions with the provider's totals (the last `cost-state` record of the main \
         transcript) and a message in the range, main transcript plus sub-agents, over the \
         messages inside the provider's window (from its `startTime` to the last record before \
         the line): {}; within {}% on the total: {}; transcripts larger by more than {}%: {}. \
         Not compared, with no message inside the window: {}.\n",
        agreement.sessions,
        agreement.tolerance_percent,
        agreement.within,
        agreement.tolerance_percent,
        agreement.exceeding,
        agreement.excluded
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

    let undercount = &total.undercount;
    let _ = writeln!(
        out,
        "Signed difference, transcripts less provider: {} tokens. The provider's excess is the \
         known undercount of the transcript method, calls its transcripts never hold: {} on \
         models no transcript holds, {} on the other models.\n",
        signed(total.difference),
        undercount.absent_models,
        signed(undercount.shared_models)
    );
    if !agreement.absent_models.is_empty() {
        let mut table = Table::new(&[
            ("model absent from the transcripts", false),
            ("sessions", true),
            ("provider total", true),
        ]);
        for model in &agreement.absent_models {
            table.row(vec![
                model.model.clone(),
                model.sessions.to_string(),
                model.total.to_string(),
            ]);
        }
        table.render(out);
    }

    let mut table = Table::new(&[
        ("session", false),
        ("windows", true),
        ("messages", true),
        ("transcripts", true),
        ("provider", true),
        ("difference", true),
        ("relative", true),
        ("absent models", true),
        ("other models", true),
        ("within", false),
    ]);
    for row in &agreement.rows {
        let c = &row.comparison;
        let within = if c.exceeds {
            "no, transcripts larger"
        } else if c.within {
            "yes"
        } else {
            "no"
        };
        table.row(vec![
            row.session.clone(),
            row.windows.to_string(),
            row.messages.to_string(),
            c.transcripts.total.to_string(),
            c.provider.total.to_string(),
            signed(c.difference),
            difference(c.transcripts.total, c.provider.total),
            c.undercount.absent_models.to_string(),
            signed(c.undercount.shared_models),
            within.to_owned(),
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

/// Says how many rows of `total` were left out after the first [`LIMIT`].
fn more(total: usize, out: &mut String) {
    if total > LIMIT {
        let _ = writeln!(
            out,
            "{} more rows are in the JSON (`--format json`).\n",
            total - LIMIT
        );
    }
}

fn context(report: &Report, out: &mut String) {
    let curve = &report.context;
    out.push_str("## Context curve\n\n");
    let _ = writeln!(
        out,
        "Prompt size (input plus cache reads and writes) per context window; a compaction is a \
         drop of it between consecutive messages. Windows: {}; compactions: {}.\n",
        curve.windows, curve.compactions
    );
    if curve.rows.is_empty() {
        return;
    }
    let mut rows: Vec<_> = curve.rows.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.peak));
    let mut table = Table::new(&[
        ("session", false),
        ("agent", false),
        ("messages", true),
        ("start", true),
        ("peak", true),
        ("compactions", true),
    ]);
    for row in rows.iter().take(LIMIT) {
        table.row(vec![
            row.session.clone(),
            row.agent.clone().unwrap_or_default(),
            row.messages.to_string(),
            row.start.to_string(),
            row.peak.to_string(),
            row.compactions.to_string(),
        ]);
    }
    table.render(out);
    more(rows.len(), out);
}

/// One identity's estimates, apart from the exact sections.
pub(crate) fn estimates(estimates: &Estimates, out: &mut String) {
    out.push_str("---\n\n## Estimates\n\n");
    let _ = writeln!(
        out,
        "Every number below is an estimate and is never added to the exact figures above. \
         Each prices tool results (bytes_per_token = {}): {}.\n",
        estimates.bytes_per_token, estimates.formula
    );
    out.push_str("Known gaps:\n\n");
    for gap in &estimates.known_gaps {
        let _ = writeln!(out, "- {gap}");
    }
    out.push('\n');
    by_tool(estimates, out);
    by_file(estimates, out);
    repeated_reads(estimates, out);
    finding(estimates, out);
    items::inferred(&estimates.items, out);
}

const RESULT_COLUMNS: [(&str, bool); 5] = [
    ("calls", true),
    ("est. tokens", true),
    ("carry turns", true),
    ("est. eqv", true),
    ("of it carry", true),
];

fn result_cells(results: &ResultCost) -> Vec<String> {
    vec![
        results.calls.to_string(),
        results.tokens.to_string(),
        results.carry_turns.to_string(),
        eqv(results.eqv),
        eqv(results.carry_eqv),
    ]
}

fn by_tool(estimates: &Estimates, out: &mut String) {
    let section = &estimates.by_tool;
    out.push_str("### Cost by tool (estimate)\n\n");
    let mut columns = vec![("tool", false), ("errors", true)];
    columns.extend(RESULT_COLUMNS);
    let mut table = Table::new(&columns);
    for row in &section.rows {
        let mut cells = vec![row.name.clone(), row.errors.to_string()];
        cells.extend(result_cells(&row.results));
        table.row(cells);
    }
    let errors: u64 = section.rows.iter().map(|r| r.errors).sum();
    let mut cells = vec!["all tools".to_owned(), errors.to_string()];
    cells.extend(result_cells(&section.total));
    table.row(cells);
    table.render(out);
}

fn by_file(estimates: &Estimates, out: &mut String) {
    let section = &estimates.by_file;
    out.push_str("### Cost by file (estimate)\n\n");
    if section.rows.is_empty() {
        out.push_str("No file was touched.\n\n");
        return;
    }
    let mut columns = vec![
        ("repository", false),
        ("path", false),
        ("reads", true),
        ("searches", true),
        ("writes", true),
        ("execs", true),
        ("unknown", true),
        ("sessions", true),
    ];
    columns.extend(RESULT_COLUMNS);
    let mut table = Table::new(&columns);
    for row in section.rows.iter().take(LIMIT) {
        let mut cells = vec![
            repo(row.repo.as_ref()),
            row.path.clone(),
            row.reads.to_string(),
            row.searches.to_string(),
            row.writes.to_string(),
            row.execs.to_string(),
            row.unknowns.to_string(),
            row.sessions.to_string(),
        ];
        cells.extend(result_cells(&row.results));
        table.row(cells);
    }
    table.render(out);
    more(section.rows.len(), out);
}

fn repeated_reads(estimates: &Estimates, out: &mut String) {
    let section = &estimates.repeated_reads;
    out.push_str("### Repeated reads (estimate)\n\n");
    let _ = writeln!(
        out,
        "Within a session: {} reads, {} est. eqv. Across sessions: {} reads, {} est. eqv.\n",
        section.within.calls,
        eqv(section.within.eqv),
        section.across.calls,
        eqv(section.across.eqv)
    );
    if section.rows.is_empty() {
        return;
    }
    let mut table = Table::new(&[
        ("repository", false),
        ("path", false),
        ("reads", true),
        ("sessions", true),
        ("repeats within", true),
        ("est. eqv within", true),
        ("repeats across", true),
        ("est. eqv across", true),
    ]);
    for row in section.rows.iter().take(LIMIT) {
        table.row(vec![
            repo(row.repo.as_ref()),
            row.path.clone(),
            row.reads.to_string(),
            row.sessions.to_string(),
            row.within.calls.to_string(),
            eqv(row.within.eqv),
            row.across.calls.to_string(),
            eqv(row.across.eqv),
        ]);
    }
    table.render(out);
    more(section.rows.len(), out);
}

fn finding(estimates: &Estimates, out: &mut String) {
    let section = &estimates.finding;
    out.push_str("### Cost of finding (estimate)\n\n");
    let _ = writeln!(
        out,
        "Reads and searches before a session's first write: {} sessions, {} calls, {} est. \
         eqv. Sessions that wrote nothing: {}, {} calls, {} est. eqv.\n",
        section.sessions_with_write,
        section.before_write.calls,
        eqv(section.before_write.eqv),
        section.sessions_without_write,
        section.without_write.calls,
        eqv(section.without_write.eqv)
    );
    if section.rows.is_empty() {
        return;
    }
    let mut rows: Vec<_> = section.rows.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.results.cost.0));
    let mut columns = vec![("session", false), ("first write", false)];
    columns.extend(RESULT_COLUMNS);
    let mut table = Table::new(&columns);
    for row in rows.iter().take(LIMIT) {
        let mut cells = vec![
            row.session.clone(),
            row.first_write_ms
                .map_or_else(|| "none".to_owned(), time::format),
        ];
        cells.extend(result_cells(&row.results));
        table.row(cells);
    }
    table.render(out);
    more(rows.len(), out);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn golden(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    /// The Markdown snapshot of the golden report of `bosun-heat` (the results of
    /// `report.exact` and `report.estimates` over the synthetic corpus). Run with
    /// `BOSUN_BLESS=1` to rewrite it.
    #[test]
    fn the_golden_report_renders_as_the_snapshot() {
        snapshot("report.json", "estimates.json", "tests/golden/report.md");
    }

    /// The same for the golden report of the seeded loop store of `bosun-heat` (#72): items,
    /// attempts, commits of the runs and the inferred sessions.
    #[test]
    fn the_golden_items_report_renders_as_the_snapshot() {
        snapshot(
            "items-report.json",
            "items-estimates.json",
            "tests/golden/report-items.md",
        );
    }

    fn snapshot(exact: &str, estimates: &str, markdown: &str) {
        let read = |name: &str| {
            std::fs::read_to_string(golden(&format!("../bosun-heat/tests/golden/{name}"))).unwrap()
        };
        let data = Document {
            exact: serde_json::from_str(&read(exact)).unwrap(),
            estimates: serde_json::from_str(&read(estimates)).unwrap(),
        };
        let mut text = String::new();
        document(&data, &mut text);
        let snapshot = golden(markdown);
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

    /// #8: the waits for two resources, longest first, with their counts and waits, and two
    /// worktrees with their sizes, the quarantined one's blocking file and the total; a report
    /// with neither prints neither heading. `mutant_report_drop_lease_waits` drops the waits.
    #[test]
    fn the_report_has_lease_waits_and_worktree_disk_use() {
        let read = std::fs::read_to_string(golden("../bosun-heat/tests/golden/report.json"));
        let mut report: Report = serde_json::from_str(&read.unwrap()).unwrap();
        report.items.lease_waits.clear();
        report.items.worktrees.clear();
        let mut text = String::new();
        super::report(&report, &mut text);
        assert!(!text.contains("## Lease waits"), "{text}");
        assert!(!text.contains("## Worktree disk use"), "{text}");

        report.items.lease_waits = vec![
            bosun_api::LeaseWaits {
                resource: "rig".to_owned(),
                requests: 3,
                total_ms: 95_400,
                longest_ms: 61_250,
            },
            bosun_api::LeaseWaits {
                resource: "cache/alpha".to_owned(),
                requests: 1,
                total_ms: 800,
                longest_ms: 800,
            },
        ];
        report.items.worktrees = vec![
            bosun_api::WorktreeUse {
                item: "alpha#3".to_owned(),
                path: "/work/worktrees/alpha-3".to_owned(),
                kind: "item".to_owned(),
                state: "created".to_owned(),
                bytes: Some(3 * 1024 * 1024),
                blocking: None,
            },
            bosun_api::WorktreeUse {
                item: "alpha#2".to_owned(),
                path: "/work/worktrees/alpha-2".to_owned(),
                kind: "verdict".to_owned(),
                state: "quarantined".to_owned(),
                bytes: Some(512 * 1024),
                blocking: Some("target/lock".to_owned()),
            },
        ];
        let mut text = String::new();
        super::report(&report, &mut text);
        let waits = text
            .split("## Lease waits\n\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no lease waits:\n{text}"));
        let rig = waits.find("| rig | 3 | 95.4 s | 61.2 s |");
        let cache = waits.find("| cache/alpha | 1 | 0.8 s | 0.8 s |");
        assert!(
            rig.is_some() && cache.is_some() && rig < cache,
            "the waits, longest first:\n{waits}"
        );
        let disk = text
            .split("## Worktree disk use\n\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no worktree disk use:\n{text}"));
        for row in [
            "| alpha#3 | item | created | 3.0 MiB | /work/worktrees/alpha-3 |",
            "| alpha#2 | verdict | quarantined (blocked by target/lock) | 512.0 KiB | \
             /work/worktrees/alpha-2 |",
            "| total of 2 |  |  | 3.5 MiB |  |",
        ] {
            assert!(disk.contains(row), "{row}\n{disk}");
        }
        assert!(
            text.find("## Lease waits") > text.find("### Commits of the runs"),
            "after the existing sections"
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
