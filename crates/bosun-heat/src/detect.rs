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
//! The file detectors (D1, D2, D3) are in [`files`]; the loop detectors (F1, F2, R1, V1, B1:
//! flaky tests, refused calls, slow verdicts, budget stops) in [`loops`]; the context detectors
//! (C1 the start context, N1 the cost of finding by directory, U1 the documents nobody opened)
//! in [`context`].

use std::collections::{BTreeMap, BTreeSet};

use bosun_store::UsageRow;

use crate::estimate::BYTES_PER_TOKEN;
use crate::exact::Scope;
use crate::metrics::{Inputs, closed_items, join};

pub(crate) mod context;
mod files;
mod loops;

pub use context::{ANY_PROVIDER, INSTRUCTION_FILES};

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

/// F1 (#44 D): one test flaky in several attempts: the tests share state.
pub const FLAKY_TEST_ACROSS_ATTEMPTS: Detector = Detector {
    id: "flaky_test_across_attempts",
    version: 1,
    remedy: "shared_test_state",
    metric: "flaky_attempts",
    unit: "attempts",
};

/// F2 (#44 D): several flaky tests under one module prefix in one attempt (an approximation of
/// one crate): the tests share state.
pub const FLAKY_TESTS_IN_ONE_MODULE: Detector = Detector {
    id: "flaky_tests_in_one_module",
    version: 1,
    remedy: "shared_test_state",
    metric: "flaky_tests_in_one_attempt",
    unit: "tests",
};

/// R1 (#210): one tool and target refused by the permission lists across items: allow it or
/// give it a script.
pub const DENIED_TARGET_ACROSS_ITEMS: Detector = Detector {
    id: "denied_target_across_items",
    version: 1,
    remedy: "allow_or_script",
    metric: "denials",
    unit: "denials",
};

/// V1 (#210): verdict runs of one command slower than a threshold or timed out.
pub const SLOW_VERDICT: Detector = Detector {
    id: "slow_verdict",
    version: 1,
    remedy: "faster_verdict",
    metric: "slow_verdict_runs",
    unit: "runs",
};

/// B1 (#210): items stopped by their budget cap (a detector of its own, for its own metric).
pub const BUDGET_STOPS: Detector = Detector {
    id: "budget_stops",
    version: 1,
    remedy: "faster_verdict",
    metric: "budget_stops",
    unit: "items",
};

/// C1 (#11 D): the median start context of run sessions above a band: the standing
/// instructions are too heavy.
pub const START_CONTEXT: Detector = Detector {
    id: "start_context",
    version: 1,
    remedy: "trim_standing_instructions",
    metric: "context_at_start",
    unit: "tokens",
};

/// N1 (#11 D): the cost of finding concentrated in one directory: it lacks a map.
pub const FINDING_BY_DIRECTORY: Detector = Detector {
    id: "finding_by_directory",
    version: 1,
    remedy: "add_map",
    metric: "finding_bytes_share_of_directory",
    unit: "ppm",
};

/// U1 (#210): tracked documents no session opened in the window: prune them or link them.
pub const UNOPENED_DOCUMENTS: Detector = Detector {
    id: "unopened_documents",
    version: 1,
    remedy: "prune_or_link",
    metric: "unopened_documents",
    unit: "documents",
};

/// Every detector, in the order of their findings: the byte detectors first.
const DETECTORS: [Detector; 11] = [
    FILE_READ_ACROSS_RUNS,
    FILE_REREAD_IN_SESSION,
    UNCHANGED_FILE_READ_BYTES,
    FLAKY_TEST_ACROSS_ATTEMPTS,
    FLAKY_TESTS_IN_ONE_MODULE,
    DENIED_TARGET_ACROSS_ITEMS,
    SLOW_VERDICT,
    BUDGET_STOPS,
    FINDING_BY_DIRECTORY,
    START_CONTEXT,
    UNOPENED_DOCUMENTS,
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
    /// F1: the distinct (item, attempt) pairs one test was flaky in.
    pub f1_attempts: u64,
    /// F2: the flaky tests of one attempt under one module prefix.
    pub f2_tests: u64,
    /// R1: the permission denials of one tool and target, ...
    pub r1_denials: u64,
    /// ... on at least this many items.
    pub r1_items: u64,
    /// V1: a verdict run longer than this is slow.
    pub v1_slow_ms: u64,
    /// V1: the slow or timed-out runs of one verdict command.
    pub v1_runs: u64,
    /// B1: the items stopped by their budget cap.
    pub b1_items: u64,
    /// C1: the band of the median start context, in tokens (it fires above it), ...
    pub c1_tokens: u64,
    /// ... and its target.
    pub c1_target: u64,
    /// N1: the share of one directory in the finding bytes, in ppm, ...
    pub n1_share_ppm: u64,
    /// ... and its target.
    pub n1_target_ppm: u64,
    /// U1: the unopened documents, ...
    pub u1_documents: u64,
    /// ... and its target.
    pub u1_target: u64,
    /// U1: the globs of the documents, each `*.<suffix>` (see [`Thresholds::check`]).
    pub documents: Vec<String>,
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
            f1_attempts: 2,
            f2_tests: 3,
            r1_denials: 5,
            r1_items: 2,
            v1_slow_ms: 600_000,
            v1_runs: 2,
            b1_items: 2,
            c1_tokens: 30_000,
            c1_target: 20_000,
            n1_share_ppm: 400_000,
            n1_target_ppm: 200_000,
            u1_documents: 10,
            u1_target: 5,
            documents: vec!["*.md".to_owned()],
            min_closed_items: 5,
            min_run_sessions: 10,
            disabled: Vec::new(),
        }
    }
}

impl Thresholds {
    /// Checks the thresholds: every document glob is of the shape `*.<suffix>`.
    ///
    /// # Errors
    ///
    /// The first glob of another shape, named.
    pub fn check(&self) -> Result<(), String> {
        self.document_suffixes().map(|_| ())
    }

    /// The suffixes of [`Self::documents`] (`md` for `*.md`), as the listing of a repository's
    /// documents takes them.
    ///
    /// # Errors
    ///
    /// A glob of a shape other than `*.<suffix>`.
    pub fn document_suffixes(&self) -> Result<Vec<&str>, String> {
        context::suffixes(&self.documents)
    }
}

/// What a finding is about, per detector family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    File(FileEvidence),
    /// F1 and F2: the flaky tests per attempt.
    Tests(TestEvidence),
    /// R1: the permission denials per item.
    Denials(DenialEvidence),
    /// V1: the slow verdict runs.
    Verdicts(VerdictEvidence),
    /// B1: the items stopped by their budget.
    Budget(BudgetEvidence),
    /// C1: the start contexts of the run sessions.
    Start(StartEvidence),
    /// N1: the finding bytes of the directory and of the largest ones.
    Directory(DirectoryEvidence),
    /// U1: the unopened documents.
    Documents(DocumentEvidence),
}

/// The start contexts of a group's run sessions, in tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartEvidence {
    /// The lower median.
    pub median: u64,
    /// The run sessions with a start.
    pub sessions: u64,
    /// Every start, summed.
    pub total: u64,
}

/// The finding bytes of one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEvidence {
    pub directory: String,
    /// Its finding bytes.
    pub bytes: u64,
    /// Every finding byte of the group (single-path calls only).
    pub total: u64,
    /// The sessions it found in, sorted, at most [`MAX_SESSIONS`].
    pub sessions: Vec<String>,
    /// Every session, listed or not.
    pub sessions_total: u64,
    /// The largest directories and their bytes, largest first.
    pub top: Vec<(String, u64)>,
}

/// The tracked documents no session opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEvidence {
    /// Sorted, at most 50.
    pub documents: Vec<String>,
    /// Every one, listed or not.
    pub total: u64,
}

/// The flaky tests a finding counted, per attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestEvidence {
    /// How the tests were grouped: `Some("module_prefix")` for F2 (an approximation of a
    /// crate), `None` for one test.
    pub grouping: Option<&'static str>,
    /// The attempts, by item and number, at most [`MAX_SESSIONS`].
    pub attempts: Vec<TestAttempt>,
    /// Every attempt, listed or not.
    pub attempts_total: u64,
}

/// The flaky tests of one attempt a finding is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAttempt {
    pub item: String,
    pub attempt: u32,
    /// Sorted.
    pub tests: Vec<String>,
}

/// The permission denials of one tool and target, per item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialEvidence {
    pub denials: u64,
    /// Each item and its denials, by item, at most [`MAX_SESSIONS`].
    pub items: Vec<(String, u64)>,
    /// Every item, listed or not.
    pub items_total: u64,
}

/// The slow or timed-out runs of one verdict command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictEvidence {
    /// By item, attempt and run, at most [`MAX_SESSIONS`].
    pub runs: Vec<SlowRun>,
    /// Every run, listed or not.
    pub runs_total: u64,
}

/// One slow or timed-out verdict run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowRun {
    pub item: String,
    pub attempt: u32,
    pub run: u32,
    pub duration_ms: Option<u64>,
    /// `pass`, `fail`, `timeout` or `unready`; `None` while it runs.
    pub result: Option<&'static str>,
}

/// The items stopped by their budget cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetEvidence {
    /// Sorted, at most [`MAX_SESSIONS`].
    pub items: Vec<String>,
    /// Every item, listed or not.
    pub items_total: u64,
}

/// Adds `and N more <what>` to `lines` when `listed` is short of `total`.
fn more(lines: &mut Vec<String>, total: u64, listed: usize, what: &str) {
    let more = total.saturating_sub(listed as u64);
    if more > 0 {
        lines.push(format!("and {more} more {what}"));
    }
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
    // Long: one arm per family, each a few lines.
    #[allow(clippy::too_many_lines)]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::File(file) => {
                let mut lines = vec![format!(
                    "{} reads, {} result bytes of single-file reads, in {} sessions",
                    file.reads, file.result_bytes, file.sessions_total
                )];
                lines.extend(file.sessions.iter().map(|s| format!("session {s}")));
                more(
                    &mut lines,
                    file.sessions_total,
                    file.sessions.len(),
                    "sessions",
                );
                lines
            }
            Self::Tests(tests) => {
                let mut lines = Vec::new();
                if let Some(grouping) = tests.grouping {
                    lines.push(format!(
                        "grouping: {grouping} (an approximation of a crate)"
                    ));
                }
                lines.push(format!("flaky in {} attempts", tests.attempts_total));
                lines.extend(
                    tests.attempts.iter().map(|a| {
                        format!("{} attempt {}: {}", a.item, a.attempt, a.tests.join(", "))
                    }),
                );
                more(
                    &mut lines,
                    tests.attempts_total,
                    tests.attempts.len(),
                    "attempts",
                );
                lines
            }
            Self::Denials(denials) => {
                let mut lines = vec![format!(
                    "{} permission denials on {} items",
                    denials.denials, denials.items_total
                )];
                lines.extend(
                    denials
                        .items
                        .iter()
                        .map(|(item, n)| format!("{item}: {n} denials")),
                );
                more(
                    &mut lines,
                    denials.items_total,
                    denials.items.len(),
                    "items",
                );
                lines
            }
            Self::Verdicts(verdicts) => {
                let mut lines = vec![format!("{} slow verdict runs", verdicts.runs_total)];
                lines.extend(verdicts.runs.iter().map(|r| {
                    let duration = r
                        .duration_ms
                        .map_or_else(|| "no duration".to_owned(), |ms| format!("{ms} ms"));
                    format!(
                        "{} attempt {} run {}: {duration}, {}",
                        r.item,
                        r.attempt,
                        r.run,
                        r.result.unwrap_or("no result")
                    )
                }));
                more(&mut lines, verdicts.runs_total, verdicts.runs.len(), "runs");
                lines
            }
            Self::Budget(budget) => {
                let mut lines = vec![format!(
                    "{} items stopped by their budget",
                    budget.items_total
                )];
                lines.extend(budget.items.iter().map(|i| format!("item {i}")));
                more(&mut lines, budget.items_total, budget.items.len(), "items");
                lines
            }
            Self::Start(start) => vec![format!(
                "median start context {} tokens over {} run sessions ({} tokens in all)",
                start.median, start.sessions, start.total
            )],
            Self::Directory(dir) => {
                let mut lines = vec![format!(
                    "{}: {} of {} finding bytes, in {} sessions",
                    dir.directory, dir.bytes, dir.total, dir.sessions_total
                )];
                lines.extend(
                    dir.top
                        .iter()
                        .map(|(d, bytes)| format!("directory {d}: {bytes} bytes")),
                );
                lines.extend(dir.sessions.iter().map(|s| format!("session {s}")));
                more(
                    &mut lines,
                    dir.sessions_total,
                    dir.sessions.len(),
                    "sessions",
                );
                lines
            }
            Self::Documents(documents) => {
                let mut lines = vec![format!(
                    "{} tracked documents no session opened",
                    documents.total
                )];
                lines.extend(documents.documents.iter().map(|d| format!("document {d}")));
                more(
                    &mut lines,
                    documents.total,
                    documents.documents.len(),
                    "documents",
                );
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
    /// What the finding is about: `file` for the file detectors; `test`, `tool_target`,
    /// `command` or `repo` for the loop detectors.
    pub subject_kind: &'static str,
    /// A repository-relative path for a file; for a loop detector a test name or a
    /// `<module>::` prefix, `<tool> <target>` or a verdict command (redacted), or `.`.
    pub subject: String,
    /// The metric, in the detector's unit: exact.
    pub value: u64,
    /// The threshold `value` reached.
    pub threshold: u64,
    /// The value the remedy should bring the metric to or below: for a file detector half the
    /// threshold, at least 1; each loop detector's own.
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
    loops: loops::LoopRows<'a>,
}

/// What a detector sees of one group.
pub(crate) struct Context<'a, 'b> {
    pub(crate) repo: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) run_sessions: u64,
    pub(crate) closed_items: u64,
    pub(crate) files: &'b files::FileReads<'a>,
    pub(crate) loops: &'b loops::LoopRows<'a>,
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

/// The rows of `inputs` in `scope`, per (repository, provider) group, and the paths of each
/// repository any call wrote.
type Gathered<'a> = (
    BTreeMap<(&'a str, &'a str), Group<'a>>,
    BTreeMap<&'a str, BTreeSet<&'a str>>,
);

/// The groups of `inputs` in `scope`: their run sessions, closed items and file reads, and the
/// paths written per repository.
fn gather<'a>(inputs: &Inputs<'a>, scope: Scope<'_>) -> Gathered<'a> {
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
    for history in inputs.histories {
        let mut rows = loops::LoopRows::default();
        let Some((repo, provider)) = rows.add(history, scope) else {
            continue;
        };
        // A group only for an item with something to count: an item none of the loop
        // detectors counts leaves the groups as the file detectors see them.
        if !rows.is_empty() {
            groups
                .entry((repo, provider))
                .or_default()
                .loops
                .merge(rows);
        }
    }

    (groups, written)
}

/// A detector's metric of one subject in one group, with the group's sample: what
/// [`value_of`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Measure {
    /// The metric, in the detector's unit (0 for a subject the group never touched).
    pub(crate) value: u64,
    /// The sample the value is over, in the detector's sample unit.
    pub(crate) sample: u64,
    pub(crate) closed_items: u64,
    pub(crate) run_sessions: u64,
}

/// The metric of the detector `detector_id` for `subject` in the group of `provider` in
/// `scope` (which names one repository), computed by the very function [`detect`] uses, so a
/// follow-up's before and after cannot drift from the trigger; `None` for a detector this
/// function does not know. A group with no row is all zeros.
pub(crate) fn value_of(
    detector_id: &str,
    inputs: &Inputs<'_>,
    scope: Scope<'_>,
    provider: &str,
    subject: &str,
) -> Option<Measure> {
    let detector = *DETECTORS.iter().find(|d| d.id == detector_id)?;
    let (groups, written) = gather(inputs, scope);
    let none = BTreeSet::new();
    let empty = Group::default();
    let (repo, group) = groups
        .iter()
        .find(|((_, p), _)| *p == provider)
        .map_or((scope.repo.unwrap_or_default(), &empty), |((r, _), g)| {
            (*r, g)
        });
    let thresholds = Thresholds::default();
    let context = Context {
        repo,
        provider,
        run_sessions: group.run_sessions.len() as u64,
        closed_items: group.closed_items,
        files: &group.files,
        loops: &group.loops,
        written: written.get(repo).unwrap_or(&none),
        scope,
        thresholds: &thresholds,
    };
    if detector == UNOPENED_DOCUMENTS {
        let (value, sample) = context::u1_value(inputs, scope, &thresholds)?;
        return Some(Measure {
            value,
            sample,
            closed_items: groups.values().map(|g| g.closed_items).sum(),
            run_sessions: sample,
        });
    }
    let (value, sample) = if context::is_context(detector) {
        let key = (repo.to_owned(), provider.to_owned());
        let start = (detector == START_CONTEXT)
            .then(|| context::starts(inputs, scope).remove(&key))
            .flatten();
        let dirs = (detector == FINDING_BY_DIRECTORY)
            .then(|| context::dirs(inputs, scope).remove(&key))
            .flatten();
        (
            context::value(detector, start.as_ref(), dirs.as_ref(), subject),
            context::sample(detector, &context, start.as_ref()).0,
        )
    } else if loops::is_loop(detector) {
        (
            loops::value(detector, &context, subject),
            loops::sample(&context).0,
        )
    } else {
        (
            files::value(detector, &context, subject).unwrap_or(0),
            files::sample(detector, &context).0,
        )
    };
    Some(Measure {
        value,
        sample,
        closed_items: context.closed_items,
        run_sessions: context.run_sessions,
    })
}

/// The result bytes of the single-file reads of `provider`'s run sessions in `scope` (as D3
/// counts them) of every file `under` accepts, and the group's closed items.
pub(crate) fn read_bytes(
    inputs: &Inputs<'_>,
    scope: Scope<'_>,
    provider: &str,
    under: impl Fn(&str) -> bool,
) -> (u64, u64) {
    let (groups, _) = gather(inputs, scope);
    groups
        .iter()
        .filter(|((_, p), _)| *p == provider)
        .fold((0, 0), |(bytes, items), (_, group)| {
            (
                bytes + group.files.bytes(&under),
                items + group.closed_items,
            )
        })
}

/// The findings of every enabled detector over `inputs` in `scope`, per repository and
/// provider, and the detectors that stay silent below the minimum sample. Findings come in
/// detector order (D1, D2, D3, then the loop detectors, which have no bytes), each detector's
/// ranked (see [`files`]; a loop finding by its value); deterministic for equal input.
#[must_use]
pub fn detect(inputs: &Inputs<'_>, scope: Scope<'_>, thresholds: &Thresholds) -> Detection {
    let (groups, written) = gather(inputs, scope);
    let starts = context::starts(inputs, scope);
    let dirs = context::dirs(inputs, scope);
    let none = BTreeSet::new();
    let mut detection = Detection::default();
    for ((repo, provider), group) in &groups {
        let context = Context {
            repo,
            provider,
            run_sessions: group.run_sessions.len() as u64,
            closed_items: group.closed_items,
            files: &group.files,
            loops: &group.loops,
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
            // U1 is one finding per repository, after the groups.
            if detector == UNOPENED_DOCUMENTS {
                continue;
            }
            let key = ((*repo).to_owned(), (*provider).to_owned());
            let start = starts.get(&key);
            let per_item = detector == UNCHANGED_FILE_READ_BYTES;
            // C1 and N1 are over sessions only: closed items do not make their sample.
            let sufficient = if context::is_context(detector) {
                context::sample(detector, &context, start).0 >= thresholds.min_run_sessions
            } else {
                sufficient
            };
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
            if detector == START_CONTEXT {
                detection
                    .findings
                    .extend(start.and_then(|start| context::c1(&context, start)));
            } else if detector == FINDING_BY_DIRECTORY {
                detection
                    .findings
                    .extend(dirs.get(&key).and_then(|dirs| context::n1(&context, dirs)));
            } else if loops::is_loop(detector) {
                detection.findings.extend(loops::run(detector, &context));
            } else {
                detection.findings.extend(files::run(detector, &context));
            }
        }
    }
    if !thresholds.disabled.contains(&UNOPENED_DOCUMENTS.id) {
        let (findings, insufficient) = context::u1(inputs, scope, thresholds);
        detection.findings.extend(findings);
        detection.insufficient.extend(insufficient);
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
