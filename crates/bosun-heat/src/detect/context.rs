//! The context detectors: C1 (the start context of run sessions above a band: the standing
//! instructions are too heavy), N1 (the cost of finding concentrated in one directory: it lacks
//! a map) and U1 (#210, tracked documents no session opened in the window).
//!
//! - C1's value is `context_at_start` of the `run` kind of the metrics ([`crate::metrics`]): the
//!   lower median of the first prompt sizes of the main transcripts, one provider's only.
//! - N1 counts, in the run sessions that wrote, the result bytes of the reads and searches
//!   before the first write (the estimates' "cost of finding" before a write, in bytes) whose
//!   touches name exactly one path; a call naming several paths is attributed to none. A read
//!   folds into its parent directory, a search into its own path; the parent is cut at the last
//!   separator of the repository's path style. A session that never wrote found nothing for a
//!   change, so it adds none.
//! - U1 is not a provider's metric: one finding per repository over the sessions of every
//!   provider, its provider [`ANY_PROVIDER`]. A document is opened by a `Read` or a `Write`
//!   touch of any session (run or interactive: opening is not triggering); `Search`, `Exec` and
//!   `Unknown` touches do not open it. Paths are compared by their canonical keys under the
//!   repository's style. The instruction files an agent loads by itself ([`INSTRUCTION_FILES`])
//!   are never listed.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::{PathStyle, TouchKind, canonical_path, style_of};
use bosun_store::ToolRow;

use super::{
    Context, Detector, DirectoryEvidence, DocumentEvidence, Evidence, FINDING_BY_DIRECTORY,
    Finding, Insufficient, MAX_SESSIONS, START_CONTEXT, StartEvidence, Thresholds,
    UNOPENED_DOCUMENTS,
};
use crate::estimate::BYTES_PER_TOKEN;
use crate::exact::Scope;
use crate::metrics::{Inputs, join, metrics};

/// The provider of a finding over the sessions of every provider (U1).
pub const ANY_PROVIDER: &str = "any";

/// The files an agent loads by itself, by file name at any depth: never an unopened document.
pub const INSTRUCTION_FILES: [&str; 3] = ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"];

/// The directories N1 lists in its evidence besides its subject.
const TOP_DIRECTORIES: usize = 3;

/// The unopened documents U1's evidence lists at most.
const MAX_DOCUMENTS: usize = 50;

/// Whether `detector` is one of this module's.
pub(crate) fn is_context(detector: Detector) -> bool {
    [START_CONTEXT, FINDING_BY_DIRECTORY, UNOPENED_DOCUMENTS].contains(&detector)
}

/// The start contexts of the run sessions of a group: the median and the sessions it is over.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Start {
    median: u64,
    sessions: u64,
    /// Every start, summed.
    total: u64,
}

/// C1's figures per (repository, provider): `context_at_start` of the `run` kind.
pub(crate) fn starts(inputs: &Inputs<'_>, scope: Scope<'_>) -> BTreeMap<(String, String), Start> {
    metrics(inputs, scope)
        .into_iter()
        .filter(|m| m.id == "context_at_start" && m.kind == Some("run"))
        .map(|m| {
            (
                (m.repo, m.provider),
                Start {
                    median: m.value,
                    sessions: m.sample,
                    total: m.total,
                },
            )
        })
        .collect()
}

/// The finding bytes of one directory.
#[derive(Debug, Default)]
struct Directory {
    bytes: u64,
    sessions: BTreeSet<String>,
}

/// N1's figures of one group: the finding bytes per directory.
#[derive(Debug, Default)]
pub(crate) struct Dirs {
    directories: BTreeMap<String, Directory>,
    total: u64,
}

/// The directory `path` (repository-relative) folds into: a read's parent, a search's own path;
/// `.` for the root. Cut at the last separator of `style`.
fn directory(path: &str, kind: TouchKind, style: PathStyle) -> String {
    let separator = match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    };
    let dir = if kind == TouchKind::Search {
        path.trim_end_matches(separator)
    } else {
        path.rfind(separator).map_or("", |at| &path[..at])
    };
    if dir.is_empty() || dir == "." {
        ".".to_owned()
    } else {
        dir.to_owned()
    }
}

/// The one path `call` names with a read or a search touch, if its touches name exactly one
/// path, and how it was touched (a read wins over a search of the same path).
fn single_path(call: &ToolRow) -> Option<(&str, TouchKind)> {
    let paths: BTreeSet<&str> = call.touches.iter().map(|(p, _)| p.as_str()).collect();
    let [path] = paths.into_iter().collect::<Vec<_>>()[..] else {
        return None;
    };
    let kinds: Vec<TouchKind> = call.touches.iter().map(|(_, k)| *k).collect();
    if kinds.contains(&TouchKind::Read) {
        Some((path, TouchKind::Read))
    } else if kinds.contains(&TouchKind::Search) {
        Some((path, TouchKind::Search))
    } else {
        None
    }
}

/// N1's figures per (repository, provider), over the run sessions of `scope` that wrote.
pub(crate) fn dirs(inputs: &Inputs<'_>, scope: Scope<'_>) -> BTreeMap<(String, String), Dirs> {
    // Per root session: whether it wrote, and its finding calls before the write.
    type Root = (bool, Vec<(String, u64)>);
    let mut joined = join(inputs);
    // In time order; the calls of one message keep their order.
    joined.sort_by_key(|j| j.message.ts_ms);
    let mut sessions: BTreeMap<(&str, &str, &str), Root> = BTreeMap::new();
    for j in &joined {
        let Some(repo) = super::in_scope(j.message, scope) else {
            continue;
        };
        if j.message.kind != "run" {
            continue;
        }
        let session = sessions
            .entry((repo, &j.message.provider, &j.message.session))
            .or_default();
        if session.0 {
            continue;
        }
        let touches = |kind: TouchKind| j.call.touches.iter().any(|(_, k)| *k == kind);
        if touches(TouchKind::Write) {
            session.0 = true;
        } else if (touches(TouchKind::Read) || touches(TouchKind::Search))
            && let Some((path, kind)) = single_path(j.call)
        {
            let dir = directory(path, kind, style_of(repo));
            session.1.push((dir, j.call.result_bytes));
        }
    }
    let mut out: BTreeMap<(String, String), Dirs> = BTreeMap::new();
    for ((repo, provider, session), (wrote, found)) in sessions {
        if !wrote {
            continue;
        }
        let group = out
            .entry((repo.to_owned(), provider.to_owned()))
            .or_default();
        for (dir, bytes) in found {
            group.total += bytes;
            let directory = group.directories.entry(dir).or_default();
            directory.bytes += bytes;
            directory.sessions.insert(session.to_owned());
        }
    }
    out
}

impl Dirs {
    /// The largest directory, by bytes then by name.
    fn largest(&self) -> Option<(&str, &Directory)> {
        self.directories
            .iter()
            .max_by(|a, b| a.1.bytes.cmp(&b.1.bytes).then_with(|| b.0.cmp(a.0)))
            .map(|(dir, d)| (dir.as_str(), d))
    }

    /// The share of `dir` in every finding byte, in ppm; 0 without one.
    fn share(&self, dir: &str) -> u64 {
        let bytes = self.directories.get(dir).map_or(0, |d| d.bytes);
        ppm(bytes, self.total)
    }
}

fn ppm(part: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    u64::try_from(u128::from(part) * 1_000_000 / u128::from(total)).unwrap_or(u64::MAX)
}

/// The sample of C1 and N1 in a group, in sessions: the run sessions with a start for C1, the
/// group's run sessions for N1.
pub(crate) fn sample(
    detector: Detector,
    context: &Context<'_, '_>,
    start: Option<&Start>,
) -> (u64, &'static str) {
    if detector == START_CONTEXT {
        (start.map_or(0, |s| s.sessions), "session")
    } else {
        (context.run_sessions, "session")
    }
}

/// A finding of this module's `detector` in `context`'s group.
fn finding(
    context: &Context<'_, '_>,
    detector: Detector,
    (subject_kind, subject): (&'static str, &str),
    (value, threshold, target): (u64, u64, u64),
    sample: u64,
    impact_bytes: u64,
    evidence: Evidence,
) -> Finding {
    Finding {
        detector,
        repo: context.repo.to_owned(),
        provider: context.provider.to_owned(),
        subject_kind,
        subject: subject.to_owned(),
        value,
        threshold,
        target,
        sample,
        sample_unit: "session",
        window_from_ms: context.scope.from_ms,
        window_to_ms: context.scope.to_ms,
        impact_bytes,
        impact_eqv_estimate: (detector == FINDING_BY_DIRECTORY)
            .then(|| impact_bytes.div_ceil(BYTES_PER_TOKEN)),
        evidence,
    }
}

/// C1: the median start context of the group's run sessions, above the band.
pub(crate) fn c1(context: &Context<'_, '_>, start: &Start) -> Option<Finding> {
    let t = context.thresholds;
    // Mutant (`mutant_heat_c1_threshold_inf`): C1 never fires.
    let band = if cfg!(mutant_heat_c1_threshold_inf) {
        u64::MAX
    } else {
        t.c1_tokens
    };
    (start.median > band).then(|| {
        finding(
            context,
            START_CONTEXT,
            ("repo", "."),
            (start.median, band, t.c1_target),
            start.sessions,
            0,
            Evidence::Start(StartEvidence {
                median: start.median,
                sessions: start.sessions,
                total: start.total,
            }),
        )
    })
}

/// N1: the directory holding the largest share of the group's finding bytes, at or above the
/// threshold.
pub(crate) fn n1(context: &Context<'_, '_>, dirs: &Dirs) -> Option<Finding> {
    let t = context.thresholds;
    // Mutant (`mutant_heat_n1_threshold_inf`): the threshold is above any share.
    let threshold = if cfg!(mutant_heat_n1_threshold_inf) {
        1_000_001
    } else {
        t.n1_share_ppm
    };
    let (dir, largest) = dirs.largest()?;
    let value = dirs.share(dir);
    if value < threshold {
        return None;
    }
    let mut top: Vec<(&String, &Directory)> = dirs.directories.iter().collect();
    top.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(b.0)));
    let evidence = DirectoryEvidence {
        directory: dir.to_owned(),
        bytes: largest.bytes,
        total: dirs.total,
        sessions: largest
            .sessions
            .iter()
            .take(MAX_SESSIONS)
            .cloned()
            .collect(),
        sessions_total: largest.sessions.len() as u64,
        top: top
            .into_iter()
            .take(TOP_DIRECTORIES)
            .map(|(d, found)| (d.clone(), found.bytes))
            .collect(),
    };
    Some(finding(
        context,
        FINDING_BY_DIRECTORY,
        ("dir", dir),
        (value, threshold, t.n1_target_ppm),
        context.run_sessions,
        largest.bytes,
        Evidence::Directory(evidence),
    ))
}

/// The value of C1 (the median start) or N1 (the share of the directory `subject`) in a group.
pub(crate) fn value(
    detector: Detector,
    start: Option<&Start>,
    dirs: Option<&Dirs>,
    subject: &str,
) -> u64 {
    if detector == START_CONTEXT {
        start.map_or(0, |s| s.median)
    } else {
        dirs.map_or(0, |d| d.share(subject))
    }
}

/// The suffixes of `globs`, each of the shape `*.<suffix>`.
///
/// # Errors
///
/// A glob of any other shape.
pub(crate) fn suffixes(globs: &[String]) -> Result<Vec<&str>, String> {
    globs
        .iter()
        .map(|glob| {
            glob.strip_prefix("*.")
                .filter(|s| !s.is_empty() && !s.contains(['*', '?', '[', '/', '\\', '.']))
                .ok_or_else(|| {
                    format!(
                        "document glob {glob:?}: only the shape `*.<suffix>` (such as `*.md`) \
                         is supported"
                    )
                })
        })
        .collect()
}

/// The file name of the `/`-separated `path`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `path`'s file name ends with `.<suffix>` of one of `suffixes`, case-insensitively.
fn has_suffix(path: &str, suffixes: &[&str]) -> bool {
    let name = file_name(path).as_bytes();
    suffixes.iter().any(|suffix| {
        let tail = suffix.len() + 1;
        name.len() > tail
            && name[name.len() - tail] == b'.'
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    })
}

/// Whether `path` is an instruction file an agent loads by itself.
fn is_instruction_file(path: &str) -> bool {
    let name = file_name(path);
    INSTRUCTION_FILES
        .iter()
        .any(|file| name.eq_ignore_ascii_case(file))
}

/// U1's figures of one repository: its run sessions of any provider and the paths any session
/// opened, as canonical keys.
#[derive(Debug, Default)]
pub(crate) struct Opened {
    run_sessions: BTreeSet<(String, String)>,
    closed_items: u64,
    paths: BTreeSet<String>,
}

/// U1's figures per repository of `scope`.
pub(crate) fn opened(inputs: &Inputs<'_>, scope: Scope<'_>) -> BTreeMap<String, Opened> {
    let mut out: BTreeMap<String, Opened> = BTreeMap::new();
    for row in inputs.usage {
        let Some(repo) = super::in_scope(row, scope) else {
            continue;
        };
        let entry = out.entry(repo.to_owned()).or_default();
        if row.kind == "run" {
            entry
                .run_sessions
                .insert((row.provider.clone(), row.session.clone()));
        }
    }
    for j in join(inputs) {
        let Some(repo) = super::in_scope(j.message, scope) else {
            continue;
        };
        let style = style_of(repo);
        let entry = out.entry(repo.to_owned()).or_default();
        for (path, kind) in &j.call.touches {
            if matches!(kind, TouchKind::Read | TouchKind::Write) {
                entry.paths.insert(canonical_path(path, style));
            }
        }
    }
    for closed in crate::metrics::closed_items(inputs, scope) {
        out.entry(closed.repo.to_owned()).or_default().closed_items += 1;
    }
    out
}

/// The tracked documents of `repo` no session in `opened` opened, sorted; `None` without a
/// listing or with a glob of an unsupported shape.
fn unopened(
    repo: &str,
    opened: &Opened,
    documents: Option<&[String]>,
    thresholds: &Thresholds,
) -> Option<Vec<String>> {
    let documents = documents?;
    let suffixes = suffixes(&thresholds.documents).ok()?;
    let style = style_of(repo);
    let mut out: Vec<String> = documents
        .iter()
        .filter(|path| has_suffix(path, &suffixes))
        // Mutant (`mutant_heat_u1_lists_instruction_files`): the instruction files are listed.
        .filter(|path| cfg!(mutant_heat_u1_lists_instruction_files) || !is_instruction_file(path))
        .filter(|path| !opened.paths.contains(&canonical_path(path, style)))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    Some(out)
}

/// U1's value (the unopened documents) and sample (the run sessions) of `repo`; `None` without
/// a listing.
pub(crate) fn u1_value(
    inputs: &Inputs<'_>,
    scope: Scope<'_>,
    thresholds: &Thresholds,
) -> Option<(u64, u64)> {
    let repo = scope.repo?;
    let all = opened(inputs, scope);
    let empty = Opened::default();
    let figures = all.get(repo).unwrap_or(&empty);
    let unopened = unopened(repo, figures, inputs.documents, thresholds)?;
    Some((unopened.len() as u64, figures.run_sessions.len() as u64))
}

/// U1 over every repository of `scope` with a row: its finding, or its [`Insufficient`] below
/// the minimum sample or without a listing. The documents are those of `scope`'s repository,
/// so a scope over every repository has no listing.
pub(crate) fn u1(
    inputs: &Inputs<'_>,
    scope: Scope<'_>,
    thresholds: &Thresholds,
) -> (Vec<Finding>, Vec<Insufficient>) {
    let (mut findings, mut insufficient) = (Vec::new(), Vec::new());
    for (repo, figures) in opened(inputs, scope) {
        let run_sessions = figures.run_sessions.len() as u64;
        let documents = if scope.repo == Some(repo.as_str()) {
            inputs.documents
        } else {
            None
        };
        let listed = unopened(&repo, &figures, documents, thresholds);
        let Some(unopened) = listed.filter(|_| run_sessions >= thresholds.min_run_sessions) else {
            insufficient.push(Insufficient {
                detector: UNOPENED_DOCUMENTS.id,
                repo: repo.clone(),
                provider: ANY_PROVIDER.to_owned(),
                closed_items: figures.closed_items,
                run_sessions,
            });
            continue;
        };
        // Mutant (`mutant_heat_u1_threshold_inf`): U1 fires only at `u64::MAX` documents.
        let threshold = if cfg!(mutant_heat_u1_threshold_inf) {
            u64::MAX
        } else {
            thresholds.u1_documents
        };
        let value = unopened.len() as u64;
        if value < threshold {
            continue;
        }
        let evidence = DocumentEvidence {
            documents: unopened.iter().take(MAX_DOCUMENTS).cloned().collect(),
            total: value,
        };
        findings.push(Finding {
            detector: UNOPENED_DOCUMENTS,
            repo: repo.clone(),
            provider: ANY_PROVIDER.to_owned(),
            subject_kind: "repo",
            subject: ".".to_owned(),
            value,
            threshold,
            target: thresholds.u1_target,
            sample: run_sessions,
            sample_unit: "session",
            window_from_ms: scope.from_ms,
            window_to_ms: scope.to_ms,
            impact_bytes: 0,
            impact_eqv_estimate: None,
            evidence: Evidence::Documents(evidence),
        });
    }
    (findings, insufficient)
}

#[cfg(test)]
mod tests;
