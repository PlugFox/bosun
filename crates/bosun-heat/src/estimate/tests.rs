//! The estimates over the synthetic corpus (#16) in a real store, ingested by `bosun-ingest`.
//!
//! The golden file `tests/golden/estimates.json` is the result of `report.estimates` over every
//! case of `fixtures/claude`, over the range of the exact golden report. Run with
//! `BOSUN_BLESS=1` to rewrite it after a deliberate change, and read the diff.
//!
//! The `report` case is built for these tests: in session 1 a search and two reads of
//! `src/lib.rs` come before an edit, a test run fails, and the prompt drops from 8065 to 706
//! after the edit; session 2 reads the same file again and writes another.

use std::path::{Path, PathBuf};

use bosun_api::{Estimates, ToolRow as ToolSummary};
use bosun_core::{Pricing, Rates};

use super::*;
use crate::exact::tests::{Corpus, day};

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/estimates.json")
}

/// The day of the `report` case.
fn report_day() -> Scope<'static> {
    Scope {
        from_ms: day(8),
        to_ms: day(9),
        repo: None,
    }
}

const SESSION_1: &str = "00000000-0000-4000-8000-008100000000";
const SESSION_2: &str = "00000000-0000-4000-8000-008200000000";
const MODEL: &str = "claude-opus-5-5";

fn tool<'a>(estimates: &'a Estimates, name: &str) -> &'a ToolSummary {
    estimates
        .by_tool
        .rows
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no tool {name}: {:?}", estimates.by_tool))
}

/// The rates of the case's model on its day.
async fn opus(corpus: &Corpus) -> Rates {
    let table = corpus.store.reader().price_table().await.unwrap();
    match table.select(MODEL, day(8), 0) {
        Pricing::Row(row) => row.rates,
        other => panic!("{MODEL} has no price: {other:?}"),
    }
}

#[tokio::test]
async fn the_estimates_match_the_golden_file() {
    let corpus = Corpus::new(&[]).await;
    let estimates = corpus
        .estimates(Scope {
            from_ms: day(3),
            to_ms: day(9),
            repo: None,
        })
        .await;
    corpus.close().await;
    let mut text = serde_json::to_string_pretty(&estimates).unwrap();
    text.push('\n');
    if std::env::var_os("BOSUN_BLESS").is_some() {
        std::fs::create_dir_all(golden().parent().unwrap()).unwrap();
        std::fs::write(golden(), &text).unwrap();
    }
    let expected = std::fs::read_to_string(golden())
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        text == expected,
        "the estimates differ from {}; rerun with BOSUN_BLESS=1 and review the diff:\n{text}",
        golden().display()
    );
}

/// #27, acceptance: every estimate section is marked as one, with its formula, and none is
/// empty over the corpus.
#[tokio::test]
async fn every_estimate_section_is_marked() {
    let corpus = Corpus::new(&[]).await;
    let estimates = corpus
        .estimates(Scope {
            from_ms: day(3),
            to_ms: day(9),
            repo: None,
        })
        .await;
    corpus.close().await;
    let json = serde_json::to_value(&estimates).unwrap();
    for section in [
        &json,
        &json["by_tool"],
        &json["by_file"],
        &json["repeated_reads"],
        &json["finding"],
    ] {
        assert_eq!(section["estimate"], true, "{section}");
        assert_ne!(section["formula"].as_str().unwrap(), "");
    }
    assert_eq!(estimates.bytes_per_token, BYTES_PER_TOKEN);
    assert!(
        estimates
            .known_gaps
            .iter()
            .any(|gap| gap.contains("base64"))
    );
    let rows = [
        estimates.by_tool.rows.len(),
        estimates.by_file.rows.len(),
        estimates.repeated_reads.rows.len(),
        estimates.finding.rows.len(),
    ];
    assert!(rows.iter().all(|n| *n > 0), "{rows:?}");
}

/// #27: a tool result costs its first write plus a cache read on every later message up to
/// the next compaction. Mutant `mutant_report_no_carry`.
#[tokio::test]
async fn a_tool_result_is_carried_until_the_compaction() {
    let corpus = Corpus::new(&["report"]).await;
    let estimates = corpus.estimates(report_day()).await;
    let rates = opus(&corpus).await;
    corpus.close().await;

    // The search of message 1: "src/lib.rs", 10 bytes, 3 tokens. Message 2 writes it (5 min:
    // it wrote no 1 h tokens); messages 3, 4 and 5 read it; message 6 is after the drop.
    let grep = &tool(&estimates, "Grep").results;
    assert_eq!((grep.calls, grep.result_bytes, grep.tokens), (1, 10, 3));
    assert_eq!(grep.carry_turns, 3, "{grep:?}");
    let expected = 3 * u128::from(rates.cache_write_5m) + 3 * 3 * u128::from(rates.cache_read);
    assert_eq!(grep.cost.0, expected, "{grep:?}");
    assert!(grep.carry_eqv > 0.0);

    // The failed test run of message 4: written by message 5, gone after the compaction.
    let bash = tool(&estimates, "Bash");
    assert_eq!((bash.errors, bash.results.carry_turns), (1, 0));
    assert!(bash.results.write_eqv > 0.0);
    assert!(bash.results.carry_eqv.abs() < f64::EPSILON);

    // The edit of message 5: the prompt drops at the next message, so nothing carried it; the
    // write of session 2 is its last message, so nothing did either.
    for name in ["Edit", "Write"] {
        let results = &tool(&estimates, name).results;
        assert_eq!((results.carry_turns, results.cost.0), (0, 0), "{name}");
        assert!(results.tokens > 0);
    }

    // The total is the rows' sum.
    let cost: u128 = estimates
        .by_tool
        .rows
        .iter()
        .map(|r| r.results.cost.0)
        .sum();
    assert_eq!(cost, estimates.by_tool.total.cost.0);
}

/// #27, acceptance: an exact total never includes an estimate. The exact report's total equals
/// the store's own bill of the same messages, while the estimates over them are not zero.
/// Mutant `mutant_report_merge_estimates`.
#[tokio::test]
async fn the_exact_total_holds_no_estimate() {
    let corpus = Corpus::new(&["report"]).await;
    let scope = report_day();
    let report = corpus.report(scope, Vec::new()).await;
    let estimates = corpus.estimates(scope).await;
    let bill = corpus
        .store
        .reader()
        .bill(scope.from_ms, scope.to_ms)
        .await
        .unwrap();
    corpus.close().await;

    assert!(estimates.by_tool.total.cost.0 > 0);
    assert_eq!(
        report.total.cost.0, bill.cost.picodollars,
        "the exact total differs from the bill of its messages"
    );
    let categories: u128 = report.by_category.iter().map(|c| c.cost.0).sum();
    assert_eq!(report.total.cost.0, categories);
}

#[tokio::test]
async fn repeated_reads_count_within_and_across_sessions() {
    let corpus = Corpus::new(&["report"]).await;
    let estimates = corpus.estimates(report_day()).await;
    corpus.close().await;

    let repeated = &estimates.repeated_reads;
    assert_eq!(repeated.rows.len(), 1, "{repeated:?}");
    let lib = &repeated.rows[0];
    assert_eq!(lib.path, "src/lib.rs");
    assert_eq!(lib.repo.as_ref().unwrap().path, "/fixture/repo-a");
    // Twice in session 1, once in session 2.
    assert_eq!((lib.reads, lib.sessions), (3, 2));
    assert_eq!((lib.within.calls, lib.across.calls), (1, 1));
    assert!(lib.within.eqv > 0.0 && lib.across.eqv > 0.0);
    assert_eq!(repeated.within, lib.within);
    assert_eq!(repeated.across, lib.across);

    let file = estimates
        .by_file
        .rows
        .iter()
        .find(|row| row.path == "src/lib.rs")
        .unwrap();
    assert_eq!(
        (file.reads, file.searches, file.writes, file.sessions),
        (3, 0, 1, 2)
    );
}

/// #76: the per-file table shows shell execs and unknown touches beside reads and writes, and
/// a script run twelve times (the `shell-touches` case) is no repeated read.
#[tokio::test]
async fn the_file_table_counts_execs_and_unknowns() {
    let corpus = Corpus::new(&["shell-touches"]).await;
    let estimates = corpus
        .estimates(Scope {
            from_ms: day(12),
            to_ms: day(13),
            repo: None,
        })
        .await;
    corpus.close().await;

    let file = |path: &str| {
        estimates
            .by_file
            .rows
            .iter()
            .find(|row| row.path == path)
            .unwrap_or_else(|| panic!("no file {path}: {:?}", estimates.by_file))
    };
    let counts = |row: &FileRow| (row.reads, row.searches, row.writes, row.execs, row.unknowns);
    assert_eq!(counts(file("tool/check.py")), (0, 0, 0, 12, 0));
    assert_eq!(counts(file("src/lib.rs")), (12, 0, 0, 0, 1));
    assert_eq!(estimates.by_file.rows.len(), 2, "{:?}", estimates.by_file);
    let repeated: Vec<(&str, u64)> = estimates
        .repeated_reads
        .rows
        .iter()
        .map(|row| (row.path.as_str(), row.reads))
        .collect();
    assert_eq!(repeated, [("src/lib.rs", 12)]);
}

#[tokio::test]
async fn the_cost_of_finding_stops_at_the_first_write() {
    let corpus = Corpus::new(&["report"]).await;
    let estimates = corpus.estimates(report_day()).await;
    corpus.close().await;

    let finding = &estimates.finding;
    assert_eq!(
        (finding.sessions_with_write, finding.sessions_without_write),
        (2, 0)
    );
    let sessions: Vec<(&str, u64)> = finding
        .rows
        .iter()
        .map(|r| (r.session.as_str(), r.results.calls))
        .collect();
    // Session 1: the search and both reads before the edit, not the test run; session 2: the
    // read before the write.
    assert_eq!(sessions, [(SESSION_1, 3), (SESSION_2, 1)]);
    assert_eq!(finding.rows[0].first_write_ms, Some(day(8) + 12_000));
    assert_eq!(finding.before_write.calls, 4);
}
