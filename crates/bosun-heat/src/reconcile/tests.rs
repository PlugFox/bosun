//! `reconcile` over synthetic findings and stored rows built here, at a constant `now`: the
//! returned writes are inspected, nothing is stored.

use bosun_core::proposal::{ProposalState, SubjectKind, dedup_key, proposal_key};
use bosun_core::style_of;
use bosun_store::{NewProposal, ProposalRow, ProposalStep, Write};

use super::{Reconciled, reconcile};
use crate::detect::{
    Evidence, FILE_READ_ACROSS_RUNS, FileEvidence, Finding, UNCHANGED_FILE_READ_BYTES,
};

pub(crate) const REPO: &str = "/fixture/demo";
const PROVIDER: &str = "claude";
const DAY: i64 = 24 * 60 * 60 * 1000;
/// 2000-03-01T00:00:00Z.
pub(crate) const NOW: i64 = 951_868_800_000;
pub(crate) const WINDOW: i64 = 14 * DAY;

/// A D1 finding for `subject` over the 14 days before `NOW`.
pub(crate) fn finding(subject: &str, impact: Option<u64>) -> Finding {
    Finding {
        detector: FILE_READ_ACROSS_RUNS,
        repo: REPO.to_owned(),
        provider: PROVIDER.to_owned(),
        subject_kind: "file",
        subject: subject.to_owned(),
        value: 7,
        threshold: 5,
        target: 2,
        sample: 12,
        sample_unit: "session",
        window_from_ms: NOW - WINDOW,
        window_to_ms: NOW,
        impact_bytes: impact.map_or(0, |v| v * 4),
        impact_eqv_estimate: impact,
        evidence: Evidence::File(FileEvidence {
            reads: 7,
            result_bytes: impact.map_or(0, |v| v * 4),
            sessions: vec!["00000000-0000-4000-8000-000000000001".to_owned()],
            sessions_total: 7,
        }),
    }
}

fn key_of(f: &Finding) -> String {
    dedup_key(
        f.detector.id,
        &f.provider,
        SubjectKind::File,
        &f.subject,
        style_of(&f.repo),
    )
}

/// The row a finding's sighting stored at `created_ms`, then in `state` with `seen_count`.
pub(crate) fn row(
    f: &Finding,
    created_ms: i64,
    state: ProposalState,
    seen_count: u64,
) -> ProposalRow {
    let dedup = key_of(f);
    ProposalRow {
        id: 1,
        key: proposal_key(&dedup, created_ms),
        dedup_key: dedup,
        repo: f.repo.clone(),
        provider: f.provider.clone(),
        detector: f.detector.id.to_owned(),
        detector_version: f.detector.version,
        remedy: f.detector.remedy.to_owned(),
        subject_kind: SubjectKind::File,
        subject: f.subject.clone(),
        metric: f.detector.metric.to_owned(),
        unit: f.detector.unit.to_owned(),
        estimate: false,
        baseline_value: 7,
        threshold: 5,
        target_value: 2,
        window_from_ms: created_ms - WINDOW,
        window_to_ms: created_ms,
        sample: 12,
        guard_metric: None,
        guard_baseline: None,
        guard_after: None,
        impact_eqv_estimate: f.impact_eqv_estimate,
        params_json: "{}".to_owned(),
        evidence_json: "[]".to_owned(),
        state,
        created_ms,
        last_seen_ms: created_ms,
        seen_count,
        card_key: None,
        issue_number: None,
        published_ms: None,
        item: None,
        landed_sha: None,
        landed_ms: None,
        landed_how: None,
        followup_due_ms: None,
        after_value: None,
        after_sample: None,
        measured_ms: None,
        outcome: None,
        dismissed_ms: None,
        dismiss_reason: None,
    }
}

fn seen_keys(r: &Reconciled) -> Vec<&str> {
    r.writes
        .iter()
        .filter_map(|w| match w {
            Write::ProposalSeen { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect()
}

fn opened(r: &Reconciled) -> Vec<&NewProposal> {
    r.writes
        .iter()
        .filter_map(|w| match w {
            Write::ProposalOpened(new) => Some(new),
            _ => None,
        })
        .collect()
}

fn expired_keys(r: &Reconciled) -> Vec<&str> {
    r.writes
        .iter()
        .filter_map(|w| match w {
            Write::ProposalStep {
                key,
                step: ProposalStep::Expired,
                ..
            } => Some(key.as_str()),
            _ => None,
        })
        .collect()
}

/// #11 D: a finding is a proposal only at the second evaluation in a row: the first opens a
/// sighting, the second sees it again (count 2), and a sighting not seen again is forgotten
/// (`mutant_heat_propose_on_first_sight` promotes it in the first batch).
#[test]
fn a_finding_seen_once_is_not_yet_a_proposal() {
    let big = finding("src/big.rs", Some(100));
    let dedup = key_of(&big);

    let first = reconcile(std::slice::from_ref(&big), &[], NOW);
    assert_eq!(first.writes.len(), 1, "{:?}", first.writes);
    let [new] = opened(&first)[..] else {
        panic!("{:?}", first.writes)
    };
    // The key hashes the repository's path key with the dedup key.
    assert_eq!(new.key, proposal_key(&format!("{REPO}\n{dedup}"), NOW));
    assert_ne!(new.key, proposal_key(&dedup, NOW));
    assert_eq!(new.detector, "file_read_across_runs");
    assert_eq!(new.subject_kind, SubjectKind::File);
    assert_eq!(new.subject, "src/big.rs");
    assert_eq!(new.baseline_value, Some(7));
    assert_eq!(new.target_value, Some(2));
    assert_eq!(new.window, Some((NOW - WINDOW, NOW)));
    assert!(!new.estimate);
    assert_eq!(new.at_ms, NOW);
    let params: serde_json::Value = serde_json::from_str(&new.params_json).unwrap();
    assert_eq!(
        params,
        serde_json::json!({"bytes_per_token": 4, "impact_bytes": 400})
    );
    assert_eq!(first.summary.sightings, 1);
    assert_eq!(first.summary.promoted, 0);

    // The next evaluation, a day later, sees it again: the sighting becomes a proposal.
    let sighting = row(&big, NOW, ProposalState::Open, 1);
    let later = NOW + DAY;
    let second = reconcile(
        std::slice::from_ref(&big),
        std::slice::from_ref(&sighting),
        later,
    );
    assert_eq!(second.writes.len(), 1, "{:?}", second.writes);
    assert_eq!(seen_keys(&second), [sighting.key.as_str()]);
    assert_eq!(second.summary.promoted, 1);
    assert_eq!(second.summary.sightings, 0);

    // Not seen again: forgotten.
    let gone = reconcile(&[], std::slice::from_ref(&sighting), later);
    assert_eq!(gone.writes.len(), 1, "{:?}", gone.writes);
    assert_eq!(expired_keys(&gone), [sighting.key.as_str()]);
    assert_eq!(gone.summary.forgotten, 1);
}

/// #11 D: at most 3 promotions per evaluation and 10 active proposals per repository, the
/// largest estimated impact first (`None` last), then by key; a held-back candidate gets no
/// write (`mutant_heat_no_rate_limit` promotes every candidate).
#[test]
fn the_rate_limit_keeps_the_largest_findings() {
    let before = NOW - DAY;
    let five: Vec<Finding> = [50, 40, 30, 20, 10]
        .iter()
        .enumerate()
        .map(|(n, &impact)| finding(&format!("src/f{n}.rs"), Some(impact)))
        .collect();
    let sightings: Vec<ProposalRow> = five
        .iter()
        .map(|f| row(f, before, ProposalState::Open, 1))
        .collect();
    let r = reconcile(&five, &sightings, NOW);
    assert_eq!(r.writes.len(), 3, "{:?}", r.writes);
    let expect: Vec<&str> = sightings[..3].iter().map(|s| s.key.as_str()).collect();
    assert_eq!(seen_keys(&r), expect);
    assert_eq!(r.summary.promoted, 3);
    assert_eq!(r.summary.held_back, 2);

    // Eight active proposals (and more sightings, not counted): two slots under the cap of 10.
    let mut existing = sightings.clone();
    for n in 0..8 {
        let f = finding(&format!("src/active{n}.rs"), Some(1));
        let mut active = row(&f, before, ProposalState::Open, 2);
        active.last_seen_ms = before;
        existing.push(active);
    }
    let mut published = existing.pop().unwrap();
    published.state = ProposalState::Published;
    existing.push(published);
    let r = reconcile(&five, &existing, NOW);
    let expect: Vec<&str> = sightings[..2].iter().map(|s| s.key.as_str()).collect();
    assert_eq!(seen_keys(&r), expect, "{:?}", r.writes);
    assert_eq!(r.summary.held_back, 3);
    assert!(expired_keys(&r).is_empty(), "{:?}", r.writes);

    // Equal impact, one free slot (nine active): the smaller key wins; `None` comes last.
    let pair = [
        finding("src/x.rs", Some(30)),
        finding("src/y.rs", Some(30)),
        finding("src/z.rs", None),
    ];
    let rows: Vec<ProposalRow> = pair
        .iter()
        .map(|f| row(f, before, ProposalState::Open, 1))
        .collect();
    let mut existing = rows.clone();
    existing.extend((0..9).map(|n| {
        let f = finding(&format!("src/held{n}.rs"), Some(1));
        let mut landed = row(&f, before, ProposalState::Landed, 2);
        landed.last_seen_ms = before;
        landed
    }));
    let r = reconcile(&pair, &existing, NOW);
    let smaller = rows[..2].iter().map(|s| s.key.as_str()).min().unwrap();
    assert_eq!(seen_keys(&r), [smaller], "{:?}", r.writes);
    assert_eq!(r.summary.held_back, 2);

    // `None` after every `Some`, whatever the keys.
    let some = finding("src/some.rs", Some(1));
    let none = finding("src/none.rs", None);
    let rows = [
        row(&some, before, ProposalState::Open, 1),
        row(&none, before, ProposalState::Open, 1),
    ];
    let mut existing = rows.to_vec();
    existing.extend((0..9).map(|n| {
        let f = finding(&format!("src/held{n}.rs"), Some(1));
        row(&f, before, ProposalState::Landed, 2)
    }));
    let r = reconcile(&[none.clone(), some.clone()], &existing, NOW);
    assert_eq!(seen_keys(&r), [rows[0].key.as_str()], "{:?}", r.writes);
}

/// #11 D: "A detector does not fire twice for the same file while its card is open": an open
/// proposal is seen again, a landed one is left untouched, a dismissed key is in cooldown for
/// 90 days (`mutant_heat_reconcile_no_dedup` opens a second sighting beside the active row).
#[test]
fn a_detector_does_not_fire_twice_for_the_same_file_while_open() {
    let big = finding("src/big.rs", Some(100));
    let created = NOW - 20 * DAY;
    let mut open = row(&big, created, ProposalState::Open, 3);
    open.last_seen_ms = NOW - DAY;

    let r = reconcile(std::slice::from_ref(&big), std::slice::from_ref(&open), NOW);
    assert_eq!(r.writes.len(), 1, "{:?}", r.writes);
    assert_eq!(seen_keys(&r), [open.key.as_str()]);
    assert_eq!(opened(&r).len(), 0);
    assert_eq!(r.summary.seen, 1);

    let mut landed = open.clone();
    landed.state = ProposalState::Landed;
    let r = reconcile(std::slice::from_ref(&big), &[landed], NOW);
    assert!(r.writes.is_empty(), "{:?}", r.writes);

    let mut dismissed = open.clone();
    dismissed.state = ProposalState::Dismissed;
    dismissed.dismissed_ms = Some(NOW - 10 * DAY);
    let r = reconcile(
        std::slice::from_ref(&big),
        std::slice::from_ref(&dismissed),
        NOW,
    );
    assert!(r.writes.is_empty(), "{:?}", r.writes);
    assert_eq!(r.summary.cooled_down, 1);

    dismissed.dismissed_ms = Some(NOW - 100 * DAY);
    let r = reconcile(std::slice::from_ref(&big), &[dismissed], NOW);
    assert_eq!(r.writes.len(), 1, "{:?}", r.writes);
    assert_eq!(opened(&r).len(), 1);
    assert_eq!(r.summary.sightings, 1);

    // A finding of another detector on the same file is another key.
    let mut other = big.clone();
    other.detector = UNCHANGED_FILE_READ_BYTES;
    let r = reconcile(&[other], std::slice::from_ref(&open), NOW);
    assert_eq!(opened(&r).len(), 1, "{:?}", r.writes);
}

/// #11 D: an open proposal not seen for two windows expires; a published one never does
/// (`mutant_heat_expire_never` keeps it open).
#[test]
fn an_open_proposal_not_seen_for_two_windows_expires() {
    let big = finding("src/big.rs", Some(100));
    let mut open = row(&big, NOW - 40 * DAY, ProposalState::Open, 4);
    open.window_from_ms = open.created_ms - WINDOW;
    open.window_to_ms = open.created_ms;

    open.last_seen_ms = NOW - 29 * DAY;
    let r = reconcile(&[], std::slice::from_ref(&open), NOW);
    assert_eq!(r.writes.len(), 1, "{:?}", r.writes);
    assert_eq!(expired_keys(&r), [open.key.as_str()]);
    assert_eq!(r.summary.expired, 1);

    let mut published = open.clone();
    published.state = ProposalState::Published;
    let r = reconcile(&[], &[published], NOW);
    assert!(r.writes.is_empty(), "{:?}", r.writes);

    open.last_seen_ms = NOW - 27 * DAY;
    let r = reconcile(&[], &[open], NOW);
    assert!(r.writes.is_empty(), "{:?}", r.writes);
    assert_eq!(r.summary.expired, 0);
}

/// #11 D: the evidence is stored as redacted lines, capped at 4096 bytes with a `... N more`
/// line, always a JSON array of strings (`mutant_heat_evidence_unredacted` stores the token).
#[test]
fn evidence_is_redacted_and_capped_before_it_is_written() {
    let token = format!("ghp_{}", "a".repeat(36));
    // 40 session ids of 500 bytes each, the first one holding the token.
    let sessions: Vec<String> = (0..40)
        .map(|n| {
            if n == 0 {
                format!("{token}-{}", "0".repeat(500 - token.len() - 1))
            } else {
                format!("{n:0>500}")
            }
        })
        .collect();
    let mut big = finding("src/big.rs", Some(100));
    big.evidence = Evidence::File(FileEvidence {
        reads: 40,
        result_bytes: 4000,
        sessions,
        sessions_total: 40,
    });
    let r = reconcile(&[big], &[], NOW);
    let [new] = opened(&r)[..] else {
        panic!("{:?}", r.writes)
    };
    let json = &new.evidence_json;
    assert!(json.len() <= 4096, "{} bytes", json.len());
    assert!(json.contains("[redacted]"), "{json}");
    assert!(!json.contains(&token), "{json}");
    let lines: Vec<String> = serde_json::from_str(json).unwrap();
    // 41 lines: the numbers line (64 bytes), the redacted first session line ("session " and
    // "[redacted]" and the 460 bytes after the token: 478) and 39 session lines of 508. As a
    // compact JSON array each costs its length, 2 quotes and a comma: 66 + 480 = 546 for the
    // first two, 509 + 2 = 511 per further session line, 2 for the brackets less 1 comma, and
    // the "... N more" line (12 bytes, 14 quoted, plus a comma). Six further session lines:
    // 1 + 546 + 6 * 511 + 15 = 3628 (3629 with the closing bracket counted apart); a seventh
    // would bring 4140, over 4096. So 8 lines are kept and 41 - 8 = 33 are left out.
    assert_eq!(lines.len(), 9, "{lines:?}");
    assert_eq!(lines.last().unwrap(), "... 33 more");
    assert!(lines[1].starts_with("session [redacted]-"), "{}", lines[1]);
}
