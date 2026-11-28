//! The loop detectors over synthetic item histories built in memory (the builders of
//! `metrics::tests`), in `/fixture/demo` over a 14-day window, without a store and without a
//! message: the run sessions are 0, so the minimum sample is the closed items alone.

use bosun_core::{Outcome, RunResult};
use bosun_store::{DenialRow, DenialSource, ItemHistory, VerdictRow};

use crate::detect::{
    BUDGET_STOPS, DENIED_TARGET_ACROSS_ITEMS, Detection, Detector, Evidence,
    FLAKY_TEST_ACROSS_ATTEMPTS, FLAKY_TESTS_IN_ONE_MODULE, Finding, SLOW_VERDICT, Thresholds,
    detect,
};
use crate::exact::Scope;
use crate::metrics::tests::{T, closed, inputs, transition};

const DAY: i64 = 86_400_000;
const REPO: &str = "/fixture/demo";

fn scope() -> Scope<'static> {
    Scope {
        from_ms: T,
        to_ms: T + 14 * DAY,
        repo: Some(REPO),
    }
}

fn id(n: u32) -> String {
    format!("local:demo#{n}")
}

/// The items `local:demo#1` to `#count` of `claude`, each closed in the window.
fn items(count: u32) -> Vec<ItemHistory> {
    (1..=count)
        .map(|n| {
            let done = T + DAY + i64::from(n);
            closed(&id(n), REPO, "claude", Outcome::Pass, done, &[])
        })
        .collect()
}

/// The item `local:demo#n` of `items`.
fn item(items: &mut [ItemHistory], n: u32) -> &mut ItemHistory {
    let id = id(n);
    items.iter_mut().find(|h| h.item.item.id.0 == id).unwrap()
}

fn flaky(items: &mut [ItemHistory], n: u32, attempt: u32, tests: &[&str]) {
    let history = item(items, n);
    history
        .flaky_tests
        .extend(tests.iter().map(|t| (attempt, (*t).to_owned())));
}

fn deny(items: &mut [ItemHistory], n: u32, source: DenialSource, target: &str, times: u32) {
    let history = item(items, n);
    for _ in 0..times {
        let seq = u32::try_from(history.denials.len()).unwrap();
        history.denials.push(DenialRow {
            attempt: 1,
            spawn: 1,
            seq,
            source,
            tool: "Bash".to_owned(),
            target: Some(target.to_owned()),
            reason: None,
        });
    }
}

fn verdict(items: &mut [ItemHistory], n: u32, command: &str, duration_ms: u64, result: RunResult) {
    let history = item(items, n);
    let run = u32::try_from(history.verdicts.len()).unwrap() + 1;
    history.verdicts.push(VerdictRow {
        attempt: 1,
        run,
        command: command.to_owned(),
        started_ms: T + DAY,
        ended_ms: Some(T + DAY + i64::try_from(duration_ms).unwrap()),
        result: Some(result),
        exit_code: None,
        duration_ms: Some(duration_ms),
        verdict_line: None,
        tail: None,
    });
}

/// An arrow of `local:demo#n` to `needs_you` for its budget, in the window (it was resumed and
/// closed since).
fn budget_stop(items: &mut [ItemHistory], n: u32) {
    let id = id(n);
    let history = item(items, n);
    let seq = u32::try_from(history.transitions.len()).unwrap() + 1;
    history.transitions.push(transition(
        &id,
        seq,
        "running",
        "needs_you",
        Some("budget"),
        T + 2 * DAY,
    ));
}

fn run(histories: &[ItemHistory]) -> Detection {
    let table = bosun_core::default_table();
    let inputs = inputs(&table, &[], &[], histories, &[]);
    detect(&inputs, scope(), &Thresholds::default())
}

fn of(detection: &Detection, detector: Detector) -> Vec<&Finding> {
    detection
        .findings
        .iter()
        .filter(|f| f.detector == detector)
        .collect()
}

#[test]
fn the_same_test_flaky_in_two_attempts_names_the_test() {
    let mut scene = items(5);
    flaky(&mut scene, 1, 1, &["store::tests::shared_flag"]);
    flaky(&mut scene, 3, 2, &["store::tests::shared_flag"]);
    flaky(&mut scene, 2, 1, &["net::tests::other"]);
    let detection = run(&scene);
    let found = of(&detection, FLAKY_TEST_ACROSS_ATTEMPTS);
    assert_eq!(found.len(), 1, "{found:#?}");
    let finding = found[0];
    assert_eq!(finding.subject_kind, "test");
    assert_eq!(finding.subject, "store::tests::shared_flag");
    assert_eq!(finding.value, 2);
    assert_eq!((finding.threshold, finding.target), (2, 0));
    assert_eq!((finding.sample, finding.sample_unit), (5, "closed_item"));
    assert_eq!(finding.detector.remedy, "shared_test_state");
    assert_eq!(finding.impact_bytes, 0);
    assert_eq!(finding.impact_eqv_estimate, None);
    let lines = finding.evidence.lines().join("\n");
    assert!(lines.contains("local:demo#1 attempt 1"), "{lines}");
    assert!(lines.contains("local:demo#3 attempt 2"), "{lines}");
    assert!(
        !detection
            .findings
            .iter()
            .any(|f| f.subject.contains("other")),
        "{:#?}",
        detection.findings
    );

    // Four closed items and no run session: below the minimum sample.
    scene.retain(|h| h.item.item.id.0 != id(5));
    let detection = run(&scene);
    assert!(detection.findings.is_empty(), "{:#?}", detection.findings);
    let silent = detection
        .insufficient
        .iter()
        .find(|i| i.detector == FLAKY_TEST_ACROSS_ATTEMPTS.id)
        .expect("F1 is insufficient");
    assert_eq!((silent.closed_items, silent.run_sessions), (4, 0));
}

#[test]
fn three_flaky_tests_under_one_module_in_one_attempt_fire_once() {
    let three = ["world::tests::a", "world::tests::b", "world::tests::c"];
    let mut scene = items(5);
    flaky(&mut scene, 1, 1, &three);
    flaky(&mut scene, 2, 2, &three);
    let detection = run(&scene);
    let found = of(&detection, FLAKY_TESTS_IN_ONE_MODULE);
    assert_eq!(found.len(), 1, "{found:#?}");
    let finding = found[0];
    assert_eq!(finding.subject_kind, "test");
    assert_eq!(finding.subject, "world::tests::");
    assert_eq!(finding.value, 3);
    assert_eq!((finding.threshold, finding.target), (3, 1));
    assert_eq!(finding.detector.remedy, "shared_test_state");
    let Evidence::Tests(evidence) = &finding.evidence else {
        panic!("{:?}", finding.evidence);
    };
    assert_eq!(evidence.grouping, Some("module_prefix"));
    assert_eq!(evidence.attempts_total, 2);
    let lines = finding.evidence.lines();
    assert!(lines[0].starts_with("grouping: module_prefix"), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("world::tests::c")),
        "{lines:?}"
    );

    // Two tests under one prefix: nothing.
    let mut scene = items(5);
    flaky(&mut scene, 1, 1, &three[..2]);
    let detection = run(&scene);
    assert!(
        of(&detection, FLAKY_TESTS_IN_ONE_MODULE).is_empty(),
        "{:#?}",
        detection.findings
    );
}

#[test]
fn a_target_denied_across_items_fires_with_its_redacted_target() {
    let token = format!("ghp_{}", "a".repeat(36));
    let target = format!("deploy --token={token}");
    let mut scene = items(5);
    deny(&mut scene, 1, DenialSource::Permission, &target, 3);
    deny(&mut scene, 2, DenialSource::Permission, &target, 3);
    deny(&mut scene, 3, DenialSource::Hook, "rm -rf build", 3);
    deny(&mut scene, 4, DenialSource::Hook, "rm -rf build", 3);
    let detection = run(&scene);
    let found = of(&detection, DENIED_TARGET_ACROSS_ITEMS);
    assert_eq!(found.len(), 1, "{found:#?}");
    let finding = found[0];
    assert_eq!(finding.subject_kind, "tool_target");
    assert!(
        finding.subject.starts_with("Bash deploy"),
        "{}",
        finding.subject
    );
    assert!(
        finding.subject.contains("[redacted]"),
        "{}",
        finding.subject
    );
    assert!(!finding.subject.contains(&token), "{}", finding.subject);
    assert_eq!(finding.value, 6);
    assert_eq!((finding.threshold, finding.target), (5, 0));
    assert_eq!(finding.detector.remedy, "allow_or_script");
    let lines = finding.evidence.lines().join("\n");
    assert!(!lines.contains(&token), "{lines}");
    assert!(lines.contains("local:demo#1: 3 denials"), "{lines}");
    assert!(lines.contains("local:demo#2: 3 denials"), "{lines}");
    assert!(
        !format!("{:?}", detection.findings).contains(&token),
        "no raw target reaches a finding"
    );
    assert!(
        !detection.findings.iter().any(|f| f.subject.contains("rm")),
        "{:#?}",
        detection.findings
    );

    // The six permission denials on one item only: nothing.
    let mut scene = items(5);
    deny(&mut scene, 1, DenialSource::Permission, &target, 6);
    let detection = run(&scene);
    assert!(
        of(&detection, DENIED_TARGET_ACROSS_ITEMS).is_empty(),
        "{:#?}",
        detection.findings
    );
}

/// A target and a verdict command whose token a zero-width space splits (#138): `redact` alone
/// sees no token shape, so the subject must be scrubbed (cleaned first) to lose the token.
#[test]
fn a_refused_target_with_a_zero_width_character_is_scrubbed() {
    let body = "a".repeat(36);
    let split = format!("ghp_\u{200B}{body}");
    let target = format!("deploy {split}");
    let mut scene = items(5);
    deny(&mut scene, 1, DenialSource::Permission, &target, 3);
    deny(&mut scene, 2, DenialSource::Permission, &target, 3);
    let command = format!("deploy {split} && cargo test");
    verdict(&mut scene, 1, &command, 700_000, RunResult::Pass);
    verdict(&mut scene, 2, &command, 700_000, RunResult::Pass);
    let detection = run(&scene);
    for detector in [DENIED_TARGET_ACROSS_ITEMS, SLOW_VERDICT] {
        let found = of(&detection, detector);
        assert_eq!(found.len(), 1, "{found:#?}");
        let subject = &found[0].subject;
        assert!(subject.contains("deploy [redacted]"), "{subject}");
        assert!(!subject.contains(&body), "{subject}");
        assert!(!subject.contains('\u{200B}'), "{subject}");
    }
    assert!(
        !format!("{:?}", detection.findings).contains(&body),
        "no split token reaches a finding"
    );
}

#[test]
fn a_verdict_slower_than_the_threshold_fires() {
    let mut scene = items(5);
    verdict(&mut scene, 1, "cargo test -q", 700_000, RunResult::Pass);
    verdict(&mut scene, 2, "cargo test -q", 300_000, RunResult::Timeout);
    verdict(&mut scene, 3, "cargo test -q", 1_000, RunResult::Pass);
    verdict(&mut scene, 4, "make check", 2_000, RunResult::Pass);
    verdict(&mut scene, 5, "make check", 3_000, RunResult::Fail);
    budget_stop(&mut scene, 1);
    budget_stop(&mut scene, 4);
    let detection = run(&scene);
    let found = of(&detection, SLOW_VERDICT);
    assert_eq!(found.len(), 1, "{found:#?}");
    let finding = found[0];
    assert_eq!(finding.subject_kind, "command");
    assert_eq!(finding.subject, "cargo test -q");
    assert_eq!(finding.value, 2);
    assert_eq!((finding.threshold, finding.target), (2, 0));
    assert_eq!(finding.detector.remedy, "faster_verdict");
    let lines = finding.evidence.lines().join("\n");
    assert!(lines.contains("700000 ms, pass"), "{lines}");
    assert!(lines.contains("timeout"), "{lines}");

    let found = of(&detection, BUDGET_STOPS);
    assert_eq!(found.len(), 1, "{found:#?}");
    let finding = found[0];
    assert_eq!(finding.detector.metric, "budget_stops");
    assert_eq!(finding.detector.remedy, "faster_verdict");
    assert_eq!(
        (finding.subject_kind, finding.subject.as_str()),
        ("repo", ".")
    );
    assert_eq!(finding.value, 2);
    assert_eq!((finding.threshold, finding.target), (2, 0));

    // One slow run and one budget stop: nothing.
    let mut scene = items(5);
    verdict(&mut scene, 1, "cargo test -q", 700_000, RunResult::Pass);
    verdict(&mut scene, 2, "cargo test -q", 1_000, RunResult::Pass);
    budget_stop(&mut scene, 1);
    let detection = run(&scene);
    assert!(detection.findings.is_empty(), "{:#?}", detection.findings);
}
