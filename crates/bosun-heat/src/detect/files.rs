//! The file detectors: D1 (a file read across many run sessions), D2 (a file re-read in one
//! run session) and D3 (#210, the bytes read of a file nobody wrote in the window).
//!
//! - A read is a `(path, Read)` touch. An `Exec` (a script run by an interpreter), an `Unknown`
//!   (a file a command names to an unknown effect), a `Search` and a `Write` are never reads
//!   (#44 A).
//! - A call's result bytes belong to a file only when the call names that one file, reads it
//!   and does not write it: `cat a b` counts as a read of each, and its bytes go to neither.

use std::collections::{BTreeMap, BTreeSet};

use bosun_core::TouchKind;
use bosun_store::ToolRow;

use super::{
    Context, Detector, Evidence, FILE_READ_ACROSS_RUNS, FILE_REREAD_IN_SESSION, FileEvidence,
    Finding, MAX_SESSIONS, UNCHANGED_FILE_READ_BYTES,
};

/// The reads of one file in one session.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Reads {
    reads: u64,
    /// The result bytes of the single-file reads.
    bytes: u64,
}

/// The reads of one group's run sessions, per path, then per session.
#[derive(Debug, Default)]
pub(crate) struct FileReads<'a> {
    paths: BTreeMap<&'a str, BTreeMap<&'a str, Reads>>,
}

/// Whether a touch is a read.
fn is_read(kind: TouchKind) -> bool {
    // Mutant (`mutant_heat_exec_counts_as_read`): a script run is taken for a read.
    kind == TouchKind::Read || (cfg!(mutant_heat_exec_counts_as_read) && kind == TouchKind::Exec)
}

/// The paths `call` wrote.
pub(crate) fn writes(call: &ToolRow) -> impl Iterator<Item = &str> {
    call.touches
        .iter()
        .filter(|(_, kind)| *kind == TouchKind::Write)
        .map(|(path, _)| path.as_str())
}

/// The one file `call` names, if it names exactly one, reads it and does not write it.
fn single_file(call: &ToolRow) -> Option<&str> {
    let paths: BTreeSet<&str> = call.touches.iter().map(|(p, _)| p.as_str()).collect();
    let [path] = paths.into_iter().collect::<Vec<_>>()[..] else {
        return None;
    };
    let read = call.touches.iter().any(|(_, kind)| is_read(*kind));
    let wrote = writes(call).next().is_some();
    (read && !wrote).then_some(path)
}

impl<'a> FileReads<'a> {
    /// The reads of `call`, made in the run session `session`.
    pub(crate) fn add(&mut self, session: &'a str, call: &'a ToolRow) {
        let read: BTreeSet<&str> = call
            .touches
            .iter()
            .filter(|(_, kind)| is_read(*kind))
            .map(|(path, _)| path.as_str())
            .collect();
        let single = single_file(call);
        for path in read {
            let reads = self
                .paths
                .entry(path)
                .or_default()
                .entry(session)
                .or_default();
            reads.reads += 1;
            if single == Some(path) {
                reads.bytes += call.result_bytes;
            }
        }
    }
}

/// The evidence of the sessions `sessions` (path's reads per session).
fn evidence<'s>(sessions: impl Iterator<Item = (&'s &'s str, &'s Reads)>) -> FileEvidence {
    let mut out = FileEvidence {
        reads: 0,
        result_bytes: 0,
        sessions: Vec::new(),
        sessions_total: 0,
    };
    for (session, reads) in sessions {
        out.reads += reads.reads;
        out.result_bytes += reads.bytes;
        out.sessions_total += 1;
        if out.sessions.len() < MAX_SESSIONS {
            out.sessions.push((*session).to_owned());
        }
    }
    out
}

/// The findings of `detector` in one group.
pub(crate) fn run(detector: Detector, context: &Context<'_, '_>) -> Vec<Finding> {
    match detector.id {
        id if id == FILE_READ_ACROSS_RUNS.id => d1(context),
        id if id == FILE_REREAD_IN_SESSION.id => d2(context),
        id if id == UNCHANGED_FILE_READ_BYTES.id => d3(context),
        _ => Vec::new(),
    }
}

/// What a detector's findings are ranked by, larger first: the value, but D3's bytes (a loop
/// finding, which has no file evidence, by its value).
pub(crate) fn rank(finding: &Finding) -> u64 {
    let Evidence::File(evidence) = &finding.evidence else {
        return finding.value;
    };
    if finding.detector == UNCHANGED_FILE_READ_BYTES {
        // Mutant (`mutant_heat_d3_rank_by_opens`): unchanged files ranked by their reads.
        if cfg!(mutant_heat_d3_rank_by_opens) {
            evidence.reads
        } else {
            finding.impact_bytes
        }
    } else {
        finding.value
    }
}

impl FileReads<'_> {
    /// The result bytes of the single-file reads of every path `under` accepts.
    pub(crate) fn bytes(&self, under: impl Fn(&str) -> bool) -> u64 {
        self.paths
            .iter()
            .filter(|(path, _)| under(path))
            .flat_map(|(_, sessions)| sessions.values())
            .map(|r| r.bytes)
            .sum()
    }
}

/// The sample of `detector`'s value in a group: run sessions for D1 and D2, closed items for D3.
pub(crate) fn sample(detector: Detector, context: &Context<'_, '_>) -> (u64, &'static str) {
    if detector == UNCHANGED_FILE_READ_BYTES {
        (context.closed_items, "closed_item")
    } else {
        (context.run_sessions, "session")
    }
}

/// The value of `detector` for the file `path` whose reads per session are `sessions`: D1 the
/// run sessions that read it, D2 its most reads in one of them, D3 the result bytes of its
/// single-file reads per closed item. `None` where the detector does not apply: D3 for a file
/// written in the window or a group without a closed item, and an unknown detector.
fn value_over(
    detector: Detector,
    context: &Context<'_, '_>,
    path: &str,
    sessions: &BTreeMap<&str, Reads>,
) -> Option<u64> {
    match detector.id {
        id if id == FILE_READ_ACROSS_RUNS.id => Some(sessions.len() as u64),
        id if id == FILE_REREAD_IN_SESSION.id => {
            Some(sessions.values().map(|r| r.reads).max().unwrap_or(0))
        }
        id if id == UNCHANGED_FILE_READ_BYTES.id => {
            if context.written.contains(path) || context.closed_items == 0 {
                return None;
            }
            let bytes: u64 = sessions.values().map(|r| r.bytes).sum();
            Some(bytes / context.closed_items)
        }
        _ => None,
    }
}

/// The value of `detector` for the file `path` in `context`'s group (see [`value_over`]); a
/// file the group never read has no reads.
pub(crate) fn value(detector: Detector, context: &Context<'_, '_>, path: &str) -> Option<u64> {
    let none = BTreeMap::new();
    let sessions = context.files.paths.get(path).unwrap_or(&none);
    value_over(detector, context, path, sessions)
}

/// D1: per file, the run sessions that read it.
fn d1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_d1_threshold_inf`): D1 never fires.
    let threshold = if cfg!(mutant_heat_d1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.d1_sessions
    };
    let sample = sample(FILE_READ_ACROSS_RUNS, context);
    let mut out = Vec::new();
    for (path, sessions) in &context.files.paths {
        let Some(value) = value_over(FILE_READ_ACROSS_RUNS, context, path, sessions) else {
            continue;
        };
        if value >= threshold {
            let evidence = evidence(sessions.iter());
            let finding = context.finding(
                FILE_READ_ACROSS_RUNS,
                path,
                (value, threshold),
                sample,
                evidence,
            );
            out.push(finding);
        }
    }
    out
}

/// D2: per file, its most reads in one run session.
fn d2(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_d2_threshold_inf`): D2 never fires.
    let threshold = if cfg!(mutant_heat_d2_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.d2_reads
    };
    let sample = sample(FILE_REREAD_IN_SESSION, context);
    let mut out = Vec::new();
    for (path, sessions) in &context.files.paths {
        let Some(value) = value_over(FILE_REREAD_IN_SESSION, context, path, sessions) else {
            continue;
        };
        if value >= threshold {
            let evidence = evidence(sessions.iter().filter(|(_, r)| r.reads >= threshold));
            let finding = context.finding(
                FILE_REREAD_IN_SESSION,
                path,
                (value, threshold),
                sample,
                evidence,
            );
            out.push(finding);
        }
    }
    out
}

/// D3: per file nobody wrote in the window, its single-file read bytes per closed item.
fn d3(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_d3_threshold_inf`): D3 never fires.
    let threshold = if cfg!(mutant_heat_d3_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.d3_bytes_per_item
    };
    let sample = sample(UNCHANGED_FILE_READ_BYTES, context);
    let mut out = Vec::new();
    for (path, sessions) in &context.files.paths {
        let Some(value) = value_over(UNCHANGED_FILE_READ_BYTES, context, path, sessions) else {
            continue;
        };
        if value >= threshold {
            let evidence = evidence(sessions.iter());
            let finding = context.finding(
                UNCHANGED_FILE_READ_BYTES,
                path,
                (value, threshold),
                sample,
                evidence,
            );
            out.push(finding);
        }
    }
    out
}

#[cfg(test)]
mod tests;
