//! The loop's sections of the report (#72): items and attempts with their runs' exact cost,
//! retries, flaky verdicts, refused edits, time parked and the runs' commits; and, among the
//! estimates, the inferred sessions of items. The two costs of an item are printed in separate
//! sections and never summed (#3).

use std::fmt::Write as _;

use bosun_api::{InferredItems, Items, ParkedTime};

use super::{SUM_COLUMNS, Table, eqv, more, repo, sum_cells};
use crate::render::usd;
use crate::time;

/// A duration as hours, minutes and seconds.
fn span(ms: u64) -> String {
    let seconds = ms / 1000;
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn parked_text(rows: &[ParkedTime]) -> String {
    rows.iter()
        .map(|p| {
            let open = if p.ongoing { ", open" } else { "" };
            format!("{} {} ({}x{open})", p.reason, span(p.ms), p.parks)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The exact loop sections.
pub(crate) fn items(items: &Items, out: &mut String) {
    out.push_str("## Items\n\n");
    if items.rows.is_empty() {
        out.push_str("No item has a transition in the range.\n\n");
    } else {
        out.push_str(
            "Runs: the messages of the sessions bosun spawned for each item, sub-agents \
             included, over the item's whole history. Exact; sessions inferred for an item are \
             under the estimates and are not in these numbers.\n\n",
        );
        let mut table = Table::new(&[
            ("item", false),
            ("repository", false),
            ("state", false),
            ("attempts", true),
            ("failed", true),
            ("sessions", true),
            ("wall clock", true),
            ("messages", true),
            ("runs eqv", true),
            ("runs cost USD", true),
            ("parked", false),
        ]);
        for row in items.rows.iter().take(super::LIMIT) {
            let state = match &row.detail {
                Some(detail) => format!("{} ({detail})", row.state),
                None => row.state.clone(),
            };
            table.row(vec![
                row.item.clone(),
                repo(Some(&row.repo)),
                state,
                row.attempts.len().to_string(),
                row.failed.to_string(),
                row.sessions.to_string(),
                span(row.wall_clock_ms),
                row.runs.messages.to_string(),
                eqv(row.runs.eqv),
                usd(row.runs.cost),
                parked_text(&row.parked),
            ]);
        }
        table.render(out);
        more(items.rows.len(), out);
        attempts(items, out);
    }
    retried(items, out);
    flaky(items, out);
    refused(items, out);
    parked(items, out);
    commits(items, out);
}

fn attempts(items: &Items, out: &mut String) {
    out.push_str("### Attempts\n\n");
    let mut columns = vec![
        ("item", false),
        ("attempt", true),
        ("state", false),
        ("started", false),
        ("wall clock", true),
        ("sessions", false),
        ("models", false),
        ("refused calls", true),
    ];
    columns.extend(SUM_COLUMNS);
    let mut table = Table::new(&columns);
    let rows: Vec<_> = items
        .rows
        .iter()
        .flat_map(|row| row.attempts.iter().map(move |a| (row, a)))
        .collect();
    for (row, attempt) in rows.iter().take(super::LIMIT) {
        let sessions = attempt
            .sessions
            .iter()
            .map(|s| {
                let exit = s.exit.as_deref().unwrap_or("running");
                format!("{} {} {} ({exit})", s.spawn, s.mode, s.session)
            })
            .collect::<Vec<_>>()
            .join("; ");
        let models = attempt
            .models
            .iter()
            .map(|m| m.model.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let mut cells = vec![
            row.item.clone(),
            attempt.number.to_string(),
            attempt.state.clone(),
            time::format(attempt.started_ms),
            attempt.wall_clock_ms.map_or_else(String::new, span),
            sessions,
            models,
            attempt.refused_calls.to_string(),
        ];
        cells.extend(sum_cells(&attempt.sums));
        table.row(cells);
    }
    table.render(out);
    more(rows.len(), out);
}

fn retried(items: &Items, out: &mut String) {
    out.push_str("### Retried items\n\n");
    if items.retried.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let mut table = Table::new(&[("item", false), ("attempts", true), ("failed", true)]);
    for row in &items.retried {
        table.row(vec![
            row.item.clone(),
            row.attempts.to_string(),
            row.failed.to_string(),
        ]);
    }
    table.render(out);
}

fn flaky(items: &Items, out: &mut String) {
    out.push_str("### Flaky verdicts\n\n");
    if items.flaky.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let mut table = Table::new(&[("item", false), ("attempt", true), ("failing tests", false)]);
    for row in &items.flaky {
        let tests = if row.tests.is_empty() {
            "(none found)".to_owned()
        } else {
            row.tests.join(", ")
        };
        table.row(vec![row.item.clone(), row.attempt.to_string(), tests]);
    }
    table.render(out);
}

fn refused(items: &Items, out: &mut String) {
    out.push_str("### Refused protected-path edits\n\n");
    if items.refused_edits.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let mut table = Table::new(&[("item", false), ("attempt", true), ("at", false)]);
    for row in &items.refused_edits {
        table.row(vec![
            row.item.clone(),
            row.attempt.to_string(),
            time::format(row.ts_ms),
        ]);
    }
    table.render(out);
}

fn parked(items: &Items, out: &mut String) {
    out.push_str("### Time parked\n\n");
    if items.parked.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let mut table = Table::new(&[
        ("reason", false),
        ("parks", true),
        ("time", true),
        ("still parked", false),
    ]);
    for row in &items.parked {
        table.row(vec![
            row.reason.clone(),
            row.parks.to_string(),
            span(row.ms),
            if row.ongoing { "yes" } else { "no" }.to_owned(),
        ]);
    }
    table.render(out);
}

fn commits(items: &Items, out: &mut String) {
    out.push_str("### Commits of the runs\n\n");
    let _ = writeln!(
        out,
        "Each commit is linked to its session and item by the session's commit range: exact. \
         Commits of the range made outside the runs: {}.\n",
        items.outside_runs
    );
    if !items.commits.is_empty() {
        let mark = |present: bool| if present { "yes" } else { "no" }.to_owned();
        let mut table = Table::new(&[
            ("commit", false),
            ("repository", false),
            ("at", false),
            ("item", false),
            ("attempt", true),
            ("session", false),
            ("Item:", false),
            ("Run:", false),
            ("Session:", false),
        ]);
        for row in items.commits.iter().take(super::LIMIT) {
            table.row(vec![
                short(&row.sha),
                repo(Some(&row.repo)),
                time::format(row.ts_ms),
                row.item.clone(),
                row.attempt.to_string(),
                row.session.clone(),
                mark(row.trailers.item),
                mark(row.trailers.run),
                mark(row.trailers.session),
            ]);
        }
        table.render(out);
        more(items.commits.len(), out);
    }
    if !items.trailer_missing.is_empty() {
        out.push_str("Trailers missing:\n\n");
        for row in &items.trailer_missing {
            let _ = writeln!(
                out,
                "- {} ({}, {}): {}",
                short(&row.sha),
                repo(Some(&row.repo)),
                row.item,
                row.missing.join(", ")
            );
        }
        out.push('\n');
    }
}

/// A duration in seconds with one decimal: lease waits are often shorter than a second.
fn seconds(ms: u64) -> String {
    format!("{}.{} s", ms / 1000, ms % 1000 / 100)
}

/// A size on disk in human units: bytes below 1 KiB, else KiB, MiB or GiB with one decimal.
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    #[expect(clippy::cast_precision_loss, reason = "a size shown to one decimal")]
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The waits for leased resources in the range (#8): the measured cost of contention.
/// Omitted when no request was granted in the range.
pub(crate) fn lease_waits(items: &Items, out: &mut String) {
    if items.lease_waits.is_empty() || cfg!(mutant_report_drop_lease_waits) {
        // Mutant (`mutant_report_drop_lease_waits`): the section is never printed.
        return;
    }
    out.push_str("## Lease waits\n\n");
    out.push_str(
        "Per resource, the requests filed in the range and granted, and how long they waited \
         from their filing to their grant; longest wait first. Exact.\n\n",
    );
    let mut table = Table::new(&[
        ("resource", false),
        ("requests", true),
        ("total wait", true),
        ("longest wait", true),
    ]);
    for row in &items.lease_waits {
        table.row(vec![
            row.resource.clone(),
            row.requests.to_string(),
            seconds(row.total_ms),
            seconds(row.longest_ms),
        ]);
    }
    table.render(out);
}

/// The worktrees on disk or quarantined now, with their sizes when last measured (#8).
/// Omitted when there is none.
pub(crate) fn worktrees(items: &Items, out: &mut String) {
    if items.worktrees.is_empty() {
        return;
    }
    out.push_str("## Worktree disk use\n\n");
    out.push_str(
        "The worktrees on disk now, whatever the range, with their sizes when last measured; \
         a quarantined one is kept because a file blocked its removal.\n\n",
    );
    let mut table = Table::new(&[
        ("item", false),
        ("kind", false),
        ("state", false),
        ("size", true),
        ("path", false),
    ]);
    for row in items.worktrees.iter().take(super::LIMIT) {
        let state = match &row.blocking {
            Some(file) => format!("{} (blocked by {file})", row.state),
            None => row.state.clone(),
        };
        table.row(vec![
            row.item.clone(),
            row.kind.clone(),
            state,
            row.bytes.map_or_else(|| "not measured".to_owned(), size),
            row.path.clone(),
        ]);
    }
    let total: u64 = items.worktrees.iter().filter_map(|w| w.bytes).sum();
    let count = items.worktrees.len();
    table.row(vec![
        format!("total of {count}"),
        String::new(),
        String::new(),
        size(total),
        String::new(),
    ]);
    table.render(out);
    more(count, out);
}

/// The inferred sessions of items, among the estimates.
pub(crate) fn inferred(items: &InferredItems, out: &mut String) {
    out.push_str("### Items: inferred sessions (estimate)\n\n");
    let _ = writeln!(
        out,
        "Interactive sessions attributed to an item by inference: {}. Their tokens are exact; \
         the attribution is not, and these numbers are never added to an item's runs.\n",
        items.rule
    );
    if items.rows.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    let mut table = Table::new(&[
        ("item", false),
        ("repository", false),
        ("session", false),
        ("evidence", false),
        ("messages", true),
        ("eqv", true),
        ("cost USD", true),
    ]);
    for row in &items.rows {
        for session in &row.sessions {
            let evidence = session
                .evidence
                .iter()
                .map(|e| match e {
                    bosun_api::Evidence::CommitTrailer { sha } => {
                        format!("commit {} names it", short(sha))
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            table.row(vec![
                row.item.clone(),
                repo(Some(&row.repo)),
                session.session.clone(),
                evidence,
                session.sums.messages.to_string(),
                eqv(session.sums.eqv),
                usd(session.sums.cost),
            ]);
        }
    }
    table.render(out);
}
