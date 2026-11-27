//! The follow-up over synthetic run sessions and closed items built in memory (the builders of
//! `metrics::tests`), in `/fixture/demo` unless said otherwise, provider `claude`, a 14-day
//! baseline window `[day 0, day 14)`, without a store.

use bosun_core::proposal::{FollowupOutcome, ProposalState};
use bosun_core::{Outcome, TouchKind};
use bosun_store::{Guard, ItemHistory, ProposalRow, ProposalStep, ToolRow, UsageRow, Write};
use serde_json::Value;

use super::{DIR_READ_BYTES_PER_CLOSED_ITEM, DetectorStanding, measure, standings};
use crate::detect::{FILE_READ_ACROSS_RUNS, FILE_REREAD_IN_SESSION, Finding, Thresholds, detect};
use crate::exact::Scope;
use crate::metrics::Inputs;
use crate::metrics::tests::{T, call, closed, inputs, message, tokens};
use crate::reconcile::tests::row;

const DAY: i64 = 86_400_000;
const REPO: &str = "/fixture/demo";
const WINDOWS_REPO: &str = r"c:\fixture\demo";
/// The landing: day 15.
const LANDED: i64 = T + 15 * DAY;
/// The follow-up's due time: one window (14 days) after the landing, day 29.
const DUE: i64 = LANDED + 14 * DAY;
/// Two windows after the landing: day 43.
const LAST: i64 = LANDED + 2 * 14 * DAY;

/// Synthetic rows of one repository, one message per call, from `from_ms` on.
struct Rows {
    repo: &'static str,
    from_ms: i64,
    usage: Vec<UsageRow>,
    calls: Vec<ToolRow>,
    histories: Vec<ItemHistory>,
    next: i64,
}

impl Rows {
    fn new(repo: &'static str, from_ms: i64) -> Self {
        Self {
            repo,
            from_ms,
            usage: Vec::new(),
            calls: Vec::new(),
            histories: Vec::new(),
            next: 0,
        }
    }

    /// A message of the `claude` run session `session`; its id.
    fn run(&mut self, session: &str) -> String {
        self.next += 1;
        let row = message(
            "claude",
            session,
            None,
            "run",
            Some(self.repo),
            self.from_ms + self.next * 1000,
            tokens(10, 0, 0, 10),
        );
        let id = row.message.clone();
        self.usage.push(row);
        id
    }

    /// The run sessions `<prefix>00` .. `<prefix><n - 1>`.
    fn runs(&mut self, prefix: &str, n: usize) {
        for s in 0..n {
            self.run(&format!("{prefix}{s:02}"));
        }
    }

    /// A read of `path` of `bytes` in the run session `session`.
    fn read(&mut self, session: &str, path: &str, bytes: u64) {
        let id = self.run(session);
        self.calls.push(call(
            "claude",
            &id,
            "Read",
            bytes,
            false,
            &[(path, TouchKind::Read)],
        ));
    }

    /// `n` items of `claude` closed a day after `from_ms`.
    fn closed(&mut self, n: usize) {
        for _ in 0..n {
            let k = self.histories.len();
            let id = format!("item-{}-{k}", self.from_ms);
            let done = self.from_ms + DAY + i64::try_from(k).unwrap();
            self.histories
                .push(closed(&id, self.repo, "claude", Outcome::Pass, done, &[]));
        }
    }

    /// Every message of `model`.
    fn on_model(mut self, model: &str) -> Self {
        for row in &mut self.usage {
            model.clone_into(&mut row.model);
        }
        self
    }
}

/// Measures `proposal` at `now_ms` over the rows `before` and `after`.
fn followup(proposal: &ProposalRow, before: &Rows, after: &Rows, now_ms: i64) -> Option<Write> {
    let table = bosun_core::default_table();
    let b: Inputs<'_> = inputs(&table, &before.usage, &before.calls, &before.histories, &[]);
    let a: Inputs<'_> = inputs(&table, &after.usage, &after.calls, &after.histories, &[]);
    measure(proposal, &b, &a, now_ms)
}

/// The finding of `detector` on `subject` over the baseline window of `before`.
fn finding(before: &Rows, detector: &str, subject: &str) -> Finding {
    let table = bosun_core::default_table();
    let b = inputs(&table, &before.usage, &before.calls, &before.histories, &[]);
    let scope = Scope {
        from_ms: T,
        to_ms: T + 14 * DAY,
        repo: Some(before.repo),
    };
    detect(&b, scope, &Thresholds::default())
        .findings
        .into_iter()
        .find(|f| f.detector.id == detector && f.subject == subject)
        .unwrap_or_else(|| panic!("no {detector} finding on {subject}"))
}

/// The stored row of `f`, a proposal whose remedy landed on day 15, due on day 29.
fn landed(f: &Finding) -> ProposalRow {
    let mut row = row(f, f.window_to_ms, ProposalState::Landed, 2);
    row.baseline_value = i64::try_from(f.value).unwrap();
    row.threshold = i64::try_from(f.threshold).unwrap();
    row.target_value = i64::try_from(f.target).unwrap();
    row.window_from_ms = f.window_from_ms;
    row.window_to_ms = f.window_to_ms;
    row.sample = f.sample;
    row.params_json = r#"{"bytes_per_token":4,"impact_bytes":360000}"#.to_owned();
    row.landed_ms = Some(LANDED);
    row.followup_due_ms = Some(LANDED + (f.window_to_ms - f.window_from_ms));
    row
}

/// What a `Measured` step wrote.
struct Measured {
    after_value: Option<i64>,
    after_sample: u64,
    guard: Option<Guard>,
    params: Value,
    outcome: FollowupOutcome,
}

fn measured(write: Option<Write>) -> Measured {
    let Some(Write::ProposalStep {
        step:
            ProposalStep::Measured {
                after_value,
                after_sample,
                guard,
                params_json,
                outcome,
            },
        ..
    }) = write
    else {
        panic!("no Measured step: {write:?}");
    };
    Measured {
        after_value,
        after_sample,
        guard,
        params: params_json.map_or(Value::Null, |p| serde_json::from_str(&p).unwrap()),
        outcome,
    }
}

/// The guard a follow-up of a file proposal writes.
#[allow(clippy::unnecessary_wraps)] // Compared with the step's `Option<Guard>`.
fn guard(baseline: i64, after: i64) -> Option<Guard> {
    Some(Guard {
        metric: DIR_READ_BYTES_PER_CLOSED_ITEM.to_owned(),
        baseline,
        after,
    })
}

/// The baseline window of `repo`: 12 run sessions, 9 of which (`b00`..`b08`) read `big` of
/// 40 000 bytes, and 6 closed items.
fn before(repo: &'static str, big: &str) -> Rows {
    let mut rows = Rows::new(repo, T);
    rows.runs("b", 12);
    for s in 0..9 {
        rows.read(&format!("b{s:02}"), big, 40_000);
    }
    rows.closed(6);
    rows
}

/// #11 D's acceptance on synthetic history: D1 fires on a large document before its split, and
/// the follow-up, due one window after the landing, shows the metric reaching its target with
/// fewer bytes read in its directory; a recomputed before that differs from the snapshot is
/// drift, and a change of model is a confounder (`mutant_heat_followup_before_due` measures
/// at the landing).
#[test]
fn a_split_document_shows_the_metric_moving_after_its_landing() {
    let before = before(REPO, "docs/big.md");
    let f = finding(&before, FILE_READ_ACROSS_RUNS.id, "docs/big.md");
    assert_eq!(
        (f.value, f.threshold, f.target, f.sample),
        (9, 5, 2, 12),
        "{f:?}"
    );
    let proposal = landed(&f);
    assert_eq!(proposal.followup_due_ms, Some(DUE));

    // After: 12 run sessions, `a00` reads the old document, `a01`..`a11` its first part.
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 12);
    after.read("a00", "docs/big.md", 40_000);
    for s in 1..12 {
        after.read(&format!("a{s:02}"), "docs/big/part-1.md", 3_000);
    }
    after.closed(6);

    assert!(followup(&proposal, &before, &after, DUE - 1).is_none());
    let m = measured(followup(&proposal, &before, &after, DUE));
    // 1 session of 12 read it; scaled 1 * 12 / 12 = 1 <= target 2.
    assert_eq!(m.outcome, FollowupOutcome::Reached);
    assert_eq!(m.after_value, Some(1));
    assert_eq!(m.after_sample, 12);
    // Before: 9 * 40 000 / 6 = 60 000; after: (40 000 + 11 * 3 000) / 6 = 73 000 / 6 = 12 166.
    assert_eq!(m.guard, guard(60_000, 12_166));
    assert_eq!(m.params["after_scaled"], 1);
    assert_eq!(m.params.get("drift"), None, "recomputed before is 9");
    assert_eq!(m.params.get("confounder"), None);
    assert_eq!(m.params["bytes_per_token"], 4, "the row's object is kept");
    assert_eq!(m.params["impact_bytes"], 360_000);

    // A snapshot of 7: the code now counts 9, a drift of 9 - 7 = 2.
    let mut drifted = proposal.clone();
    drifted.baseline_value = 7;
    let m = measured(followup(&drifted, &before, &after, DUE));
    assert_eq!(m.params["drift"], 2);
    assert_eq!(m.outcome, FollowupOutcome::Reached);

    // The after sessions on another model: a confounder naming both.
    let after = after.on_model("claude-sonnet-5-5");
    let m = measured(followup(&proposal, &before, &after, DUE));
    assert_eq!(
        m.params["confounder"],
        serde_json::json!({
            "models_before": ["claude-opus-5-5"],
            "models_after": ["claude-sonnet-5-5"],
        })
    );
}

/// The file guard: a document renamed, not trimmed, drops its old path's count to zero while
/// its directory is read as much as before; that is not a move. The directory is split by the
/// repository's path style; a root file's directory is the repository
/// (`mutant_heat_followup_ignores_guard` takes the zero for `reached`).
#[test]
fn a_metric_that_fell_only_because_the_file_was_renamed_is_not_moved() {
    // Unix: `docs/big.md` renamed `docs/large.md`, read as often, as large.
    let before_rows = before(REPO, "docs/big.md");
    let proposal = landed(&finding(
        &before_rows,
        FILE_READ_ACROSS_RUNS.id,
        "docs/big.md",
    ));
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 12);
    for s in 0..9 {
        after.read(&format!("a{s:02}"), "docs/large.md", 40_000);
    }
    after.closed(6);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    assert_eq!(m.after_value, Some(0));
    // 9 * 40 000 / 6 = 60 000 before and after: the guard did not fall.
    assert_eq!(m.guard, guard(60_000, 60_000));
    assert_eq!(m.outcome, FollowupOutcome::NotMoved);

    // Windows: the same with `\`, and `src\main.rs` read for 3 * 200 000 = 600 000 bytes
    // before and 3 * 2 000 = 6 000 after, outside the guard's `docs\`.
    let mut before_rows = before(WINDOWS_REPO, r"docs\big.md");
    for s in 9..12 {
        before_rows.read(&format!("b{s:02}"), r"src\main.rs", 200_000);
    }
    let proposal = landed(&finding(
        &before_rows,
        FILE_READ_ACROSS_RUNS.id,
        r"docs\big.md",
    ));
    let mut after = Rows::new(WINDOWS_REPO, LANDED);
    after.runs("a", 12);
    for s in 0..9 {
        after.read(&format!("a{s:02}"), r"docs\large.md", 40_000);
    }
    for s in 9..12 {
        after.read(&format!("a{s:02}"), r"src\main.rs", 2_000);
    }
    after.closed(6);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    // Split on `/`, the guard would be the repository: (360 000 + 600 000) / 6 = 160 000
    // before, (360 000 + 6 000) / 6 = 61 000 after, a fall, and `reached`.
    assert_eq!(m.guard, guard(60_000, 60_000));
    assert_eq!(m.outcome, FollowupOutcome::NotMoved);

    // A root file: its guard is the whole repository.
    let mut before_rows = before(REPO, "readme.md");
    for s in 9..12 {
        before_rows.read(&format!("b{s:02}"), "docs/other.md", 60_000);
    }
    let proposal = landed(&finding(
        &before_rows,
        FILE_READ_ACROSS_RUNS.id,
        "readme.md",
    ));
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 12);
    after.read("a00", "readme.md", 40_000);
    for s in 9..12 {
        after.read(&format!("a{s:02}"), "docs/other.md", 60_000);
    }
    after.closed(6);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    // (9 * 40 000 + 3 * 60 000) / 6 = 540 000 / 6 = 90 000 before; (40 000 + 180 000) / 6 =
    // 220 000 / 6 = 36 666 after.
    assert_eq!(m.guard, guard(90_000, 36_666));
    assert_eq!(m.outcome, FollowupOutcome::Reached);
}

/// D1 counts sessions out of its sample, so its after value is compared at the baseline's
/// sample, whatever the after window's length and activity; D2's maximum is not scaled
/// (`mutant_heat_followup_unscaled` compares D1 unscaled).
#[test]
fn a_count_of_sessions_is_compared_at_the_baseline_sample() {
    let before_rows = before(REPO, "docs/big.md");
    let proposal = landed(&finding(
        &before_rows,
        FILE_READ_ACROSS_RUNS.id,
        "docs/big.md",
    ));

    // A busy after window: 36 run sessions, 9 read it, 18 closed items.
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 36);
    for s in 0..9 {
        after.read(&format!("a{s:02}"), "docs/big.md", 40_000);
    }
    after.closed(18);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    // 9 * 12 / 36 = 3 > target 2; (9 - 3) * 5 = 30 >= 9: moved.
    assert_eq!(m.outcome, FollowupOutcome::Moved);
    assert_eq!(m.after_value, Some(9), "as measured");
    assert_eq!(m.params["after_scaled"], 3);
    // 360 000 / 18 = 20 000 after.
    assert_eq!(m.guard, guard(60_000, 20_000));

    // A quiet after window: 4 run sessions, 3 read it (10 000 bytes), 6 closed items.
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 4);
    for s in 0..3 {
        after.read(&format!("a{s:02}"), "docs/big.md", 10_000);
    }
    after.closed(6);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    // 3 * 12 / 4 = 9: (9 - 9) * 5 < 9, not moved; the raw 3 would have moved.
    assert_eq!(m.params["after_scaled"], 9);
    assert_eq!(m.after_value, Some(3));
    // 30 000 / 6 = 5 000: the guard holds.
    assert_eq!(m.guard, guard(60_000, 5_000));
    assert_eq!(m.outcome, FollowupOutcome::NotMoved);

    // D2: 5 reads in one session of 12 before, 5 in one of 36 after: not scaled.
    let mut before_rows = Rows::new(REPO, T);
    before_rows.runs("b", 12);
    for _ in 0..5 {
        before_rows.read("b00", "docs/big.md", 1_000);
    }
    before_rows.closed(6);
    let proposal = landed(&finding(
        &before_rows,
        FILE_REREAD_IN_SESSION.id,
        "docs/big.md",
    ));
    assert_eq!((proposal.baseline_value, proposal.target_value), (5, 2));
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 36);
    for _ in 0..5 {
        after.read("a00", "docs/big.md", 100);
    }
    after.closed(18);
    let m = measured(followup(&proposal, &before_rows, &after, DUE));
    // Scaled, 5 * 12 / 36 = 1 would reach the target; as it is, 5 did not move.
    assert_eq!(m.after_value, Some(5));
    assert_eq!(m.params.get("after_scaled"), None);
    // 5 000 / 6 = 833 before, 500 / 18 = 27 after: the guard holds.
    assert_eq!(m.guard, guard(833, 27));
    assert_eq!(m.outcome, FollowupOutcome::NotMoved);
}

/// Below the minimum sample after the landing (or with no closed item for the guard) the
/// follow-up waits, and at two window lengths it is `insufficient`, never `not_moved`.
#[test]
fn too_few_sessions_after_the_landing_is_insufficient_not_moved() {
    let before_rows = before(REPO, "docs/big.md");
    let proposal = landed(&finding(
        &before_rows,
        FILE_READ_ACROSS_RUNS.id,
        "docs/big.md",
    ));

    // 3 run sessions and 1 closed item.
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 3);
    after.read("a00", "docs/big.md", 40_000);
    after.closed(1);
    assert!(followup(&proposal, &before_rows, &after, DUE).is_none());
    assert!(followup(&proposal, &before_rows, &after, LAST - 1).is_none());
    let m = measured(followup(&proposal, &before_rows, &after, LAST));
    assert_eq!(m.outcome, FollowupOutcome::Insufficient);
    assert_eq!(m.after_value, None);

    // 12 run sessions and no closed item: the guard cannot be computed.
    let mut after = Rows::new(REPO, LANDED);
    after.runs("a", 12);
    after.read("a00", "docs/big.md", 40_000);
    assert!(followup(&proposal, &before_rows, &after, DUE).is_none());
    let m = measured(followup(&proposal, &before_rows, &after, LAST));
    assert_eq!(m.outcome, FollowupOutcome::Insufficient);
    assert_eq!(m.after_value, None);
}

/// A detector whose measured proposals rarely moved, or which the user mostly dismissed, is a
/// candidate for removal; `insufficient` and `expired` count nowhere, sightings not at all
/// (`mutant_heat_flag_never` flags nothing).
#[test]
fn a_detector_with_one_moved_in_six_is_flagged() {
    use FollowupOutcome::{Insufficient, Moved, NotMoved, Reached};
    use ProposalState::{Dismissed, Expired, Measured, Open};
    let f = crate::reconcile::tests::finding("docs/big.md", None);
    let mut rows = Vec::new();
    let mut add = |detector: &str, state: ProposalState, outcome: Option<FollowupOutcome>, n| {
        for _ in 0..n {
            let mut r = row(&f, T, state, 2);
            detector.clone_into(&mut r.detector);
            r.outcome = outcome;
            rows.push(r);
        }
    };
    add("d1", Measured, Some(Moved), 1);
    add("d1", Measured, Some(NotMoved), 5);
    add("d1", Measured, Some(Insufficient), 2);
    add("d1", Expired, None, 3);
    add("d2", Measured, Some(Reached), 2);
    add("d2", Measured, Some(NotMoved), 3);
    add("d3", Measured, Some(NotMoved), 4);
    add("dismissed", Dismissed, None, 7);
    add("dismissed", Measured, Some(Reached), 2);
    add("dismissed", Measured, Some(NotMoved), 1);
    add("half", Dismissed, None, 6);
    add("half", Measured, Some(Moved), 4);
    let mut sighting = row(&f, T, Open, 1);
    "sighted".clone_into(&mut sighting.detector);
    rows.push(sighting.clone());
    "d1".clone_into(&mut sighting.detector);
    rows.push(sighting);

    let standing = |detector: &str, measured, improved, dismissed, flagged| DetectorStanding {
        detector: detector.to_owned(),
        measured,
        improved,
        dismissed,
        flagged,
    };
    assert_eq!(
        standings(&rows),
        vec![
            // 1 * 100 = 100 < 30 * 6 = 180.
            standing("d1", 6, 1, 0, Some("rarely_moves")),
            // 2 * 100 = 200 >= 30 * 5 = 150.
            standing("d2", 5, 2, 0, None),
            // Fewer than 5 measured.
            standing("d3", 4, 0, 0, None),
            // 7 * 100 = 700 >= 70 * (7 + 3) = 700.
            standing("dismissed", 3, 2, 7, Some("mostly_dismissed")),
            // 6 * 100 = 600 < 70 * (6 + 4) = 700.
            standing("half", 4, 4, 6, None),
        ]
    );
}
