//! The follow-up of heat's proposals (#11, part D, "Follow-up"): once a proposal's remedy has
//! landed and one baseline window has passed, its metric is measured again and the outcome
//! recorded; over the measured proposals, a detector whose proposals rarely move their metric
//! (or are mostly dismissed) is flagged as a candidate for removal ([`standings`]). Pure: no I/O,
//! no clock (`now_ms` is given).
//!
//! - **When.** Nothing before `followup_due_ms` (one window length `L` after the landing). From
//!   then the after window is `[landed_ms, min(now_ms, landed_ms + 2 L))`; the before window is
//!   the proposal's frozen `[window_from_ms, window_to_ms)`. While the after window's group (the
//!   repository and the proposal's provider) is below the detectors' minimum sample, the
//!   follow-up waits; at `landed_ms + 2 L` it records `insufficient`.
//! - **What.** Both values come from the detector's own function ([`value_of`]), for the
//!   proposal's provider and subject only: the before is recomputed by the current code over the
//!   frozen window, and a difference from the stored baseline is recorded as definition drift.
//! - **Normalization.** A count of sessions (D1) is compared at the before window's sample:
//!   `after * before_sample / after_sample` ([`scaled`]). D2's maximum and D3's bytes per closed
//!   item are compared as they are.
//! - **Outcome.** `reached` when the compared after is at or below the target; `moved` when it is
//!   at least 20% below the recomputed before; else `not_moved`. Every figure is exact;
//!   [`ProposalRow::impact_eqv_estimate`] plays no part.
//! - **Guard.** For a file subject, the read bytes per closed item of the subject's directory
//!   (recursively; the whole repository for a file at its root) must fall by at least 10% for
//!   `reached` or `moved` to stand: a split or renamed file's old path drops to zero by itself.
//!   For a directory subject (N1) the directory is the subject itself.
//!   The separator is that of the repository's path style.
//! - **Confounder.** The models of the provider's run sessions in each window, when they differ.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::proposal::{FollowupOutcome, ProposalState, SubjectKind};
use bosun_core::{PathStyle, style_of};
use bosun_store::{Guard, ProposalRow, ProposalStep, Write};
use serde_json::{Map, Value};

use crate::detect::{FILE_READ_ACROSS_RUNS, Measure, Thresholds, read_bytes, value_of};
use crate::exact::Scope;
use crate::metrics::Inputs;

/// The guard metric of the file detectors.
pub const DIR_READ_BYTES_PER_CLOSED_ITEM: &str = "dir_read_bytes_per_closed_item";

/// Whether the follow-up compares `detector_id`'s after value at the before window's sample.
///
/// By detector, not by the sample unit's name: D1's value is a count of the sessions of the
/// sample that read a file, so it grows with the number of sessions and is scaled; D2's sample
/// unit is `session` too, but its value is a maximum within one session, which does not grow
/// with their number, and D3's is already per closed item: neither is scaled.
#[must_use]
pub fn scaled(detector_id: &str) -> bool {
    detector_id == FILE_READ_ACROSS_RUNS.id
}

/// `value * before_sample / after_sample`, rounded down; 0 when `after_sample` is 0.
fn at_sample(value: u64, before_sample: u64, after_sample: u64) -> u64 {
    if after_sample == 0 {
        return 0;
    }
    let scaled = u128::from(value) * u128::from(before_sample) / u128::from(after_sample);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The directory prefix of the repository-relative path `subject` under `style`: the subject
/// up to and including its last separator; empty (the whole repository) for a file at the root.
fn directory(subject: &str, style: PathStyle) -> &str {
    let last = match style {
        PathStyle::Windows => subject.rfind('\\'),
        PathStyle::Unix => subject.rfind('/'),
    };
    last.map_or("", |at| &subject[..=at])
}

/// The prefix of the paths under the directory `subject` (N1's subject, `.` for the root)
/// under `style`: the subject and a separator; empty for the root.
fn dir_prefix(subject: &str, style: PathStyle) -> String {
    if subject == "." || subject.is_empty() {
        return String::new();
    }
    let separator = match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    };
    format!("{}{separator}", subject.trim_end_matches(separator))
}

/// Whether a subject of `kind` has the guard: a file's directory, or the directory itself.
fn guarded_kind(kind: SubjectKind) -> bool {
    matches!(kind, SubjectKind::File | SubjectKind::Dir)
}

/// The models of `provider`'s run messages of `scope`'s repository in its range, sorted.
fn models(inputs: &Inputs<'_>, scope: Scope<'_>, provider: &str) -> BTreeSet<String> {
    inputs
        .usage
        .iter()
        .filter(|row| {
            row.provider == provider
                && row.kind == "run"
                && row.ts_ms >= scope.from_ms
                && row.ts_ms < scope.to_ms
                && row.repo.is_some()
                && scope.holds_repo(row.repo.as_deref())
        })
        .map(|row| row.model.clone())
        .collect()
}

/// Whether a group's sample is below the detectors' minimum.
fn short(measure: &Measure) -> bool {
    let min = Thresholds::default();
    measure.closed_items < min.min_closed_items && measure.run_sessions < min.min_run_sessions
}

/// The follow-up of the `landed` proposal `proposal` at `now_ms`: its `Measured` step, or
/// `None` while it is not due or waits for a sample (and for any other state). `before` holds
/// the rows of the frozen window, `after` those of the after window (see the module
/// documentation); rows outside a window are ignored.
#[must_use]
// Long: the rules in order, each a few lines.
#[allow(clippy::too_many_lines)]
pub fn measure(
    proposal: &ProposalRow,
    before: &Inputs<'_>,
    after: &Inputs<'_>,
    now_ms: i64,
) -> Option<Write> {
    if proposal.state != ProposalState::Landed {
        return None;
    }
    let landed_ms = proposal.landed_ms?;
    let due_ms = proposal.followup_due_ms?;
    // Mutant (`mutant_heat_followup_before_due`): a landed proposal is measured at once.
    if !cfg!(mutant_heat_followup_before_due) && now_ms < due_ms {
        return None;
    }
    let length = proposal.window_to_ms - proposal.window_from_ms;
    let last_ms = landed_ms + 2 * length;
    let final_call = now_ms >= last_ms;
    let repo = proposal.repo.as_str();
    let before_scope = Scope {
        from_ms: proposal.window_from_ms,
        to_ms: proposal.window_to_ms,
        repo: Some(repo),
    };
    let after_scope = Scope {
        from_ms: landed_ms,
        to_ms: now_ms.min(last_ms),
        repo: Some(repo),
    };
    let detector = proposal.detector.as_str();
    let provider = proposal.provider.as_str();
    let subject = proposal.subject.as_str();
    let b = value_of(detector, before, before_scope, provider, subject)?;
    let a = value_of(detector, after, after_scope, provider, subject)?;

    // The guard: the read bytes per closed item of the subject's directory.
    let guard = if guarded_kind(proposal.subject_kind) {
        let dir = match proposal.subject_kind {
            SubjectKind::Dir => dir_prefix(subject, style_of(repo)),
            _ => directory(subject, style_of(repo)).to_owned(),
        };
        let under = |path: &str| path.starts_with(dir.as_str());
        let (bytes_before, items_before) = read_bytes(before, before_scope, provider, under);
        let (bytes_after, items_after) = read_bytes(after, after_scope, provider, under);
        if items_before == 0 || items_after == 0 {
            None
        } else {
            Some((bytes_before / items_before, bytes_after / items_after))
        }
    } else {
        Some((0, 0))
    };

    let Some((guard_baseline, guard_after)) = guard.filter(|_| !short(&a)) else {
        if !final_call {
            return None;
        }
        return Some(Write::ProposalStep {
            key: proposal.key.clone(),
            step: ProposalStep::Measured {
                after_value: None,
                after_sample: a.sample,
                guard: None,
                params_json: None,
                outcome: FollowupOutcome::Insufficient,
            },
            at_ms: now_ms,
        });
    };

    // Mutant (`mutant_heat_followup_unscaled`): D1's after value is compared as measured.
    let compared = if scaled(detector) && !cfg!(mutant_heat_followup_unscaled) {
        at_sample(a.value, b.sample, a.sample)
    } else {
        a.value
    };
    let before_value = b.value;
    let metric_outcome = if i128::from(compared) <= i128::from(proposal.target_value) {
        FollowupOutcome::Reached
    } else if u128::from(before_value.saturating_sub(compared)) * 5 >= u128::from(before_value)
        && compared < before_value
    {
        FollowupOutcome::Moved
    } else {
        FollowupOutcome::NotMoved
    };
    // A baseline of 0 bytes (or a subject without a guard) means no guard.
    let guarded = guarded_kind(proposal.subject_kind) && guard_baseline > 0;
    let holds = !guarded
        || u128::from(guard_baseline.saturating_sub(guard_after)) * 10
            >= u128::from(guard_baseline);
    // Mutant (`mutant_heat_followup_ignores_guard`): the guard is computed but not applied.
    let outcome = if holds || cfg!(mutant_heat_followup_ignores_guard) {
        metric_outcome
    } else {
        FollowupOutcome::NotMoved
    };

    let mut params: Map<String, Value> = match serde_json::from_str(&proposal.params_json) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    if scaled(detector) {
        params.insert("after_scaled".to_owned(), Value::from(compared));
    }
    let drift = i128::from(before_value) - i128::from(proposal.baseline_value);
    if drift != 0 {
        params.insert(
            "drift".to_owned(),
            Value::from(i64::try_from(drift).unwrap_or(i64::MAX)),
        );
    }
    let models_before = models(before, before_scope, provider);
    let models_after = models(after, after_scope, provider);
    if models_before != models_after {
        params.insert(
            "confounder".to_owned(),
            serde_json::json!({
                "models_before": models_before,
                "models_after": models_after,
            }),
        );
    }

    Some(Write::ProposalStep {
        key: proposal.key.clone(),
        step: ProposalStep::Measured {
            after_value: Some(signed(a.value)),
            after_sample: a.sample,
            guard: guarded.then(|| Guard {
                metric: DIR_READ_BYTES_PER_CLOSED_ITEM.to_owned(),
                baseline: signed(guard_baseline),
                after: signed(guard_after),
            }),
            params_json: Some(Value::Object(params).to_string()),
            outcome,
        },
        at_ms: now_ms,
    })
}

/// How one detector's proposals fared, computed at query time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorStanding {
    pub detector: String,
    /// Proposals measured with an outcome other than `insufficient`.
    pub measured: u64,
    /// Of those, the ones `reached` or `moved`.
    pub improved: u64,
    /// Proposals the user dismissed.
    pub dismissed: u64,
    /// `rarely_moves` or `mostly_dismissed`: a candidate for removal. Nothing is disabled.
    pub flagged: Option<&'static str>,
}

/// Whether a standing makes its detector a candidate for removal, and why: fewer than 30% of
/// at least 5 measured proposals improved (`rarely_moves`), or at least 70% of at least 5
/// dismissed or measured ones were dismissed (`mostly_dismissed`); the first wins.
fn flag(measured: u64, improved: u64, dismissed: u64) -> Option<&'static str> {
    // Mutant (`mutant_heat_flag_never`): no detector is flagged.
    if cfg!(mutant_heat_flag_never) {
        return None;
    }
    let decided = dismissed + measured;
    if measured >= 5 && improved * 100 < 30 * measured {
        Some("rarely_moves")
    } else if decided >= 5 && dismissed * 100 >= 70 * decided {
        Some("mostly_dismissed")
    } else {
        None
    }
}

/// The standing of each detector over `proposals`, by detector name. Sightings (`open`, seen
/// once) are left out; `expired` rows and `insufficient` follow-ups are counted nowhere.
#[must_use]
pub fn standings(proposals: &[ProposalRow]) -> Vec<DetectorStanding> {
    let mut counts: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
    for row in proposals {
        if row.state == ProposalState::Open && row.seen_count < 2 {
            continue;
        }
        let entry = counts.entry(&row.detector).or_default();
        match (row.state, row.outcome) {
            (ProposalState::Measured, Some(FollowupOutcome::Reached | FollowupOutcome::Moved)) => {
                entry.0 += 1;
                entry.1 += 1;
            }
            (ProposalState::Measured, Some(FollowupOutcome::NotMoved)) => entry.0 += 1,
            (ProposalState::Dismissed, _) => entry.2 += 1,
            _ => {}
        }
    }
    counts
        .into_iter()
        .map(
            |(detector, (measured, improved, dismissed))| DetectorStanding {
                detector: detector.to_owned(),
                measured,
                improved,
                dismissed,
                flagged: flag(measured, improved, dismissed),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests;
