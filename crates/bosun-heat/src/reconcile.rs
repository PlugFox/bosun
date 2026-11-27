//! The reconciliation of heat (#11, part D): one evaluation's [`Finding`]s of one repository
//! against the proposals stored for it, as the writes that bring the store up to date. Pure: no
//! I/O, no clock (`now_ms` is given).
//!
//! - A **sighting** is a row `open` with `seen_count = 1`, what [`Write::ProposalOpened`]
//!   stores; a **proposal** is a row with `seen_count >= 2`. A finding becomes a proposal only
//!   when two evaluations in a row see it: a sighting not seen again is forgotten (`expired`,
//!   never listed and never counted).
//! - A finding is matched with the rows by its dedup key ([`dedup_key`]); a subject never has
//!   two active rows. A finding of an `open` proposal or a `published` one is seen again; a
//!   `landed` one is left untouched (its frozen window is the follow-up's baseline).
//! - After a dismissal or a follow-up that did not move the metric the key is in cooldown for
//!   [`COOLDOWN_MS`]: its findings are dropped. Nothing is written that the store would refuse,
//!   since a refused write fails the whole batch.
//! - Promotions are rate-limited: at most [`MAX_NEW`] per evaluation and [`MAX_ACTIVE`] active
//!   proposals per repository, the largest estimated impact first, then by key. A candidate held
//!   back stays a sighting and is a candidate again at the next evaluation that sees it.
//! - An `open` proposal not seen for two window lengths expires; `published` and `landed` ones
//!   never do.
//! - Evidence is stored as redacted lines, a JSON array of at most [`EVIDENCE_CAP`] bytes.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::proposal::{
    COOLDOWN_MS, ProposalState, SubjectKind, dedup_key, proposal_key, starts_cooldown,
};
use bosun_core::publish::redact;
use bosun_core::style_of;
use bosun_store::{NewProposal, ProposalRow, ProposalStep, Write};

use crate::detect::Finding;
use crate::estimate::BYTES_PER_TOKEN;

/// The proposals one evaluation of a repository promotes at most.
pub const MAX_NEW: usize = 3;
/// The active proposals (`open` with `seen_count >= 2`, `published`, `landed`) of a repository
/// at most.
pub const MAX_ACTIVE: usize = 10;
/// The largest stored evidence array, in bytes.
pub const EVIDENCE_CAP: usize = 4096;

/// What one reconciliation did, in counts: the evaluation's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// The findings reconciled.
    pub findings: u64,
    /// New sightings opened.
    pub sightings: u64,
    /// Sightings seen again and promoted to proposals.
    pub promoted: u64,
    /// Sightings seen again the limits held back.
    pub held_back: u64,
    /// Proposals seen again.
    pub seen: u64,
    /// Sightings not seen again, expired.
    pub forgotten: u64,
    /// Open proposals not seen for two windows, expired.
    pub expired: u64,
    /// Findings dropped because their key is in cooldown.
    pub cooled_down: u64,
    /// Findings of a subject kind no proposal has, or a second finding of one key.
    pub skipped: u64,
}

/// The writes of one reconciliation and its summary.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reconciled {
    pub writes: Vec<Write>,
    pub summary: Summary,
}

/// A sighting seen again, waiting for a slot.
struct Candidate<'a> {
    key: &'a str,
    impact: Option<u64>,
}

fn is_sighting(row: &ProposalRow) -> bool {
    row.state == ProposalState::Open && row.seen_count < 2
}

/// The writes that reconcile `findings`, one evaluation of one repository, with `existing`, the
/// stored rows of that repository in every state, at `now_ms`. Deterministic for equal input.
#[must_use]
// Long: the rules in order, each a few lines.
#[allow(clippy::too_many_lines)]
pub fn reconcile(findings: &[Finding], existing: &[ProposalRow], now_ms: i64) -> Reconciled {
    let mut summary = Summary::default();
    let mut writes = Vec::new();

    let mut active: BTreeMap<&str, &ProposalRow> = BTreeMap::new();
    let mut cooldown: BTreeMap<&str, i64> = BTreeMap::new();
    for row in existing {
        if bosun_core::proposal::is_active(row.state) {
            active.insert(&row.dedup_key, row);
        }
        if starts_cooldown(row.state, row.outcome) {
            let ended = match row.state {
                ProposalState::Dismissed => row.dismissed_ms,
                _ => row.measured_ms,
            };
            if let Some(ended) = ended {
                let latest = cooldown.entry(&row.dedup_key).or_insert(ended);
                *latest = (*latest).max(ended);
            }
        }
    }

    let mut matched: BTreeSet<&str> = BTreeSet::new();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    for finding in findings {
        summary.findings += 1;
        let Some(kind) = SubjectKind::parse(finding.subject_kind) else {
            summary.skipped += 1;
            continue;
        };
        let key = dedup_key(
            finding.detector.id,
            &finding.provider,
            kind,
            &finding.subject,
            style_of(&finding.repo),
        );
        if !keys.insert(key.clone()) {
            summary.skipped += 1;
            continue;
        }
        if cooldown
            .get(key.as_str())
            .is_some_and(|ended| now_ms - ended < COOLDOWN_MS)
        {
            summary.cooled_down += 1;
            continue;
        }
        // Mutant (`mutant_heat_reconcile_no_dedup`): an active row of the same key is ignored
        // and a new sighting is opened.
        let row = if cfg!(mutant_heat_reconcile_no_dedup) {
            None
        } else {
            active.get(key.as_str()).copied()
        };
        if let Some(row) = row {
            matched.insert(&row.dedup_key);
            match row.state {
                ProposalState::Landed => {}
                _ if is_sighting(row) => candidates.push(Candidate {
                    key: &row.key,
                    impact: finding.impact_eqv_estimate,
                }),
                _ => {
                    summary.seen += 1;
                    writes.push(Write::ProposalSeen {
                        key: row.key.clone(),
                        at_ms: now_ms,
                    });
                }
            }
            continue;
        }
        summary.sightings += 1;
        let new = opened(finding, kind, &key, now_ms);
        // Mutant (`mutant_heat_propose_on_first_sight`): a new finding is opened and promoted
        // at once.
        let promote = cfg!(mutant_heat_propose_on_first_sight).then(|| new.key.clone());
        writes.push(Write::ProposalOpened(new));
        if let Some(key) = promote {
            writes.push(Write::ProposalSeen { key, at_ms: now_ms });
        }
    }

    let mut active_count = 0;
    for row in existing {
        if !bosun_core::proposal::is_active(row.state) || is_sighting(row) {
            continue;
        }
        let window = row.window_to_ms - row.window_from_ms;
        let stale = row.state == ProposalState::Open
            && !matched.contains(row.dedup_key.as_str())
            && now_ms - row.last_seen_ms > 2 * window;
        // Mutant (`mutant_heat_expire_never`): no proposal expires.
        if stale && !cfg!(mutant_heat_expire_never) {
            summary.expired += 1;
            writes.push(expired(row, now_ms));
        } else {
            active_count += 1;
        }
    }
    for row in existing {
        if is_sighting(row) && !matched.contains(row.dedup_key.as_str()) {
            summary.forgotten += 1;
            writes.push(expired(row, now_ms));
        }
    }

    // The largest estimated impact first (`None` last), then by key.
    candidates.sort_by(|a, b| {
        let impact = |c: &Candidate<'_>| c.impact.map_or((1, 0), |v| (0, u64::MAX - v));
        impact(a).cmp(&impact(b)).then_with(|| a.key.cmp(b.key))
    });
    // Mutant (`mutant_heat_no_rate_limit`): every candidate is promoted.
    let slots = if cfg!(mutant_heat_no_rate_limit) {
        candidates.len()
    } else {
        MAX_NEW.min(MAX_ACTIVE.saturating_sub(active_count))
    };
    for (n, candidate) in candidates.iter().enumerate() {
        if n < slots {
            summary.promoted += 1;
            writes.push(Write::ProposalSeen {
                key: candidate.key.to_owned(),
                at_ms: now_ms,
            });
        } else {
            summary.held_back += 1;
        }
    }

    Reconciled { writes, summary }
}

fn expired(row: &ProposalRow, now_ms: i64) -> Write {
    Write::ProposalStep {
        key: row.key.clone(),
        step: ProposalStep::Expired,
        at_ms: now_ms,
    }
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The sighting `finding` opens under `dedup`.
fn opened(finding: &Finding, kind: SubjectKind, dedup: &str, now_ms: i64) -> NewProposal {
    let params_json = match finding.impact_eqv_estimate {
        Some(_) => serde_json::json!({
            "bytes_per_token": BYTES_PER_TOKEN,
            "impact_bytes": finding.impact_bytes,
        })
        .to_string(),
        None => "{}".to_owned(),
    };
    NewProposal {
        key: proposal_key(dedup, now_ms),
        repo: finding.repo.clone(),
        provider: finding.provider.clone(),
        detector: finding.detector.id.to_owned(),
        detector_version: finding.detector.version,
        remedy: finding.detector.remedy.to_owned(),
        subject_kind: kind,
        subject: finding.subject.clone(),
        metric: finding.detector.metric.to_owned(),
        unit: finding.detector.unit.to_owned(),
        // Every trigger is exact; the impact estimate is its own labelled field.
        estimate: false,
        baseline_value: Some(signed(finding.value)),
        threshold: signed(finding.threshold),
        target_value: Some(signed(finding.target)),
        window: Some((finding.window_from_ms, finding.window_to_ms)),
        sample: finding.sample,
        guard: None,
        impact_eqv_estimate: finding.impact_eqv_estimate,
        params_json,
        evidence_json: evidence_json(&finding.evidence.lines()),
        at_ms: now_ms,
    }
}

/// `lines`, each redacted, as a JSON array of at most [`EVIDENCE_CAP`] bytes: while it is over,
/// its last line is dropped, and the dropped lines are counted in a last `... N more`.
fn evidence_json(lines: &[String]) -> String {
    // Mutant (`mutant_heat_evidence_unredacted`): the lines are written as the finding holds
    // them.
    let lines: Vec<String> = if cfg!(mutant_heat_evidence_unredacted) {
        lines.to_vec()
    } else {
        lines.iter().map(|l| redact(l)).collect()
    };
    let mut kept = lines.len();
    loop {
        let dropped = lines.len() - kept;
        let mut array: Vec<&str> = lines[..kept].iter().map(String::as_str).collect();
        let more = format!("... {dropped} more");
        if dropped > 0 {
            array.push(&more);
        }
        let json = serde_json::to_string(&array).unwrap_or_else(|_| "[]".to_owned());
        if json.len() <= EVIDENCE_CAP || kept == 0 {
            return json;
        }
        kept -= 1;
    }
}

#[cfg(test)]
mod tests;
