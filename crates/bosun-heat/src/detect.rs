//! The detectors of heat (#11, part D): deterministic rules over the rows the metrics are
//! computed from, each with a threshold and a named remedy class. Pure: no I/O, no store.
//!
//! - A group is one repository and one provider: two providers' sessions are never counted
//!   together, and a finding's before and after come from one provider.
//! - Only `run` sessions trigger; interactive ones are shown in the metrics and never count.
//! - Below the minimum sample (fewer than [`Thresholds::min_closed_items`] closed items and
//!   fewer than [`Thresholds::min_run_sessions`] run sessions) a detector answers
//!   [`Insufficient`] and stays silent.
//! - The trigger, the target and the evidence are exact (counts and bytes);
//!   [`Finding::impact_eqv_estimate`] is an estimate, for ranking and display only.
//!
//! The file detectors (D1, D2, D3) are in [`files`].

use std::collections::{BTreeMap, BTreeSet};

use bosun_store::UsageRow;

use crate::estimate::BYTES_PER_TOKEN;
use crate::exact::Scope;
use crate::metrics::{Inputs, closed_items, join};

mod files;

/// What a detector is, apart from its numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detector {
    /// The detector's stable name: part of a proposal's key.
    pub id: &'static str,
    /// Bumped when the rule changes meaning.
    pub version: u32,
    /// The class of remedy a finding proposes.
    pub remedy: &'static str,
    /// The metric a finding's value is in.
    pub metric: &'static str,
    /// The unit of the value, the threshold and the target.
    pub unit: &'static str,
}

/// D1: a file read in many run sessions of the window: give it an index or split it.
pub const FILE_READ_ACROSS_RUNS: Detector = Detector {
    id: "file_read_across_runs",
    version: 1,
    remedy: "index_or_split",
    metric: "file_read_sessions",
    unit: "sessions",
};

/// D2: a file re-read many times in one run session: it does not fit in the head, split it.
pub const FILE_REREAD_IN_SESSION: Detector = Detector {
    id: "file_reread_in_session",
    version: 1,
    remedy: "split",
    metric: "file_reads_in_one_session",
    unit: "reads",
};

/// D3 (#210): the bytes read of a file nobody wrote in the window, per closed item: point to
/// the part that matters or trim it.
pub const UNCHANGED_FILE_READ_BYTES: Detector = Detector {
    id: "unchanged_file_read_bytes",
    version: 1,
    remedy: "pointer_or_trim",
    metric: "unchanged_file_read_bytes_per_item",
    unit: "bytes_per_closed_item",
};

/// Every detector, in the order of their findings.
const DETECTORS: [Detector; 3] = [
    FILE_READ_ACROSS_RUNS,
    FILE_REREAD_IN_SESSION,
    UNCHANGED_FILE_READ_BYTES,
];

/// The thresholds of the detectors and the minimum sample. Thresholds from the configuration
/// are a later card's: for now the defaults and a per-detector switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thresholds {
    /// D1: the run sessions that read one file.
    pub d1_sessions: u64,
    /// D2: the reads of one file in one run session.
    pub d2_reads: u64,
    /// D3: the bytes of an unchanged file's single-file reads per closed item.
    pub d3_bytes_per_item: u64,
    /// The minimum sample: a group with fewer closed items than this and fewer run sessions
    /// than [`Self::min_run_sessions`] is insufficient.
    pub min_closed_items: u64,
    pub min_run_sessions: u64,
    /// The ids of the detectors switched off.
    pub disabled: Vec<&'static str>,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            d1_sessions: 5,
            d2_reads: 4,
            d3_bytes_per_item: 32 * 1024,
            min_closed_items: 5,
            min_run_sessions: 10,
            disabled: Vec::new(),
        }
    }
}

/// What a finding is about, per detector family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    File(FileEvidence),
}

/// The reads of one file a finding counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvidence {
    /// The reads of the file, single-file or not.
    pub reads: u64,
    /// The result bytes of its single-file reads (a call naming several files adds none).
    pub result_bytes: u64,
    /// The sessions, sorted, at most [`MAX_SESSIONS`].
    pub sessions: Vec<String>,
    /// Every session, listed or not.
    pub sessions_total: u64,
}

/// The sessions an evidence lists at most.
pub(crate) const MAX_SESSIONS: usize = 10;

impl Evidence {
    /// The evidence as short lines: the numbers first, then one line per session.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::File(file) => {
                let mut lines = vec![format!(
                    "{} reads, {} result bytes of single-file reads, in {} sessions",
                    file.reads, file.result_bytes, file.sessions_total
                )];
                lines.extend(file.sessions.iter().map(|s| format!("session {s}")));
                let more = file
                    .sessions_total
                    .saturating_sub(file.sessions.len() as u64);
                if more > 0 {
                    lines.push(format!("and {more} more sessions"));
                }
                lines
            }
        }
    }
}

/// A detector's threshold crossed in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub detector: Detector,
    /// The repository's path key.
    pub repo: String,
    pub provider: String,
    /// What the finding is about: `file` for the file detectors.
    pub subject_kind: &'static str,
    /// A repository-relative path for a file.
    pub subject: String,
    /// The metric, in the detector's unit: exact.
    pub value: u64,
    /// The threshold `value` reached.
    pub threshold: u64,
    /// The value the remedy should bring the metric to or below: half the threshold, at
    /// least 1.
    pub target: u64,
    /// The group's sample the value is over, in `sample_unit`.
    pub sample: u64,
    pub sample_unit: &'static str,
    pub window_from_ms: i64,
    pub window_to_ms: i64,
    /// The exact result bytes the finding is about (its evidence's bytes).
    pub impact_bytes: u64,
    /// An ESTIMATE: `impact_bytes` in tokens at [`BYTES_PER_TOKEN`], rounded up. For ranking
    /// and display under a header that says estimate only: never compared with a threshold
    /// and never added to an exact figure. `None` for a detector without bytes.
    pub impact_eqv_estimate: Option<u64>,
    pub evidence: Evidence,
}

/// A detector that stays silent in a group below the minimum sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insufficient {
    pub detector: &'static str,
    pub repo: String,
    pub provider: String,
    pub closed_items: u64,
    pub run_sessions: u64,
}

/// The findings and the silent detectors of one evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Detection {
    pub findings: Vec<Finding>,
    pub insufficient: Vec<Insufficient>,
}

/// One repository and provider.
#[derive(Debug, Default)]
struct Group<'a> {
    run_sessions: BTreeSet<&'a str>,
    closed_items: u64,
    files: files::FileReads<'a>,
}

/// What a detector sees of one group.
pub(crate) struct Context<'a, 'b> {
    pub(crate) repo: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) run_sessions: u64,
    pub(crate) closed_items: u64,
    pub(crate) files: &'b files::FileReads<'a>,
    /// The paths of the repository any call wrote in the window.
    pub(crate) written: &'b BTreeSet<&'a str>,
    pub(crate) scope: Scope<'b>,
    pub(crate) thresholds: &'b Thresholds,
}

impl Context<'_, '_> {
    /// A finding of `detector` about the file `path`.
    pub(crate) fn finding(
        &self,
        detector: Detector,
        path: &str,
        (value, threshold): (u64, u64),
        (sample, sample_unit): (u64, &'static str),
        evidence: FileEvidence,
    ) -> Finding {
        let impact_bytes = evidence.result_bytes;
        Finding {
            detector,
            repo: self.repo.to_owned(),
            provider: self.provider.to_owned(),
            subject_kind: "file",
            subject: path.to_owned(),
            value,
            threshold,
            target: (threshold / 2).max(1),
            sample,
            sample_unit,
            window_from_ms: self.scope.from_ms,
            window_to_ms: self.scope.to_ms,
            impact_bytes,
            impact_eqv_estimate: Some(impact_bytes.div_ceil(BYTES_PER_TOKEN)),
            evidence: Evidence::File(evidence),
        }
    }
}

/// The repository of a message in `scope`: in the range and in a repository the scope holds.
fn in_scope<'a>(row: &'a UsageRow, scope: Scope<'_>) -> Option<&'a str> {
    let repo = row.repo.as_deref()?;
    let in_range = row.ts_ms >= scope.from_ms && row.ts_ms < scope.to_ms;
    (in_range && scope.holds_repo(Some(repo))).then_some(repo)
}

/// The findings of every enabled detector over `inputs` in `scope`, per repository and
/// provider, and the detectors that stay silent below the minimum sample. Findings come in
/// detector order (D1, D2, D3), each detector's ranked (see [`files`]); deterministic for
/// equal input.
#[must_use]
pub fn detect(inputs: &Inputs<'_>, scope: Scope<'_>, thresholds: &Thresholds) -> Detection {
    let mut groups: BTreeMap<(&str, &str), Group<'_>> = BTreeMap::new();
    let mut written: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in inputs.usage {
        let Some(repo) = in_scope(row, scope) else {
            continue;
        };
        let group = groups.entry((repo, &row.provider)).or_default();
        if row.kind == "run" {
            group.run_sessions.insert(&row.session);
        }
    }
    for joined in join(inputs) {
        let Some(repo) = in_scope(joined.message, scope) else {
            continue;
        };
        written
            .entry(repo)
            .or_default()
            .extend(files::writes(joined.call));
        if joined.message.kind == "run" {
            let group = groups.entry((repo, &joined.message.provider)).or_default();
            group.files.add(&joined.message.session, joined.call);
        }
    }
    for closed in closed_items(inputs, scope) {
        groups
            .entry((closed.repo, closed.provider))
            .or_default()
            .closed_items += 1;
    }

    let none = BTreeSet::new();
    let mut detection = Detection::default();
    for ((repo, provider), group) in &groups {
        let context = Context {
            repo,
            provider,
            run_sessions: group.run_sessions.len() as u64,
            closed_items: group.closed_items,
            files: &group.files,
            written: written.get(repo).unwrap_or(&none),
            scope,
            thresholds,
        };
        // Mutant (`mutant_heat_no_minimum_sample`): every group is taken for a sufficient one.
        let sufficient = cfg!(mutant_heat_no_minimum_sample)
            || context.closed_items >= thresholds.min_closed_items
            || context.run_sessions >= thresholds.min_run_sessions;
        for detector in DETECTORS {
            if thresholds.disabled.contains(&detector.id) {
                continue;
            }
            let per_item = detector == UNCHANGED_FILE_READ_BYTES;
            if !sufficient || (per_item && context.closed_items == 0) {
                detection.insufficient.push(Insufficient {
                    detector: detector.id,
                    repo: (*repo).to_owned(),
                    provider: (*provider).to_owned(),
                    closed_items: context.closed_items,
                    run_sessions: context.run_sessions,
                });
                continue;
            }
            detection.findings.extend(files::run(detector, &context));
        }
    }
    let order = |f: &Finding| DETECTORS.iter().position(|d| *d == f.detector);
    detection.findings.sort_by(|a, b| {
        order(a)
            .cmp(&order(b))
            .then_with(|| files::rank(b).cmp(&files::rank(a)))
            .then_with(|| {
                (&a.subject, &a.repo, &a.provider).cmp(&(&b.subject, &b.repo, &b.provider))
            })
    });
    detection
}
