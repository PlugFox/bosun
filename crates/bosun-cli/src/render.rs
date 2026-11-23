//! Text output: one section per identity, headed `== <identity> ==`, and plain tables.
//!
//! Token and message counts are printed in full, as plain integers, and come first; eqv
//! (rounded to a whole token) and the cost in USD (rounded to 1/100 of a cent) follow them.

use std::fmt::Write as _;

use bosun_api::{
    IngestStatus, LastPass, ModelRow, PassReport, Picodollars, ReingestResult, RepoRow, SessionRow,
    TokenSums, TokensSummary,
};

use crate::time;

/// The header of an identity's section.
pub(crate) fn header(identity: &str) -> String {
    format!("== {identity} ==\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

/// A table with a header row, body rows and an optional total row under a rule.
#[derive(Debug)]
struct Table {
    columns: Vec<(&'static str, Align)>,
    rows: Vec<Vec<String>>,
    total: Option<Vec<String>>,
}

impl Table {
    fn render(&self, out: &mut String) {
        let mut widths: Vec<usize> = self.columns.iter().map(|(h, _)| h.len()).collect();
        for row in self.rows.iter().chain(&self.total) {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let line = |out: &mut String, cells: &mut dyn Iterator<Item = &str>| {
            let mut text = String::new();
            for (i, ((_, align), cell)) in self.columns.iter().zip(cells).enumerate() {
                if i > 0 {
                    text.push_str("  ");
                }
                let width = widths[i];
                let _ = match align {
                    Align::Left => write!(text, "{cell:<width$}"),
                    Align::Right => write!(text, "{cell:>width$}"),
                };
            }
            out.push_str(text.trim_end());
            out.push('\n');
        };
        line(out, &mut self.columns.iter().map(|(h, _)| *h));
        for row in &self.rows {
            line(out, &mut row.iter().map(String::as_str));
        }
        if let Some(total) = &self.total {
            let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            line(out, &mut rule.iter().map(String::as_str));
            line(out, &mut total.iter().map(String::as_str));
        }
    }
}

/// An amount in USD, rounded half up to four decimals, computed exactly from picodollars.
pub(crate) fn usd(cost: Picodollars) -> String {
    let units = (cost.0 + 50_000_000) / 100_000_000;
    format!("{}.{:04}", units / 10_000, units % 10_000)
}

/// The sum columns, in order.
const SUM_COLUMNS: [(&str, Align); 8] = [
    ("messages", Align::Right),
    ("input", Align::Right),
    ("cache write 5m", Align::Right),
    ("cache write 1h", Align::Right),
    ("cache read", Align::Right),
    ("output", Align::Right),
    ("eqv", Align::Right),
    ("cost USD", Align::Right),
];

fn sum_cells(sums: &TokenSums) -> [String; 8] {
    [
        sums.messages.to_string(),
        sums.input.to_string(),
        sums.cache_write_5m.to_string(),
        sums.cache_write_1h.to_string(),
        sums.cache_read.to_string(),
        sums.output.to_string(),
        format!("{:.0}", sums.eqv),
        usd(sums.cost),
    ]
}

fn sums_table<'a>(
    keys: &[(&'static str, Align)],
    rows: impl Iterator<Item = (Vec<String>, &'a TokenSums)>,
    total: &TokenSums,
) -> Table {
    let mut columns = keys.to_vec();
    columns.extend(SUM_COLUMNS);
    let rows = rows
        .map(|(mut cells, sums)| {
            cells.extend(sum_cells(sums));
            cells
        })
        .collect();
    let mut total_row = vec!["total".to_owned()];
    total_row.resize(keys.len(), String::new());
    total_row.extend(sum_cells(total));
    Table {
        columns,
        rows,
        total: Some(total_row),
    }
}

fn summary_lines(summary: &TokensSummary, out: &mut String) {
    let range = match (summary.from_ms, summary.to_ms) {
        (None, None) => "all time".to_owned(),
        (from, to) => format!(
            "from {} until {}",
            from.map_or_else(|| "the beginning".to_owned(), time::format),
            to.map_or_else(|| "now".to_owned(), time::format)
        ),
    };
    let _ = writeln!(out, "range: {range}");
    let _ = writeln!(
        out,
        "eqv: input-equivalent tokens of {}",
        summary.reference_model
    );
}

fn unpriced_lines(summary: &TokensSummary, out: &mut String) {
    for model in &summary.unpriced {
        let _ = writeln!(
            out,
            "unpriced: {} messages of {} have no price at their time; their tokens are counted, \
             their cost and eqv are not",
            model.messages, model.model
        );
    }
}

/// A section of `tokens --by session`.
pub(crate) fn sessions(summary: &TokensSummary, rows: &[SessionRow], out: &mut String) {
    out.push_str(&header(&summary.identity));
    summary_lines(summary, out);
    let keys = [("session", Align::Left), ("agent", Align::Left)];
    let rows = rows.iter().map(|row| {
        let agent = row.agent.clone().unwrap_or_else(|| "-".to_owned());
        (vec![row.session.clone(), agent], &row.sums)
    });
    sums_table(&keys, rows, &summary.total).render(out);
    unpriced_lines(summary, out);
}

/// A section of `tokens --by repo`.
pub(crate) fn repos(summary: &TokensSummary, rows: &[RepoRow], out: &mut String) {
    out.push_str(&header(&summary.identity));
    summary_lines(summary, out);
    let keys = [("repo", Align::Left), ("path", Align::Left)];
    let rows = rows.iter().map(|row| {
        let cells = match &row.repo {
            Some(repo) => vec![
                repo.name
                    .clone()
                    .unwrap_or_else(|| "(not configured)".to_owned()),
                repo.path.clone(),
            ],
            None => vec!["(no repository)".to_owned(), String::new()],
        };
        (cells, &row.sums)
    });
    sums_table(&keys, rows, &summary.total).render(out);
    unpriced_lines(summary, out);
}

/// A section of `tokens --by model`.
pub(crate) fn models(summary: &TokensSummary, rows: &[ModelRow], out: &mut String) {
    out.push_str(&header(&summary.identity));
    summary_lines(summary, out);
    let keys = [("model", Align::Left)];
    let rows = rows.iter().map(|row| (vec![row.model.clone()], &row.sums));
    sums_table(&keys, rows, &summary.total).render(out);
    unpriced_lines(summary, out);
}

fn fields(pairs: Vec<(&'static str, String)>, out: &mut String) {
    let table = Table {
        columns: vec![("", Align::Left), ("", Align::Left)],
        rows: pairs
            .into_iter()
            .map(|(k, v)| vec![k.to_owned(), v])
            .collect(),
        total: None,
    };
    // Without the empty header line.
    let mut text = String::new();
    table.render(&mut text);
    out.push_str(text.split_once('\n').map_or("", |(_, rest)| rest));
}

/// The fields of a pass, under `keys`.
fn pass_fields(pass: &PassReport, keys: [&'static str; 4]) -> [(&'static str, String); 4] {
    let values = [
        pass.files.to_string(),
        pass.parsed_bytes.to_string(),
        pass.restarted.to_string(),
        pass.failed.len().to_string(),
    ];
    let mut values = values.into_iter();
    keys.map(|key| (key, values.next().unwrap_or_default()))
}

const PASS_KEYS: [&str; 4] = ["files", "parsed bytes", "restarted", "failed"];

fn failed_lines(pass: &PassReport, out: &mut String) {
    for file in &pass.failed {
        let _ = writeln!(out, "failed: {}: {}", file.path, file.error);
    }
}

/// A section of `ingest status`.
pub(crate) fn ingest_status(status: &IngestStatus, out: &mut String) {
    out.push_str(&header(&status.identity));
    let mut pairs = vec![
        ("database", status.database.clone()),
        ("transcripts", status.transcripts.clone()),
        ("files", status.files.to_string()),
        ("size bytes", status.size.to_string()),
        ("parsed bytes", status.parsed.to_string()),
        ("parsed since start", status.parsed_since_start.to_string()),
    ];
    let mut failed = None;
    match &status.last_pass {
        LastPass::Pending => pairs.push(("last pass", "pending".to_owned())),
        LastPass::Done(pass) => {
            pairs.push(("last pass", "done".to_owned()));
            pairs.extend(pass_fields(
                pass,
                ["  files", "  parsed bytes", "  restarted", "  failed"],
            ));
            failed = Some(pass);
        }
        LastPass::Failed { error } => pairs.push(("last pass", format!("failed: {error}"))),
    }
    fields(pairs, out);
    if let Some(pass) = failed {
        failed_lines(pass, out);
    }
}

/// A section of `reingest`.
pub(crate) fn reingest(result: &ReingestResult, out: &mut String) {
    out.push_str(&header(&result.identity));
    fields(pass_fields(&result.pass, PASS_KEYS).to_vec(), out);
    failed_lines(&result.pass, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_is_exact_and_rounded_half_up() {
        assert_eq!(usd(Picodollars(0)), "0.0000");
        assert_eq!(usd(Picodollars(49_999_999)), "0.0000");
        assert_eq!(usd(Picodollars(50_000_000)), "0.0001");
        assert_eq!(usd(Picodollars(1_234_567_890_000_000)), "1234.5679");
        assert_eq!(
            usd(Picodollars(u128::from(u64::MAX) * 1_000_000)),
            "18446744073709.5516"
        );
    }

    #[test]
    fn counts_are_in_full_and_columns_align() {
        let summary = TokensSummary {
            identity: "personal".to_owned(),
            from_ms: Some(0),
            to_ms: None,
            reference_model: "model-ref".to_owned(),
            total: TokenSums {
                messages: 2,
                output: 12_345_678_901,
                eqv: 1_000_000.4,
                cost: Picodollars(1_500_000_000_000),
                ..TokenSums::default()
            },
            unpriced: vec![],
        };
        let rows = vec![ModelRow {
            model: "m".to_owned(),
            sums: summary.total.clone(),
        }];
        let mut out = String::new();
        models(&summary, &rows, &mut out);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "== personal ==");
        assert_eq!(lines[1], "range: from 1970-01-01T00:00:00Z until now");
        assert!(lines[3].starts_with("model  messages"), "{out}");
        assert!(lines[4].contains("12345678901"), "{out}");
        assert!(lines[4].ends_with("1000000    1.5000"), "{out}");
        assert!(lines[6].starts_with("total"), "{out}");
        assert_eq!(lines[3].len(), lines[4].len(), "{out}");
    }
}
