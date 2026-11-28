//! The loop detectors, over the items' histories rather than the transcripts: F1 and F2 (#44 D,
//! flaky tests that share state), R1 (#210, a target the permission lists refuse across items),
//! V1 (#210, slow or timed-out verdict runs) and B1 (#210, items stopped by their budget).
//!
//! - An item belongs to the provider of its last session, as [`closed_items`] decides; an item
//!   without a session is skipped. Its flaky tests and denials count when it has an arrow in
//!   the window; a verdict run when it started in the window; a budget stop when its arrow to
//!   `needs_you` is in the window.
//! - Every value is an exact count, over the group's closed items: a loop finding has no bytes
//!   (`impact_bytes` 0, no estimate), so it ranks after every byte detector.
//! - What a subject names is scrubbed here ([`bosun_core::publish::scrub`]: invisible
//!   characters out, then redacted, #138; then cut to [`SUBJECT_CAP`] bytes): no raw refused
//!   target or verdict command reaches a finding, not even a token split by a zero-width
//!   character.
//!
//! [`closed_items`]: crate::metrics::closed_items

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::RunResult;
use bosun_core::publish::{redact, scrub};
use bosun_store::{DenialSource, ItemHistory};

use super::{
    BUDGET_STOPS, BudgetEvidence, Context, DENIED_TARGET_ACROSS_ITEMS, DenialEvidence, Detector,
    Evidence, FLAKY_TEST_ACROSS_ATTEMPTS, FLAKY_TESTS_IN_ONE_MODULE, Finding, MAX_SESSIONS,
    SLOW_VERDICT, SlowRun, TestAttempt, TestEvidence, VerdictEvidence,
};
use crate::exact::Scope;

/// The bytes a refused target or a verdict command keeps in a subject, after redaction.
pub(crate) const SUBJECT_CAP: usize = 200;

/// The grouping F2's evidence names: a module prefix, an approximation of a crate.
const MODULE_PREFIX: &str = "module_prefix";

/// One verdict run of a command.
#[derive(Debug, Clone, Copy)]
struct Verdict<'a> {
    item: &'a str,
    attempt: u32,
    run: u32,
    duration_ms: Option<u64>,
    result: Option<RunResult>,
}

/// What the loop detectors count of one group's items.
#[derive(Debug, Default)]
pub(crate) struct LoopRows<'a> {
    /// Per test name, the (item, attempt) pairs it was flaky in.
    flaky: BTreeMap<&'a str, BTreeSet<(&'a str, u32)>>,
    /// Per (item, attempt), its flaky tests.
    attempts: BTreeMap<(&'a str, u32), BTreeSet<&'a str>>,
    /// Per subject `<tool> <redacted target>`, the permission denials per item.
    denials: BTreeMap<String, BTreeMap<&'a str, u64>>,
    /// Per redacted verdict command, its runs in the window.
    verdicts: BTreeMap<String, Vec<Verdict<'a>>>,
    /// The items with an arrow to `needs_you` for their budget in the window.
    budget: BTreeSet<&'a str>,
}

/// `text` scrubbed (cleaned of invisible characters, then redacted, #138), then cut to
/// [`SUBJECT_CAP`] bytes at a char boundary.
fn redacted(text: &str) -> String {
    // Mutant (`mutant_heat_loop_subject_redact_only`): the subject is only redacted, so a token
    // split by a zero-width character keeps all but that character.
    let mut out = if cfg!(mutant_heat_loop_subject_redact_only) {
        redact(text)
    } else {
        scrub(text)
    };
    if out.len() > SUBJECT_CAP {
        let mut end = SUBJECT_CAP;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// The subject of a denial of `tool` on `target`: the tool and its redacted target, or the tool
/// alone when there is no target.
fn denial_subject(tool: &str, target: Option<&str>) -> String {
    let target = target.unwrap_or_default();
    // Mutant (`mutant_heat_denial_target_unredacted`): denials are grouped and named by the raw
    // target.
    let target = if cfg!(mutant_heat_denial_target_unredacted) {
        target.to_owned()
    } else {
        redacted(target)
    };
    let target = target.trim();
    if target.is_empty() {
        tool.to_owned()
    } else {
        format!("{tool} {target}")
    }
}

/// The stored name of a verdict run's result.
fn result_name(result: RunResult) -> &'static str {
    match result {
        RunResult::Pass => "pass",
        RunResult::Fail => "fail",
        RunResult::Timeout => "timeout",
        RunResult::Unready => "unready",
    }
}

/// The module prefixes of a test name, longest first: `a::b::t` has `a::b::` and `a::`.
fn prefixes(test: &str) -> impl Iterator<Item = &str> {
    test.rmatch_indices("::")
        .map(move |(at, _)| &test[..at + 2])
}

impl<'a> LoopRows<'a> {
    /// What the loop detectors count of `history` in `scope`; the item's repository and
    /// provider, or `None` for an item out of the scope or without a session.
    pub(crate) fn add(
        &mut self,
        history: &'a ItemHistory,
        scope: Scope<'_>,
    ) -> Option<(&'a str, &'a str)> {
        let repo = history.item.repo.as_str();
        if !scope.holds_repo(Some(repo)) {
            return None;
        }
        let provider = history.runs.last()?.provider.as_str();
        let in_range = |ts: i64| ts >= scope.from_ms && ts < scope.to_ms;
        if !history.transitions.iter().any(|t| in_range(t.ts_ms)) {
            return None;
        }
        let item = history.item.item.id.0.as_str();
        for (attempt, test) in &history.flaky_tests {
            self.flaky
                .entry(test.as_str())
                .or_default()
                .insert((item, *attempt));
            self.attempts
                .entry((item, *attempt))
                .or_default()
                .insert(test.as_str());
        }
        for denial in &history.denials {
            if denial.source != DenialSource::Permission {
                continue;
            }
            let subject = denial_subject(&denial.tool, denial.target.as_deref());
            *self
                .denials
                .entry(subject)
                .or_default()
                .entry(item)
                .or_default() += 1;
        }
        for verdict in history.verdicts.iter().filter(|v| in_range(v.started_ms)) {
            self.verdicts
                .entry(redacted(&verdict.command))
                .or_default()
                .push(Verdict {
                    item,
                    attempt: verdict.attempt,
                    run: verdict.run,
                    duration_ms: verdict.duration_ms,
                    result: verdict.result,
                });
        }
        let budget = history.transitions.iter().any(|t| {
            t.to == "needs_you" && t.to_detail.as_deref() == Some("budget") && in_range(t.ts_ms)
        });
        if budget {
            self.budget.insert(item);
        }
        Some((repo, provider))
    }

    /// Whether nothing was counted.
    pub(crate) fn is_empty(&self) -> bool {
        self.flaky.is_empty()
            && self.denials.is_empty()
            && self.verdicts.is_empty()
            && self.budget.is_empty()
    }

    /// `other`'s rows added to these.
    pub(crate) fn merge(&mut self, other: Self) {
        for (test, pairs) in other.flaky {
            self.flaky.entry(test).or_default().extend(pairs);
        }
        for (pair, tests) in other.attempts {
            self.attempts.entry(pair).or_default().extend(tests);
        }
        for (subject, items) in other.denials {
            let mine = self.denials.entry(subject).or_default();
            for (item, n) in items {
                *mine.entry(item).or_default() += n;
            }
        }
        for (command, runs) in other.verdicts {
            self.verdicts.entry(command).or_default().extend(runs);
        }
        self.budget.extend(other.budget);
    }

    /// The flaky tests of the attempt `pair` under `prefix`.
    fn under(&self, pair: &(&'a str, u32), prefix: &str) -> BTreeSet<&'a str> {
        self.attempts
            .get(pair)
            .into_iter()
            .flatten()
            .copied()
            .filter(|test| test.starts_with(prefix))
            .collect()
    }
}

/// Whether `detector` is one of this module's.
pub(crate) fn is_loop(detector: Detector) -> bool {
    [
        FLAKY_TEST_ACROSS_ATTEMPTS,
        FLAKY_TESTS_IN_ONE_MODULE,
        DENIED_TARGET_ACROSS_ITEMS,
        SLOW_VERDICT,
        BUDGET_STOPS,
    ]
    .contains(&detector)
}

/// The sample of every loop detector: the group's closed items.
pub(crate) fn sample(context: &Context<'_, '_>) -> (u64, &'static str) {
    (context.closed_items, "closed_item")
}

/// Whether a verdict run is slow: longer than the threshold, or timed out.
fn slow(run: &Verdict<'_>, context: &Context<'_, '_>) -> bool {
    run.result == Some(RunResult::Timeout)
        || run
            .duration_ms
            .is_some_and(|ms| ms > context.thresholds.v1_slow_ms)
}

/// The value of `detector` for `subject` in `context`'s group (0 for a subject it never saw):
/// F1 the attempts the test was flaky in, F2 the most flaky tests under the prefix in one
/// attempt, R1 the permission denials of the subject, V1 the slow runs of the command, B1 the
/// items stopped by their budget.
pub(crate) fn value(detector: Detector, context: &Context<'_, '_>, subject: &str) -> u64 {
    let rows = context.loops;
    match detector.id {
        id if id == FLAKY_TEST_ACROSS_ATTEMPTS.id => {
            rows.flaky.get(subject).map_or(0, |p| p.len() as u64)
        }
        id if id == FLAKY_TESTS_IN_ONE_MODULE.id => rows
            .attempts
            .keys()
            .map(|pair| rows.under(pair, subject).len() as u64)
            .max()
            .unwrap_or(0),
        id if id == DENIED_TARGET_ACROSS_ITEMS.id => {
            rows.denials.get(subject).map_or(0, |i| i.values().sum())
        }
        id if id == SLOW_VERDICT.id => rows.verdicts.get(subject).map_or(0, |runs| {
            runs.iter().filter(|r| slow(r, context)).count() as u64
        }),
        id if id == BUDGET_STOPS.id => rows.budget.len() as u64,
        _ => 0,
    }
}

/// The findings of `detector` in one group.
pub(crate) fn run(detector: Detector, context: &Context<'_, '_>) -> Vec<Finding> {
    match detector.id {
        id if id == FLAKY_TEST_ACROSS_ATTEMPTS.id => f1(context),
        id if id == FLAKY_TESTS_IN_ONE_MODULE.id => f2(context),
        id if id == DENIED_TARGET_ACROSS_ITEMS.id => r1(context),
        id if id == SLOW_VERDICT.id => v1(context),
        id if id == BUDGET_STOPS.id => b1(context),
        _ => Vec::new(),
    }
}

/// A finding of `detector` about `subject`: exact, without bytes.
fn finding(
    context: &Context<'_, '_>,
    detector: Detector,
    (subject_kind, subject): (&'static str, String),
    (value, threshold, target): (u64, u64, u64),
    evidence: Evidence,
) -> Finding {
    let (sample, sample_unit) = sample(context);
    Finding {
        detector,
        repo: context.repo.to_owned(),
        provider: context.provider.to_owned(),
        subject_kind,
        subject,
        value,
        threshold,
        target,
        sample,
        sample_unit,
        window_from_ms: context.scope.from_ms,
        window_to_ms: context.scope.to_ms,
        impact_bytes: 0,
        impact_eqv_estimate: None,
        evidence,
    }
}

/// The evidence of the attempts `attempts`, each with its tests.
fn tests_evidence<'s>(
    grouping: Option<&'static str>,
    attempts: impl Iterator<Item = (&'s str, u32, Vec<&'s str>)>,
) -> Evidence {
    let mut out = TestEvidence {
        grouping,
        attempts: Vec::new(),
        attempts_total: 0,
    };
    for (item, attempt, tests) in attempts {
        out.attempts_total += 1;
        if out.attempts.len() < MAX_SESSIONS {
            out.attempts.push(TestAttempt {
                item: item.to_owned(),
                attempt,
                tests: tests.into_iter().map(str::to_owned).collect(),
            });
        }
    }
    Evidence::Tests(out)
}

/// F1: per test, the distinct (item, attempt) pairs it was flaky in.
fn f1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_f1_threshold_inf`): F1 never fires.
    let threshold = if cfg!(mutant_heat_f1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.f1_attempts
    };
    let mut out = Vec::new();
    for (test, pairs) in &context.loops.flaky {
        let value = value(FLAKY_TEST_ACROSS_ATTEMPTS, context, test);
        if value >= threshold {
            let evidence =
                tests_evidence(None, pairs.iter().map(|(item, n)| (*item, *n, vec![*test])));
            out.push(finding(
                context,
                FLAKY_TEST_ACROSS_ATTEMPTS,
                ("test", (*test).to_owned()),
                (value, threshold, 0),
                evidence,
            ));
        }
    }
    out
}

/// F2: per attempt, each flaky test's longest module prefix that at least the threshold of the
/// attempt's flaky tests share; one finding per prefix, however many attempts show it.
fn f2(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_f2_threshold_inf`): F2 never fires.
    let threshold = if cfg!(mutant_heat_f2_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.f2_tests
    };
    let rows = context.loops;
    // Per prefix, the attempts that show it.
    let mut found: BTreeMap<&str, BTreeSet<(&str, u32)>> = BTreeMap::new();
    for (pair, tests) in &rows.attempts {
        let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
        for test in tests {
            for prefix in prefixes(test) {
                *counts.entry(prefix).or_default() += 1;
            }
        }
        for test in tests {
            if let Some(prefix) = prefixes(test).find(|p| counts[p] >= threshold) {
                found.entry(prefix).or_default().insert(*pair);
            }
        }
    }
    let mut out = Vec::new();
    for (prefix, pairs) in found {
        let value = value(FLAKY_TESTS_IN_ONE_MODULE, context, prefix);
        let evidence = tests_evidence(
            Some(MODULE_PREFIX),
            pairs.iter().map(|pair| {
                let tests = rows.under(pair, prefix).into_iter().collect();
                (pair.0, pair.1, tests)
            }),
        );
        out.push(finding(
            context,
            FLAKY_TESTS_IN_ONE_MODULE,
            ("test", prefix.to_owned()),
            (value, threshold, 1),
            evidence,
        ));
    }
    out
}

/// R1: per tool and redacted target, the permission denials, on enough items.
fn r1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_r1_threshold_inf`): R1 never fires.
    let threshold = if cfg!(mutant_heat_r1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.r1_denials
    };
    let mut out = Vec::new();
    for (subject, items) in &context.loops.denials {
        let value = value(DENIED_TARGET_ACROSS_ITEMS, context, subject);
        if value < threshold || (items.len() as u64) < context.thresholds.r1_items {
            continue;
        }
        let evidence = DenialEvidence {
            denials: value,
            items: items
                .iter()
                .take(MAX_SESSIONS)
                .map(|(item, n)| (redact(item), *n))
                .collect(),
            items_total: items.len() as u64,
        };
        out.push(finding(
            context,
            DENIED_TARGET_ACROSS_ITEMS,
            ("tool_target", subject.clone()),
            (value, threshold, 0),
            Evidence::Denials(evidence),
        ));
    }
    out
}

/// V1: per redacted verdict command, its runs slower than the threshold or timed out.
fn v1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_v1_threshold_inf`): V1 (and B1) never fire.
    let threshold = if cfg!(mutant_heat_v1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.v1_runs
    };
    let mut out = Vec::new();
    for (command, runs) in &context.loops.verdicts {
        let value = value(SLOW_VERDICT, context, command);
        if value < threshold {
            continue;
        }
        let slow: Vec<&Verdict<'_>> = runs.iter().filter(|r| slow(r, context)).collect();
        let evidence = VerdictEvidence {
            runs: slow
                .iter()
                .take(MAX_SESSIONS)
                .map(|r| SlowRun {
                    item: r.item.to_owned(),
                    attempt: r.attempt,
                    run: r.run,
                    duration_ms: r.duration_ms,
                    result: r.result.map(result_name),
                })
                .collect(),
            runs_total: slow.len() as u64,
        };
        out.push(finding(
            context,
            SLOW_VERDICT,
            ("command", command.clone()),
            (value, threshold, 0),
            Evidence::Verdicts(evidence),
        ));
    }
    out
}

/// B1: the items of the group stopped by their budget.
fn b1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_v1_threshold_inf`): B1 (and V1) never fire.
    let threshold = if cfg!(mutant_heat_v1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.b1_items
    };
    let value = value(BUDGET_STOPS, context, ".");
    if value < threshold {
        return Vec::new();
    }
    let items = &context.loops.budget;
    let evidence = BudgetEvidence {
        items: items
            .iter()
            .take(MAX_SESSIONS)
            .map(|i| (*i).to_owned())
            .collect(),
        items_total: items.len() as u64,
    };
    vec![finding(
        context,
        BUDGET_STOPS,
        ("repo", ".".to_owned()),
        (value, threshold, 0),
        Evidence::Budget(evidence),
    )]
}

#[cfg(test)]
mod tests;
