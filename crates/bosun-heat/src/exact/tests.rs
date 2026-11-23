//! The report over the synthetic corpus (#16) in a real store, ingested by `bosun-ingest`.
//!
//! The golden file `tests/golden/report.json` is the result of `report.exact` over every case
//! of `fixtures/claude`, with commits written at fixed times. Run with `BOSUN_BLESS=1` to
//! rewrite it after a deliberate change, and read the diff: every number in it is exact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bosun_api::{CommitScan, RepoRef, ScanState};
use bosun_core::Commit;
use bosun_ingest::{Ingest, Options as IngestOptions, claude};
use bosun_store::{Options, Store, Write};
use tempfile::TempDir;

use super::*;

/// Midnight UTC of 2000-01-`day`, the day of the fixture case numbered `day`.
pub(crate) fn day(day: i64) -> i64 {
    946_684_800_000 + (day - 1) * 86_400_000
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude")
}

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/report.json")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A store holding the transcripts of `cases` (every case when empty), ingested from copies
/// in one directory, with the repository roots those cases name.
pub(crate) struct Corpus {
    _dir: TempDir,
    pub(crate) store: Store,
}

impl Corpus {
    pub(crate) async fn new(cases: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let mut roots = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(fixtures())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| cases.is_empty() || cases.contains(&name.as_str()))
            .collect();
        names.sort();
        assert_ne!(names.len(), 0, "no case named {cases:?}");
        for name in &names {
            let case = fixtures().join(name);
            copy_dir(&case.join("projects"), &projects);
            let repos: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(case.join("repos.json")).unwrap())
                    .unwrap();
            for root in repos["roots"].as_array().unwrap() {
                let root = root.as_str().unwrap().to_owned();
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        let ingest = Ingest::new(claude::Parser, projects, &store, IngestOptions::default());
        ingest.register_repos(&roots).await.unwrap();
        let report = ingest.scan().await.unwrap();
        assert_eq!(report.failed, []);
        Self { _dir: dir, store }
    }

    pub(crate) async fn estimates(&self, scope: Scope<'_>) -> bosun_api::Estimates {
        crate::estimates(self.store.reader(), "personal", scope, name)
            .await
            .unwrap()
    }

    pub(crate) async fn report(&self, scope: Scope<'_>, scans: Vec<CommitScan>) -> Report {
        exact(self.store.reader(), "personal", scope, name, scans)
            .await
            .unwrap()
    }

    pub(crate) async fn close(self) {
        self.store.close().await.unwrap();
    }
}

/// The configured name of each repository key of the corpus.
pub(crate) fn name(key: &str) -> RepoRef {
    let name = match key {
        "/fixture/repo-a" => "a",
        r"c:\fixture\repo-a" => "win-a",
        r"c:\fixture\repo-b" => "win-b",
        other => panic!("an unexpected repository key {other:?}"),
    };
    RepoRef {
        name: Some(name.to_owned()),
        path: key.to_owned(),
    }
}

fn commit(n: u32, ts_ms: i64) -> Commit {
    Commit {
        sha: format!("{n:040x}"),
        ts_ms,
        author: "A U Thor <author@example.invalid>".to_owned(),
        lines_added: 1,
        lines_removed: 0,
        trailers: BTreeMap::new(),
    }
}

/// The report the golden file holds: every case, the six days 2000-01-03 to 2000-01-08, and
/// commits in two repositories in both periods.
async fn golden_report() -> Report {
    let corpus = Corpus::new(&[]).await;
    let commits = [
        // The previous period, 1999-12-28 to 2000-01-03.
        ("/fixture/repo-a", commit(1, day(1) + 3_600_000)),
        ("/fixture/repo-a", commit(2, day(2) + 3_600_000)),
        // Outside both periods.
        ("/fixture/repo-a", commit(3, day(20))),
        // The range.
        ("/fixture/repo-a", commit(4, day(3) + 3_600_000)),
        ("/fixture/repo-a", commit(5, day(8) + 3_600_000)),
        ("/fixture/repo-a", commit(6, day(8) + 7_200_000)),
        (r"C:\fixture\repo-a", commit(7, day(4) + 3_600_000)),
    ];
    corpus
        .store
        .writer()
        .write(
            commits
                .into_iter()
                .map(|(repo, commit)| Write::Commit {
                    repo: repo.to_owned(),
                    commit,
                })
                .collect(),
        )
        .await
        .unwrap();
    let scans = vec![
        CommitScan {
            repo: name("/fixture/repo-a"),
            state: ScanState::Done,
        },
        CommitScan {
            repo: name(r"c:\fixture\repo-a"),
            state: ScanState::Pending,
        },
        CommitScan {
            repo: name(r"c:\fixture\repo-b"),
            state: ScanState::Failed {
                error: "not a git repository".to_owned(),
            },
        },
    ];
    let scope = Scope {
        from_ms: day(3),
        to_ms: day(9),
        repo: None,
    };
    let report = corpus.report(scope, scans).await;
    corpus.close().await;
    report
}

#[tokio::test]
async fn the_report_matches_the_golden_file() {
    let report = golden_report().await;
    let mut text = serde_json::to_string_pretty(&report).unwrap();
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
        "the report differs from {}; rerun with BOSUN_BLESS=1 and review the diff:\n{text}",
        golden().display()
    );
}

/// The golden report's sections hold together: what the golden file says is checked by
/// identities between sections, not only by being what it was.
#[tokio::test]
async fn the_sections_partition_the_total() {
    let report = golden_report().await;
    let total = &report.total;
    let tokens = |s: &TokenSums| {
        [
            s.input,
            s.cache_write_5m,
            s.cache_write_1h,
            s.cache_read,
            s.output,
        ]
    };
    let categories: Vec<u64> = report.by_category.iter().map(|c| c.tokens).collect();
    assert_eq!(categories, tokens(total));
    let cost: u128 = report.by_category.iter().map(|c| c.cost.0).sum();
    assert_eq!(cost, total.cost.0, "the categories sum to the total cost");
    for rows in [
        report.by_model.iter().map(|r| &r.sums).collect::<Vec<_>>(),
        report.by_kind.iter().map(|r| &r.sums).collect(),
    ] {
        let messages: u64 = rows.iter().map(|s| s.messages).sum();
        let cost: u128 = rows.iter().map(|s| s.cost.0).sum();
        assert_eq!((messages, cost), (total.messages, total.cost.0));
    }
    let kinds: Vec<&str> = report.by_kind.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["interactive", "run"]);
    assert_eq!(report.by_kind[1].sums.messages, 0, "stage 0 spawns no run");

    // Every message of the range is in one row of the per-commit section.
    let messages: u64 = report
        .per_commit
        .rows
        .iter()
        .map(|r| r.trend.current.messages)
        .sum();
    assert_eq!(messages, total.messages);
    let a = &report.per_commit.rows[0];
    assert_eq!(a.repo.as_ref().unwrap().path, "/fixture/repo-a");
    assert_eq!((a.trend.current.commits, a.trend.previous.commits), (3, 2));
    assert!(a.trend.change.is_some());
    assert!(report.per_commit.rows.last().unwrap().repo.is_none());

    // The old method sees one block per session and far less than the real total.
    assert_eq!(report.old_method.sums.messages, report.old_method.sessions);
    assert!(report.old_method.sums.cost.0 < total.cost.0);

    // Every model of the corpus has a price; `<synthetic>` is free, not unpriced.
    assert_eq!(total.unpriced_messages, 0, "{:?}", report.unpriced);
    let synthetic = report
        .by_model
        .iter()
        .find(|r| r.model == "<synthetic>")
        .unwrap();
    assert_eq!((synthetic.sums.messages, synthetic.sums.cost.0), (1, 0));
}

/// #26, acceptance: the agreement compares main plus sub-agent sums with the last
/// `cost-state` record of the session. Mutant `mutant_report_main_only`.
#[tokio::test]
async fn the_agreement_counts_subagents() {
    let corpus = Corpus::new(&["subagent-cost-state"]).await;
    let scope = Scope {
        from_ms: day(3),
        to_ms: day(4),
        repo: None,
    };
    let report = corpus.report(scope, Vec::new()).await;
    corpus.close().await;

    let agreement = &report.health.agreement;
    assert_eq!(
        (agreement.sessions, agreement.within),
        (1, 1),
        "{agreement:?}"
    );
    let row = &agreement.rows[0];
    assert_eq!(row.session, "00000000-0000-4000-8000-003100000000");
    assert_eq!((row.windows, row.messages), (2, 4));
    // The last record (output 50 010 + 1 300), not the first (50 000 + 1 300); and main plus
    // sub-agent, not the main transcript alone (cache read 13 000 + 11 000).
    let provider = CheckTokens {
        input: 12,
        cache_write: 1620,
        cache_read: 24_000,
        output: 51_310,
        total: 12 + 1620 + 24_000 + 51_310,
    };
    assert_eq!(row.comparison.provider, provider);
    assert_eq!(
        row.comparison.transcripts, provider,
        "the transcripts disagree with cost-state"
    );
    assert_eq!(agreement.total, row.comparison);
}

/// A session with the provider's totals is compared only when it has a message in the range,
/// and then with its sums over the provider's window, whatever the range.
#[tokio::test]
async fn the_agreement_selects_sessions_by_the_range() {
    let corpus = Corpus::new(&["subagent-cost-state"]).await;
    let outside = Scope {
        from_ms: day(4),
        to_ms: day(5),
        repo: None,
    };
    let report = corpus.report(outside, Vec::new()).await;
    assert_eq!(report.health.agreement.sessions, 0);
    assert_eq!(report.total.messages, 0);
    // The last message of the session is at 00:00:08; a range holding only it still compares
    // every message of the provider's window, here the whole session.
    let last = Scope {
        from_ms: day(3) + 8_000,
        to_ms: day(3) + 9_000,
        repo: None,
    };
    let report = corpus.report(last, Vec::new()).await;
    corpus.close().await;
    assert_eq!(report.total.messages, 1);
    let agreement = &report.health.agreement;
    assert_eq!((agreement.sessions, agreement.within), (1, 1));
    assert_eq!(agreement.rows[0].messages, 4);
}

#[tokio::test]
async fn a_repository_scope_counts_its_messages_and_commits_only() {
    let corpus = Corpus::new(&["cwd-change"]).await;
    let key = r"c:\fixture\repo-b";
    let scope = Scope {
        from_ms: day(4),
        to_ms: day(5),
        repo: Some(key),
    };
    let report = corpus.report(scope, Vec::new()).await;
    let all = corpus
        .report(
            Scope {
                repo: None,
                ..scope
            },
            Vec::new(),
        )
        .await;
    corpus.close().await;
    assert_eq!(report.repo.as_ref().unwrap().path, key);
    assert!(report.total.messages > 0);
    assert!(report.total.messages < all.total.messages);
    let rows: Vec<Option<&str>> = report
        .per_commit
        .rows
        .iter()
        .map(|r| r.repo.as_ref().map(|r| r.path.as_str()))
        .collect();
    assert_eq!(rows, [Some(key)]);
    // Ingest health is the store's, whatever the scope.
    assert_eq!(report.health, all.health);
}

/// The context curve: the prompt sizes of the `report` case are measured counts, and the drop
/// from 8065 to 706 after the edit is its one compaction.
#[tokio::test]
async fn the_context_curve_counts_a_drop_as_a_compaction() {
    let corpus = Corpus::new(&["report"]).await;
    let scope = Scope {
        from_ms: day(8),
        to_ms: day(9),
        repo: None,
    };
    let report = corpus.report(scope, Vec::new()).await;
    corpus.close().await;

    let curve = &report.context;
    let rows: Vec<(&str, u64, u64, u64, u64)> = curve
        .rows
        .iter()
        .map(|r| {
            (
                r.session.as_str(),
                r.messages,
                r.start,
                r.peak,
                r.compactions,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("00000000-0000-4000-8000-008100000000", 6, 4321, 8065, 1),
            ("00000000-0000-4000-8000-008200000000", 2, 9087, 10_098, 0),
        ]
    );
    assert_eq!((curve.windows, curve.compactions), (2, 1));
}

/// #46, acceptance: a resumed session (`startTime` after its first messages) with a message
/// after the last `cost-state` line is compared over the provider's window only, and agrees;
/// the signed difference splits into the model no transcript holds and the rest. Mutant
/// `mutant_report_no_window`.
#[tokio::test]
async fn the_agreement_counts_the_provider_window_only() {
    let corpus = Corpus::new(&["resumed-cost-state"]).await;
    let scope = Scope {
        from_ms: day(11),
        to_ms: day(12),
        repo: None,
    };
    let report = corpus.report(scope, Vec::new()).await;
    corpus.close().await;

    // Every message is in the bill, inside the window or not.
    assert_eq!(report.total.messages, 6);
    let agreement = &report.health.agreement;
    assert_eq!(
        (
            agreement.sessions,
            agreement.within,
            agreement.exceeding,
            agreement.excluded
        ),
        (1, 1, 0, 0),
        "{agreement:?}"
    );
    let row = &agreement.rows[0];
    assert_eq!(row.session, "00000000-0000-4000-8000-011100000000");
    // `M2`, `M5` in the main transcript and `M3`, `M4` in the sub-agent; not `M1`, before the
    // resume, nor the message after the line.
    assert_eq!((row.windows, row.messages), (2, 4));
    let c = &row.comparison;
    assert_eq!(
        c.transcripts,
        CheckTokens {
            input: 15,
            cache_write: 1350,
            cache_read: 27_000,
            output: 1390,
            total: 29_755,
        }
    );
    assert_eq!(
        c.provider,
        CheckTokens {
            input: 115,
            cache_write: 1350,
            cache_read: 27_500,
            output: 1440,
            total: 30_405,
        }
    );
    assert_eq!(c.difference, -650);
    assert!(c.within && !c.exceeds);
    assert_eq!(
        c.undercount,
        Undercount {
            absent_models: 150,
            shared_models: 500,
        }
    );
    assert_eq!(agreement.total, *c);
    assert_eq!(
        agreement.absent_models,
        [AbsentModel {
            model: "claude-haiku-5-5".to_owned(),
            sessions: 1,
            total: 150,
        }]
    );
}

/// #46: a session with the provider's totals but no message inside its window is excluded and
/// counted, not compared.
#[test]
fn a_session_with_no_message_in_the_window_is_excluded() {
    let check = CheckRow {
        provider: "claude".to_owned(),
        session: "s".to_owned(),
        model: "m".to_owned(),
        input: 1,
        cache_write: 0,
        cache_read: 0,
        output: 1,
        cost_usd: None,
    };
    let outside = WindowSums {
        provider: "claude".to_owned(),
        session: "s".to_owned(),
        agent: None,
        model: "m".to_owned(),
        in_window: false,
        messages: 1,
        tokens: Tokens {
            input: 1,
            output: 1,
            ..Tokens::default()
        },
    };
    let ranged = BTreeSet::from([("claude", "s")]);
    let agreement = agreement(&ranged, &[check], &[outside]);
    assert_eq!((agreement.sessions, agreement.excluded), (0, 1));
    assert_eq!(agreement.rows, []);
}

/// The criterion is one-sided: a provider larger by more than the tolerance is not within it
/// but does not exceed; transcripts larger by more than the tolerance exceed it.
#[test]
fn only_the_transcripts_exceed() {
    let sums = |total| CheckTokens {
        input: total,
        total,
        ..CheckTokens::default()
    };
    let under = compare(sums(90), sums(100), 0);
    assert_eq!(
        (under.difference, under.within, under.exceeds),
        (-10, false, false)
    );
    assert_eq!(under.undercount.shared_models, 10);
    let over = compare(sums(106), sums(100), 0);
    assert_eq!(
        (over.difference, over.within, over.exceeds),
        (6, false, true)
    );
    assert_eq!(over.undercount.shared_models, -6);
    let edge = compare(sums(105), sums(100), 0);
    assert_eq!((edge.within, edge.exceeds), (true, false));
    assert_eq!(base_model("claude-opus-5-5[1m]"), "claude-opus-5-5");
    assert_eq!(base_model("claude-opus-5-5"), "claude-opus-5-5");
}
