//! The tool detectors of #210, over the columns Claude's parser version 4 stores of a call
//! (#232): X1 (a shell command run verbatim again and again in one run session: a missing
//! script), T1 (the wall time of a tool, from its call to its result) and M1 (the share of
//! media in the result bytes).
//!
//! - **The parser gate.** A (repository, provider) group in which any message of the window
//!   that made a tool call, of any session kind, was parsed by a version below
//!   [`MIN_PARSER_VERSION`] holds calls without these columns: all three answer
//!   `insufficient` for it (the reason asks for `bosun reingest`) and fire nothing.
//! - Only `run` sessions count; the sample of all three is the group's run sessions, at least
//!   [`super::Thresholds::min_run_sessions`] (closed items do not make it).
//! - Every value is exact: a count of calls, milliseconds, a share of bytes in ppm. None has an
//!   estimate; M1's `impact_bytes` are its media bytes, which are base64 image data and not
//!   text tokens, so no bytes-per-token figure applies to them.
//! - X1 groups calls by their fingerprint (`command_hash`) only, and its evidence holds no more
//!   of a command than `command_text`, which #232 stored already scrubbed and capped. A message
//!   attributed to its repository only after it was stored has no fingerprint until
//!   `bosun reingest`, although its parser version is 4: X1 then undercounts it.

use std::collections::{BTreeMap, BTreeSet};

use bosun_store::ToolRow;

use super::{
    Context, Detector, Evidence, Finding, MAX_SESSIONS, MEDIA_SHARE, REPEATED_COMMAND,
    TOOL_WALL_TIME,
};

/// The parser version from which a call carries its command's fingerprint and text, its times
/// and its media bytes: Claude's (`bosun_ingest::claude::PARSER_VERSION` 4, #232; not imported,
/// `bosun-heat` does not depend on `bosun-ingest`). The `stub` provider parses at version 1 and
/// stores none of these columns, so it is always `insufficient` for these detectors, which is
/// right: there is nothing to measure.
pub const MIN_PARSER_VERSION: u32 = 4;

/// Why a tool detector is silent in a group of an older parser.
pub const REINGEST_NEEDED: &str =
    "rows of a parser version below 4 in the window: run `bosun reingest`";

/// The tools whose subject is the tool and its command head.
const SHELLS: [&str; 2] = ["Bash", "PowerShell"];

/// The tools M1's evidence names at most.
const TOP_TOOLS: usize = 3;

/// One fingerprint's calls.
#[derive(Debug, Default)]
struct Command<'a> {
    /// Its scrubbed, capped text, as the first call gave it.
    text: Option<&'a str>,
    /// Its calls per run session.
    sessions: BTreeMap<&'a str, u64>,
}

/// One subject's calls: `(session, call_ts_ms, result_ts_ms)` for each one timed, and the
/// calls skipped for want of both times in order.
#[derive(Debug, Default)]
struct Timed<'a> {
    calls: Vec<(&'a str, i64, i64)>,
    skipped: u64,
}

/// What the tool detectors count of one group's calls.
#[derive(Debug, Default)]
pub(crate) struct ToolCalls<'a> {
    /// The lowest parser version of a message with a call, of any session kind.
    min_parser: Option<u32>,
    /// Per fingerprint, the run sessions' calls.
    commands: BTreeMap<&'a str, Command<'a>>,
    /// Per subject (`Bash cargo`, `WebFetch`), the run sessions' calls.
    timed: BTreeMap<String, Timed<'a>>,
    /// The media bytes of the run sessions' results, per tool.
    media: BTreeMap<&'a str, u64>,
    /// Every result byte of the run sessions, media included.
    result_bytes: u64,
}

/// T1's subject of `call`: its tool, and for a shell its command head.
fn subject(call: &ToolRow) -> String {
    match &call.target {
        Some(head) if SHELLS.contains(&call.name.as_str()) => format!("{} {head}", call.name),
        _ => call.name.clone(),
    }
}

impl<'a> ToolCalls<'a> {
    /// `call`, made in the session `session` of kind `run` when `run`.
    pub(crate) fn add(&mut self, session: &'a str, run: bool, call: &'a ToolRow) {
        self.min_parser = Some(
            self.min_parser
                .map_or(call.parser_version, |v| v.min(call.parser_version)),
        );
        if !run {
            return;
        }
        if let Some(hash) = call.command_hash.as_deref() {
            let command = self.commands.entry(hash).or_default();
            if command.text.is_none() {
                command.text = call.command_text.as_deref();
            }
            *command.sessions.entry(session).or_default() += 1;
        }
        let timed = self.timed.entry(subject(call)).or_default();
        match (call.call_ts_ms, call.result_ts_ms) {
            (Some(called), Some(answered)) if answered >= called => {
                timed.calls.push((session, called, answered));
            }
            _ => timed.skipped += 1,
        }
        *self.media.entry(&call.name).or_default() += call.result_media_bytes.unwrap_or(0);
        self.result_bytes += call.result_bytes;
    }

    /// Whether every call of the group was parsed by [`MIN_PARSER_VERSION`] or later (a group
    /// without a call is).
    pub(crate) fn current(&self) -> bool {
        // Mutant (`mutant_heat_ignore_parser_version`): the gate is skipped.
        cfg!(mutant_heat_ignore_parser_version)
            || self.min_parser.is_none_or(|v| v >= MIN_PARSER_VERSION)
    }

    fn media_bytes(&self) -> u64 {
        self.media.values().sum()
    }
}

/// Whether `detector` is one of this module's.
pub(crate) fn is_tool(detector: Detector) -> bool {
    [REPEATED_COMMAND, TOOL_WALL_TIME, MEDIA_SHARE].contains(&detector)
}

/// The sample of every tool detector: the group's run sessions.
pub(crate) fn sample(context: &Context<'_, '_>) -> (u64, &'static str) {
    (context.run_sessions, "session")
}

/// Whether a tool detector may fire in `context`'s group: `Ok`, or why it is silent as
/// [`super::Insufficient::reason`] says it (`Some` for the parser gate, `None` below the
/// minimum sample).
pub(crate) fn sufficient(context: &Context<'_, '_>) -> Result<(), Option<&'static str>> {
    if !context.tools.current() {
        Err(Some(REINGEST_NEEDED))
    } else if context.run_sessions < context.thresholds.min_run_sessions {
        Err(None)
    } else {
        Ok(())
    }
}

/// `part / whole` in parts per million, rounded down; 0 for an empty whole.
fn ppm(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    u64::try_from(u128::from(part) * 1_000_000 / u128::from(whole)).unwrap_or(u64::MAX)
}

/// The nearest-rank 90th percentile of `sorted`: the value at `ceil(0.9 n) - 1`; 0 when empty.
fn p90(sorted: &[u64]) -> u64 {
    let n = sorted.len();
    if n == 0 {
        return 0;
    }
    sorted[(9 * n).div_ceil(10) - 1]
}

/// The wall times of `timed`, sorted.
fn walls(timed: &Timed<'_>) -> Vec<u64> {
    let mut walls: Vec<u64> = timed
        .calls
        .iter()
        .map(|(_, called, answered)| u64::try_from(answered - called).unwrap_or(0))
        .collect();
    walls.sort_unstable();
    walls
}

/// The value of `detector` for `subject` in `context`'s group (0 for a subject it never saw):
/// X1 the most calls of the fingerprint in one run session, T1 the p90 of the subject's wall
/// times in ms, M1 the media share of the result bytes in ppm.
pub(crate) fn value(detector: Detector, context: &Context<'_, '_>, subject: &str) -> u64 {
    let rows = context.tools;
    match detector.id {
        id if id == REPEATED_COMMAND.id => rows
            .commands
            .get(subject)
            .and_then(|c| c.sessions.values().max().copied())
            .unwrap_or(0),
        id if id == TOOL_WALL_TIME.id => rows.timed.get(subject).map_or(0, |t| p90(&walls(t))),
        id if id == MEDIA_SHARE.id => ppm(rows.media_bytes(), rows.result_bytes),
        _ => 0,
    }
}

/// The findings of `detector` in one group.
pub(crate) fn run(detector: Detector, context: &Context<'_, '_>) -> Vec<Finding> {
    match detector.id {
        id if id == REPEATED_COMMAND.id => x1(context),
        id if id == TOOL_WALL_TIME.id => t1(context),
        id if id == MEDIA_SHARE.id => m1(context),
        _ => Vec::new(),
    }
}

/// What a tool finding ranks by, larger first: T1's total wall time per closed item (or per
/// run session); `None` for another finding.
pub(crate) fn rank(finding: &Finding) -> Option<u64> {
    match &finding.evidence {
        Evidence::Wall(wall) => Some(wall.total_per),
        _ => None,
    }
}

/// A finding of `detector` about `subject`: exact, with `impact_bytes` and no estimate.
fn finding(
    context: &Context<'_, '_>,
    detector: Detector,
    (subject_kind, subject): (&'static str, String),
    (value, threshold, target): (u64, u64, u64),
    impact_bytes: u64,
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
        impact_bytes,
        impact_eqv_estimate: None,
        evidence,
    }
}

/// The calls of one command a finding counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEvidence {
    /// The command's text as #232 stored it (scrubbed, at most 200 bytes); `None` when the
    /// store holds only its fingerprint.
    pub text: Option<String>,
    /// The sessions it ran in at least the threshold's times, and its calls in each, by
    /// session, at most [`MAX_SESSIONS`].
    pub sessions: Vec<(String, u64)>,
    /// Every such session, listed or not.
    pub sessions_total: u64,
}

/// The wall times of one tool (or shell command head) a finding counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallEvidence {
    /// The calls with both times in order.
    pub calls: u64,
    /// The calls skipped: a time unknown, or the result before the call.
    pub skipped: u64,
    /// The sum of the wall times, in ms.
    pub total_ms: u64,
    /// The union of the calls' intervals per session, summed over the sessions, in ms: the time
    /// the calls took when parallel ones are not read as serial.
    pub union_ms: u64,
    /// The calls whose interval overlaps another call's of the same session.
    pub parallel_calls: u64,
    /// `total_ms` per closed item, or per run session when the window has fewer closed items
    /// than the minimum sample: what T1's findings rank by.
    pub total_per: u64,
    /// `closed_item` or `session`.
    pub per: &'static str,
}

/// The media of a group's results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaEvidence {
    pub media_bytes: u64,
    /// Every result byte, media included.
    pub result_bytes: u64,
    /// The tools with the most media bytes and theirs, largest first, at most three.
    pub top: Vec<(String, u64)>,
}

/// The lines of a tool detector's evidence.
pub(crate) fn command_lines(command: &CommandEvidence) -> Vec<String> {
    // The text first, so a reader sees the command and not only its fingerprint.
    let mut lines = vec![
        command
            .text
            .clone()
            .unwrap_or_else(|| "(no command text stored)".to_owned()),
    ];
    lines.extend(
        command
            .sessions
            .iter()
            .map(|(s, n)| format!("session {s}: {n} calls")),
    );
    super::more(
        &mut lines,
        command.sessions_total,
        command.sessions.len(),
        "sessions",
    );
    lines
}

pub(crate) fn wall_lines(wall: &WallEvidence) -> Vec<String> {
    vec![
        format!(
            "{} calls, {} ms in all ({} ms per {}), {} ms as the union of the intervals per \
             session",
            wall.calls, wall.total_ms, wall.total_per, wall.per, wall.union_ms
        ),
        format!("parallel_calls: {}", wall.parallel_calls),
        format!(
            "skipped: {} calls without a call and result time in order",
            wall.skipped
        ),
    ]
}

pub(crate) fn media_lines(media: &MediaEvidence) -> Vec<String> {
    let mut lines = vec![format!(
        "{} media bytes of {} result bytes",
        media.media_bytes, media.result_bytes
    )];
    lines.extend(
        media
            .top
            .iter()
            .map(|(tool, bytes)| format!("tool {tool}: {bytes} media bytes")),
    );
    lines
}

/// X1: per fingerprint, its most calls in one run session.
fn x1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_x1_threshold_inf`): X1 fires only at `u64::MAX` calls.
    let threshold = if cfg!(mutant_heat_x1_threshold_inf) {
        u64::MAX
    } else {
        context.thresholds.x1_calls
    };
    let mut out = Vec::new();
    for (hash, command) in &context.tools.commands {
        let value = value(REPEATED_COMMAND, context, hash);
        if value < threshold {
            continue;
        }
        let repeated: Vec<(&str, u64)> = command
            .sessions
            .iter()
            .filter(|(_, n)| **n >= threshold)
            .map(|(s, n)| (*s, *n))
            .collect();
        let evidence = CommandEvidence {
            text: command.text.map(str::to_owned),
            sessions: repeated
                .iter()
                .take(MAX_SESSIONS)
                .map(|(s, n)| ((*s).to_owned(), *n))
                .collect(),
            sessions_total: repeated.len() as u64,
        };
        out.push(finding(
            context,
            REPEATED_COMMAND,
            ("command", (*hash).to_owned()),
            (value, threshold, context.thresholds.x1_target),
            0,
            Evidence::Commands(evidence),
        ));
    }
    out
}

/// The union of the intervals of `calls` per session and the calls that overlap another one
/// of their session.
fn overlap(calls: &[(&str, i64, i64)]) -> (u64, u64) {
    let mut by_session: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for (session, from, to) in calls {
        by_session.entry(session).or_default().push((*from, *to));
    }
    let (mut union, mut parallel) = (0_i64, 0_u64);
    for intervals in by_session.values_mut() {
        intervals.sort_unstable();
        let mut overlapping: BTreeSet<usize> = BTreeSet::new();
        for (i, a) in intervals.iter().enumerate() {
            for (j, b) in intervals.iter().enumerate().skip(i + 1) {
                if b.0 < a.1 && a.0 < b.1 {
                    overlapping.extend([i, j]);
                }
            }
        }
        parallel += overlapping.len() as u64;
        let mut current: Option<(i64, i64)> = None;
        for &(from, to) in intervals.iter() {
            current = match current {
                Some((start, end)) if from <= end => Some((start, end.max(to))),
                Some((start, end)) => {
                    union += end - start;
                    Some((from, to))
                }
                None => Some((from, to)),
            };
        }
        if let Some((start, end)) = current {
            union += end - start;
        }
    }
    (u64::try_from(union).unwrap_or(0), parallel)
}

/// T1: per tool (and shell command head), the p90 of its wall times.
fn t1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_t1_threshold_inf`): T1's threshold is `i64::MAX` ms.
    let threshold = if cfg!(mutant_heat_t1_threshold_inf) {
        i64::MAX.unsigned_abs()
    } else {
        context.thresholds.t1_ms
    };
    let (per_count, per) = if context.closed_items >= context.thresholds.min_closed_items {
        (context.closed_items, "closed_item")
    } else {
        (context.run_sessions, "session")
    };
    let mut out = Vec::new();
    for (subject, timed) in &context.tools.timed {
        if (timed.calls.len() as u64) < context.thresholds.t1_min_calls {
            continue;
        }
        let walls = walls(timed);
        let value = p90(&walls);
        if value < threshold {
            continue;
        }
        let total_ms: u64 = walls.iter().sum();
        let (union_ms, parallel_calls) = overlap(&timed.calls);
        let evidence = WallEvidence {
            calls: timed.calls.len() as u64,
            skipped: timed.skipped,
            total_ms,
            union_ms,
            parallel_calls,
            total_per: total_ms.checked_div(per_count).unwrap_or(total_ms),
            per,
        };
        out.push(finding(
            context,
            TOOL_WALL_TIME,
            ("tool_target", subject.clone()),
            (value, threshold, context.thresholds.t1_target_ms),
            0,
            Evidence::Wall(evidence),
        ));
    }
    out
}

/// M1: the media share of the group's result bytes, with enough media bytes.
fn m1(context: &Context<'_, '_>) -> Vec<Finding> {
    // Mutant (`mutant_heat_m1_threshold_inf`): M1's threshold is above 1_000_000 ppm.
    let threshold = if cfg!(mutant_heat_m1_threshold_inf) {
        1_000_001
    } else {
        context.thresholds.m1_share_ppm
    };
    let rows = context.tools;
    let media_bytes = rows.media_bytes();
    let value = value(MEDIA_SHARE, context, ".");
    if value < threshold || media_bytes < context.thresholds.m1_media_bytes {
        return Vec::new();
    }
    let mut top: Vec<(String, u64)> = rows
        .media
        .iter()
        .filter(|(_, bytes)| **bytes > 0)
        .map(|(tool, bytes)| ((*tool).to_owned(), *bytes))
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(TOP_TOOLS);
    let evidence = MediaEvidence {
        media_bytes,
        result_bytes: rows.result_bytes,
        top,
    };
    vec![finding(
        context,
        MEDIA_SHARE,
        ("repo", ".".to_owned()),
        (value, threshold, context.thresholds.m1_target_ppm),
        media_bytes,
        Evidence::Media(evidence),
    )]
}

#[cfg(test)]
mod tests;
