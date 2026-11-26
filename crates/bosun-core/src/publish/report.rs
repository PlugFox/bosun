//! The item report of #5: a pure function from an item's facts to the body of its sticky
//! comment.
//!
//! The default sections of stage 1: the outcome and its reason; the verdict, flaky tests,
//! landing, diff, time parked and cost; the attempts; the agent's summary, quoted and marked
//! as the agent's; the verdict output tails, collapsed. Every string the agent influenced is
//! redacted and sanitised; every section has a budget, so the body stays under GitHub's
//! [`BODY_CAP`], and the tails take what is left, truncated from the front.

use std::fmt::Write as _;

use super::redact::redact;
use super::sanitise::{sanitise, sanitise_inline};
use super::scrub;
use super::text::{chars, clean, code_block, code_span, first_chars};
use crate::attempt::{AttemptState, RunResult};
use crate::item::{Guard, ItemId, Outcome, ParkReason, Reason, State};

/// GitHub's cap on a comment or pull request body, in characters.
pub const BODY_CAP: usize = 65_536;

/// The item title, in characters after sanitising.
const TITLE_CAP: usize = 256;
/// The words of the reason (lint findings, a refusal's error).
const DETAIL_CAP: usize = 2_000;
/// The verdict command.
const COMMAND_CAP: usize = 500;
/// A verdict's one-line result.
const LINE_CAP: usize = 300;
/// The reasons an attempt was refused with, together; each is at most [`LINE_CAP`].
const REFUSAL_CAP: usize = 1_000;
/// The tool calls an attempt's CLI denied, together: the entries shown (each a tool and its
/// target, or a tool and its count), the rest counted.
const DENIAL_CAP: usize = 500;
/// A name shown as a code span: a test, a path, a branch.
const NAME_CAP: usize = 200;
/// A model name or a session id.
const ID_CAP: usize = 80;
/// Entries shown of a list of names; the rest are counted.
const LIST_MAX: usize = 20;
/// Attempts listed, the latest ones; earlier ones are counted.
const ATTEMPTS_MAX: usize = 20;
/// Session ids listed per attempt.
const SESSIONS_MAX: usize = 5;
/// The agent's "what was done", quoted.
const SUMMARY_CAP: usize = 16_000;
/// One decision, deviation, question or follow-up.
const ENTRY_CAP: usize = 1_000;
/// The whole of the agent's section.
const AGENT_CAP: usize = 24_000;
/// One verdict output tail, its fence and its summary line included.
const TAIL_CAP: usize = 24_000;
/// A tail budget below which the tail is left out rather than shown as a few characters.
const TAIL_MIN: usize = 200;

/// What the report says about an item: facts from the database (#3), filled by the daemon at
/// send time (#5). Strings the agent influenced are raw here; the renderer cleans them.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportFacts {
    /// The item.
    pub item: ItemId,
    /// The issue's title.
    pub title: String,
    /// The item's state.
    pub state: State,
    /// The reason in words, when the state has one worth spelling out: lint findings, a
    /// refusal's error. May hold text the agent influenced.
    pub detail: Option<String>,
    /// The verdict command, from the snapshot.
    pub verdict: String,
    /// The model its sessions asked for (`--model`): the card's, else the repository's default,
    /// from the snapshot (#133). What they ran is each attempt's [`AttemptFacts::model`].
    pub model: Option<String>,
    /// The attempts, in order.
    pub attempts: Vec<AttemptFacts>,
    /// Tests that failed and then passed on the re-run.
    pub flaky_tests: Vec<String>,
    /// Time parked, per stretch.
    pub parked: Vec<ParkedFacts>,
    /// Landing, once it happened.
    pub landing: Option<Landing>,
    /// The item's diff against its base.
    pub diff: Option<DiffStat>,
    /// The agent's summary of the attempt it belongs to (`submit_summary`, #7).
    pub summary: Option<AgentSummary>,
    /// The item's cost.
    pub bill: Bill,
}

/// One attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptFacts {
    /// Its number, from 1.
    pub number: u32,
    /// Its state.
    pub state: AttemptState,
    /// The model its sessions ran.
    pub model: Option<String>,
    /// The effort level its sessions asked for (`--effort`); `None` when none was passed.
    pub effort: Option<String>,
    /// Its session ids.
    pub sessions: Vec<String>,
    /// Crash retries used.
    pub crash_retries: u32,
    /// Its verdict, once one ran.
    pub verdict: Option<VerdictFacts>,
    /// Why the daemon refused it before its verdict (#137): its failed preconditions or guard
    /// refusals, in the daemon's words; empty when nothing refused it. A reason may quote the
    /// repository (a path, a commit message), so the renderer cleans it.
    pub refusal: Vec<String>,
    /// The tool calls the agent CLI refused during its sessions (#7), in order.
    pub denials: Vec<DeniedCall>,
}

/// A tool call the agent CLI refused under the permission lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedCall {
    /// The tool's name.
    pub tool: String,
    /// The call's subject (a command line, a path). Agent-influenced: it names files and
    /// quotes commands, so a public repository's report only counts the calls.
    pub target: Option<String>,
}

/// A verdict of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictFacts {
    /// The first run.
    pub first: RunResult,
    /// The re-run after a non-passing first run.
    pub rerun: Option<RunResult>,
    /// Wall-clock time of the runs, milliseconds.
    pub duration_ms: u64,
    /// The result in one line (the output's last meaningful line, say). Agent-influenced.
    pub line: String,
    /// The tail of the output. Agent-influenced.
    pub tail: String,
}

/// A stretch of time parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedFacts {
    /// Why.
    pub reason: ParkReason,
    /// How long, milliseconds.
    pub ms: u64,
}

/// The landing mode of #8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingMode {
    /// Commits stay on the local branch.
    Local,
    /// Merged into the target branch.
    Merge,
    /// A pull request.
    Pr,
}

/// How the item landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    /// The mode.
    pub mode: LandingMode,
    /// The item branch.
    pub branch: String,
    /// The landed commits.
    pub commits: Vec<String>,
    /// The pull request's number, in `pr` mode.
    pub pull_request: Option<u64>,
}

/// The diff stat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStat {
    /// Files changed.
    pub files: u32,
    /// Lines added.
    pub insertions: u64,
    /// Lines removed.
    pub deletions: u64,
    /// Protected paths the item was allowed to change.
    pub protected: Vec<String>,
}

/// The agent's summary (`submit_summary`, #7). Every field is the agent's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentSummary {
    /// The attempt it belongs to.
    pub attempt: u32,
    /// The HEAD commit it was submitted at.
    pub head: String,
    /// What was done (Markdown).
    pub done: String,
    /// Decisions taken.
    pub decisions: Vec<String>,
    /// Deviations from the card.
    pub deviations: Vec<String>,
    /// Non-blocking questions.
    pub questions: Vec<String>,
    /// Follow-ups.
    pub followups: Vec<String>,
}

/// The item's cost (#3, #10).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bill {
    /// Input-equivalent tokens.
    pub eqv: f64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Wall-clock time of the item, milliseconds.
    pub wall_clock_ms: u64,
    /// Commits landed.
    pub commits: u32,
}

/// How to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportOptions {
    /// Publish the verdict output tails.
    pub tails: bool,
    /// Publish the reasons an attempt was refused with before its verdict (#137): they name
    /// files of the working tree and quote commit messages. Also the targets of the tool calls
    /// the agent CLI denied (#7): a command line, a path.
    pub refusals: bool,
}

impl ReportOptions {
    /// The defaults for a repository: verdict tails and refusal reasons are off on public
    /// repositories (#5, #137).
    #[must_use]
    pub fn for_repository(public: bool) -> Self {
        Self {
            tails: !public,
            refusals: !public,
        }
    }
}

/// The hidden key of an item's sticky report comment.
#[must_use]
pub fn report_key(item: &ItemId) -> String {
    format!("report:{item}")
}

/// The body of the item's sticky comment: the hidden key marker on the first line, then the
/// default sections. At most [`BODY_CAP`] characters.
#[must_use]
pub fn render_report(facts: &ReportFacts, options: ReportOptions) -> String {
    let mut body = super::marker(&report_key(&facts.item));
    let title = sanitise_inline(&facts.title, TITLE_CAP);
    let _ = write!(
        body,
        "\n### bosun: {title}\n\n**{}**",
        state_text(facts.state)
    );
    if let Some(detail) = &facts.detail {
        let _ = write!(
            body,
            ": {}",
            sanitise_inline(&redact_head(detail, DETAIL_CAP), DETAIL_CAP)
        );
    }
    body.push_str("\n\n");
    facts_list(&mut body, facts);
    attempts(&mut body, &facts.attempts, options.refusals);
    if let Some(summary) = &facts.summary {
        body.push_str("\n\n");
        body.push_str(&agent_section(summary));
    }
    tails(&mut body, &facts.attempts, options);
    body
}

/// The bullet list of facts: verdict, model asked, flaky tests, landing, diff, time parked, cost.
fn facts_list(body: &mut String, facts: &ReportFacts) {
    let _ = write!(body, "- Verdict: {}", name(&facts.verdict, COMMAND_CAP));
    if let Some(model) = &facts.model {
        let _ = write!(body, "\n- Model asked: {}", name(model, ID_CAP));
    }
    if !facts.flaky_tests.is_empty() {
        let _ = write!(
            body,
            "\n- Flaky tests: {}",
            names(&facts.flaky_tests, NAME_CAP, true)
        );
    }
    if let Some(landing) = &facts.landing {
        let mode = match landing.mode {
            LandingMode::Local => "local",
            LandingMode::Merge => "merge",
            LandingMode::Pr => "pull request",
        };
        let _ = write!(
            body,
            "\n- Landing: {mode}, branch {}",
            name(&landing.branch, NAME_CAP)
        );
        if !landing.commits.is_empty() {
            let _ = write!(body, ", commits {}", names(&landing.commits, ID_CAP, false));
        }
        if let Some(pr) = landing.pull_request {
            let _ = write!(body, ", #{pr}");
        }
    }
    if let Some(diff) = &facts.diff {
        let _ = write!(
            body,
            "\n- Diff: {} files, +{} \u{2212}{}",
            diff.files,
            group(diff.insertions),
            group(diff.deletions)
        );
        if !diff.protected.is_empty() {
            let _ = write!(
                body,
                "; protected paths allowed: {}",
                names(&diff.protected, NAME_CAP, true)
            );
        }
    }
    if !facts.parked.is_empty() {
        let total: u64 = facts.parked.iter().map(|p| p.ms).sum();
        let parts: Vec<String> = facts
            .parked
            .iter()
            .take(LIST_MAX)
            .map(|p| format!("{} {}", park_text(p.reason), duration(p.ms)))
            .collect();
        let _ = write!(
            body,
            "\n- Parked: {} ({})",
            duration(total),
            parts.join(", ")
        );
    }
    let bill = &facts.bill;
    let _ = write!(
        body,
        "\n- Cost: eqv {}; tokens {} in, {} out, {} cache read, {} cache write; wall clock {}",
        eqv(bill.eqv),
        group(bill.input_tokens),
        group(bill.output_tokens),
        group(bill.cache_read_tokens),
        group(bill.cache_write_tokens),
        duration(bill.wall_clock_ms)
    );
    if bill.commits > 0 {
        let _ = write!(
            body,
            "; eqv per commit {}",
            eqv(bill.eqv / f64::from(bill.commits))
        );
    }
}

/// The attempts, the latest [`ATTEMPTS_MAX`] of them; the reasons of a refusal only when
/// `refusals`, else only that there was one.
fn attempts(body: &mut String, attempts: &[AttemptFacts], refusals: bool) {
    if attempts.is_empty() {
        return;
    }
    body.push_str("\n\n#### Attempts\n");
    let skipped = attempts.len().saturating_sub(ATTEMPTS_MAX);
    if skipped > 0 {
        let _ = write!(body, "\n- {skipped} earlier attempts not shown");
    }
    for attempt in &attempts[skipped..] {
        let _ = write!(
            body,
            "\n- Attempt {}: {}",
            attempt.number,
            attempt.state.as_str()
        );
        if let Some(verdict) = &attempt.verdict {
            let runs = match verdict.rerun {
                Some(rerun) => format!("{}, then {}", run_text(verdict.first), run_text(rerun)),
                None => run_text(verdict.first).to_owned(),
            };
            let _ = write!(
                body,
                ", verdict {runs} in {}",
                duration(verdict.duration_ms)
            );
            let line = sanitise_inline(&redact_head(&verdict.line, LINE_CAP), LINE_CAP);
            if !line.is_empty() {
                let _ = write!(body, ": {line}");
            }
        }
        if !attempt.refusal.is_empty() && !refusals {
            body.push_str(", refused before its verdict (the reasons are in `bosun item`)");
        } else if !attempt.refusal.is_empty() {
            // Each reason capped, as many as fit the attempt's budget; the rest counted.
            let mut reasons: Vec<String> = Vec::new();
            let mut used = 0;
            for reason in &attempt.refusal {
                let text = sanitise_inline(&redact_head(reason, LINE_CAP), LINE_CAP);
                if !reasons.is_empty() && used + chars(&text) > REFUSAL_CAP {
                    break;
                }
                used += chars(&text);
                reasons.push(text);
            }
            let _ = write!(body, ", refused before its verdict: {}", reasons.join("; "));
            let more = attempt.refusal.len() - reasons.len();
            if more > 0 {
                let _ = write!(body, " and {more} more");
            }
        }
        denials(body, &attempt.denials, refusals);
        if let Some(model) = &attempt.model {
            let _ = write!(body, "; model {}", name(model, ID_CAP));
        }
        if let Some(effort) = &attempt.effort {
            let _ = write!(body, "; effort {}", name(effort, ID_CAP));
        }
        if !attempt.sessions.is_empty() {
            let shown = &attempt.sessions[..attempt.sessions.len().min(SESSIONS_MAX)];
            let _ = write!(body, "; sessions {}", names(shown, ID_CAP, false));
            let more = attempt.sessions.len() - shown.len();
            if more > 0 {
                let _ = write!(body, " and {more} more");
            }
        }
        if attempt.crash_retries > 0 {
            let _ = write!(body, "; crash retries {}", attempt.crash_retries);
        }
    }
}

/// The agent's section: quoted, so nothing in it passes for the daemon's own text, and headed
/// as the agent's.
fn agent_section(summary: &AgentSummary) -> String {
    let mut section = format!(
        "#### Agent's summary\n\n> *Written by the agent (attempt {}, at {}); bosun did not \
         check it.*",
        summary.attempt,
        name(&summary.head, ID_CAP)
    );
    // Appends a quoted block after a blank line, or after a line break for the next item of
    // a list; refuses what would overflow the section's budget.
    let add = |section: &mut String, markdown: &str, next_item: bool| -> bool {
        let quoted = quote(markdown);
        let sep = if next_item { "\n" } else { "\n>\n" };
        if chars(section) + chars(sep) + chars(&quoted) > AGENT_CAP {
            return false;
        }
        section.push_str(sep);
        section.push_str(&quoted);
        true
    };
    let done = fit(&redact_head(&summary.done, SUMMARY_CAP), SUMMARY_CAP);
    if !done.is_empty() {
        add(&mut section, &done, false);
    }
    let lists: [(&str, &[String]); 4] = [
        ("Decisions", &summary.decisions),
        ("Deviations from the card", &summary.deviations),
        ("Questions (non-blocking)", &summary.questions),
        ("Follow-ups", &summary.followups),
    ];
    let mut omitted = 0;
    for (heading, entries) in lists {
        if entries.is_empty() {
            continue;
        }
        if omitted > 0 || !add(&mut section, &format!("**{heading}**"), false) {
            omitted += entries.len();
            continue;
        }
        let mut first = true;
        for (i, entry) in entries.iter().enumerate() {
            if omitted > 0 || i >= LIST_MAX {
                omitted += 1;
                continue;
            }
            let text = sanitise(&redact_head(entry, ENTRY_CAP), ENTRY_CAP);
            if text.is_empty() {
                continue;
            }
            let item = format!("- {}", text.replace('\n', "\n  "));
            if add(&mut section, &item, !first) {
                first = false;
            } else {
                omitted += 1;
            }
        }
    }
    if omitted > 0 {
        let _ = write!(section, "\n>\n> *({omitted} more entries omitted)*");
    }
    section
}

/// The sanitised `markdown` that, quoted, fits in `cap` characters.
fn fit(markdown: &str, cap: usize) -> String {
    let mut inner = cap;
    loop {
        let text = sanitise(markdown, inner);
        let len = chars(&quote(&text));
        if len <= cap || inner == 0 {
            return text;
        }
        // Shrink in proportion to the overshoot, and by at least one character.
        inner = (inner.saturating_mul(cap) / len).min(inner - 1);
    }
}

/// `markdown` as a block quote.
fn quote(markdown: &str) -> String {
    markdown
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The verdict output tails, collapsed, in what is left of [`BODY_CAP`]: the latest
/// attempt's tail is fitted first, and the attempts the list shows are the only ones looked at.
fn tails(body: &mut String, attempts: &[AttemptFacts], options: ReportOptions) {
    const HEADER: &str = "\n\n#### Verdict output";
    let shown = &attempts[attempts.len().saturating_sub(ATTEMPTS_MAX)..];
    let has_tail = |a: &AttemptFacts| {
        a.verdict
            .as_ref()
            .is_some_and(|v| !v.tail.trim().is_empty())
    };
    if !shown.iter().any(has_tail) {
        return;
    }
    if !options.tails {
        body.push_str("\n\n*Verdict output is not published on public repositories.*");
        return;
    }
    let mut remaining = BODY_CAP.saturating_sub(chars(body) + chars(HEADER));
    let mut blocks = Vec::new();
    for attempt in shown.iter().rev() {
        if remaining < 2 * TAIL_MIN {
            break;
        }
        let Some(verdict) = &attempt.verdict else {
            continue;
        };
        #[cfg(mutant_publish_redact_before_clean)]
        // Mutant (`mutant_publish_redact_before_clean`): redacted, then cleaned.
        let tail = clean(&redact(&verdict.tail));
        #[cfg(not(mutant_publish_redact_before_clean))]
        let tail = scrub(&verdict.tail);
        let tail = tail.trim_end();
        if tail.is_empty() {
            continue;
        }
        let allow = remaining.min(TAIL_CAP);
        let mut content = allow.saturating_sub(TAIL_MIN);
        while content >= TAIL_MIN {
            let block = tail_block(attempt.number, tail, content);
            let len = chars(&block);
            if len <= allow {
                remaining -= len;
                blocks.push(block);
                break;
            }
            content = content.saturating_sub(len - allow);
        }
    }
    if !blocks.is_empty() {
        body.push_str(HEADER);
        for block in blocks.iter().rev() {
            body.push_str(block);
        }
    }
}

/// One collapsed tail of at most about `content` characters of output.
fn tail_block(number: u32, tail: &str, content: usize) -> String {
    let (kept, cut) = truncate_front(tail, content);
    let lines = kept.split('\n').count();
    let mut summary = format!("Attempt {number}: the last {lines} lines");
    if cut > 0 {
        let _ = write!(summary, ", {} earlier lines cut", group(cut as u64));
    }
    format!(
        "\n\n<details><summary>{summary}</summary>\n\n{}\n\n</details>",
        code_block(kept, "", "")
    )
}

/// The end of `tail` in at most `max` characters, starting at a line boundary, and the number
/// of lines cut from the front. A tail is cut from the front: its last lines say why it failed.
fn truncate_front(tail: &str, max: usize) -> (&str, usize) {
    let total = chars(tail);
    if total <= max {
        return (tail, 0);
    }
    #[cfg(mutant_publish_truncate_back)]
    {
        let head = first_chars(tail, max);
        let kept = head.rfind('\n').map_or(head, |i| &head[..i]);
        let cut = tail[kept.len()..].matches('\n').count();
        (kept, cut)
    }
    #[cfg(not(mutant_publish_truncate_back))]
    {
        let skip = tail.len() - first_chars_from_end(tail, max).len();
        let mut kept = &tail[skip..];
        if skip > 0
            && !tail[..skip].ends_with('\n')
            && let Some(i) = kept.find('\n')
        {
            kept = &kept[i + 1..];
        }
        let cut = tail[..tail.len() - kept.len()].matches('\n').count();
        (kept, cut)
    }
}

/// The last `n` characters of `s`.
#[cfg(not(mutant_publish_truncate_back))]
fn first_chars_from_end(s: &str, n: usize) -> &str {
    match s.char_indices().rev().nth(n.saturating_sub(1)) {
        Some((i, _)) if n > 0 => &s[i..],
        _ if n == 0 => "",
        _ => s,
    }
}

/// The tool calls an attempt's CLI denied: with `targets`, each tool and its target as code
/// spans; else only the count per tool, most frequent first. As many entries as fit
/// [`DENIAL_CAP`], the rest counted.
fn denials(body: &mut String, denials: &[DeniedCall], targets: bool) {
    if denials.is_empty() {
        return;
    }
    let n = denials.len();
    let calls = if n == 1 { "tool call" } else { "tool calls" };
    // Mutant (`mutant_report_denial_targets_on_public`): the targets are listed on a public
    // repository too.
    if !targets && !cfg!(mutant_report_denial_targets_on_public) {
        let mut by_tool: Vec<(&str, usize)> = Vec::new();
        for denial in denials {
            match by_tool.iter_mut().find(|(tool, _)| *tool == denial.tool) {
                Some((_, count)) => *count += 1,
                None => by_tool.push((&denial.tool, 1)),
            }
        }
        // A stable sort: a tie keeps the order of the first call.
        by_tool.sort_by_key(|a| std::cmp::Reverse(a.1));
        let counts = by_tool.iter().filter_map(|(tool, count)| {
            let tool = sanitise_inline(&redact_head(tool, ID_CAP), ID_CAP);
            (!tool.is_empty()).then(|| format!("{tool} {count}"))
        });
        let _ = write!(
            body,
            "; {n} {calls} denied ({})",
            fit_entries(counts, by_tool.len())
        );
        return;
    }
    let entries = denials.iter().filter_map(|denial| {
        let tool = name(&denial.tool, ID_CAP);
        if tool.is_empty() {
            return None;
        }
        Some(match denial.target.as_deref().map(|t| name(t, NAME_CAP)) {
            Some(target) if !target.is_empty() => format!("{tool} {target}"),
            _ => tool,
        })
    });
    let _ = write!(body, "; {n} {calls} denied: {}", fit_entries(entries, n));
}

/// `entries` (of `total`; one that renders empty is left out) joined, as many as fit
/// [`DENIAL_CAP`] (the first always), the rest of `total` counted.
fn fit_entries(entries: impl Iterator<Item = String>, total: usize) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0;
    for text in entries {
        if !shown.is_empty() && used + chars(&text) > DENIAL_CAP {
            break;
        }
        used += chars(&text);
        shown.push(text);
    }
    let mut out = shown.join(", ");
    if total > shown.len() {
        let _ = write!(out, " and {} more", total - shown.len());
    }
    out
}

/// The cleaned and redacted head of `text` ([`scrub`]), long enough for anything that renders
/// in `cap` characters.
///
/// Redacting a megabyte to publish a kilobyte is waste, so the text is cut first, and the cut
/// drops the trailing partial word: a token cut in half would no longer have its shape and
/// would slip past the redactor.
fn redact_head(text: &str, cap: usize) -> String {
    let head = first_chars(text, cap.saturating_mul(4).saturating_add(4096));
    let cut = if head.len() == text.len() {
        text
    } else {
        head.rfind(char::is_whitespace).map_or("", |i| &head[..i])
    };
    if cfg!(mutant_publish_redact_before_clean) {
        // Mutant (`mutant_publish_redact_before_clean`): redacted before it is cleaned; the
        // caller cleans it after.
        return redact(cut);
    }
    scrub(cut)
}

/// `s` as a code span of at most `cap` characters of content.
fn name(s: &str, cap: usize) -> String {
    let cleaned = clean(&redact_head(s, cap));
    let short = first_chars(&cleaned, cap);
    if short.len() < cleaned.len() {
        code_span(&format!("{short}\u{2026}"))
    } else {
        code_span(short)
    }
}

/// The first [`LIST_MAX`] of `items` as code spans, the rest counted; `latest` keeps the last.
fn names(items: &[String], cap: usize, latest: bool) -> String {
    let shown = if latest {
        &items[items.len().saturating_sub(LIST_MAX)..]
    } else {
        &items[..items.len().min(LIST_MAX)]
    };
    let mut out = shown
        .iter()
        .map(|s| name(s, cap))
        .filter(|span| !span.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let more = items.len() - shown.len();
    if more > 0 {
        let _ = write!(out, " and {more} more");
    }
    out
}

fn state_text(state: State) -> String {
    match state {
        State::Ready => "Ready".into(),
        State::Claimed => "Claimed".into(),
        State::Running => "In progress: running".into(),
        State::Parked(park) => format!("In progress: parked ({})", park_text(park.reason)),
        State::Verifying => "Verify: the verdict is running".into(),
        State::Landing => "Verify: landing".into(),
        State::Done(Outcome::Pass) => "Done: the verdict passed".into(),
        State::Done(Outcome::Flaky) => "Done: the verdict passed on its re-run (flaky)".into(),
        State::Done(Outcome::UserClosed) => "Done: closed by hand, no verdict".into(),
        State::NeedsYou(reason) => format!("Needs you: {}", reason_text(reason)),
        State::Cancelled => "Cancelled".into(),
    }
}

fn reason_text(reason: Reason) -> &'static str {
    match reason {
        Reason::Lint => "the card failed lint",
        Reason::Cycle => "the card is in a blocked-by cycle",
        Reason::Question => "the agent asked a blocking question",
        Reason::Auth => "the provider's authentication expired",
        Reason::Budget => "the item's budget was exceeded",
        Reason::ParkCap => "too many parks and resumes",
        Reason::ConfigChanged => "the configuration or the permission policy changed",
        Reason::AttemptsExhausted => "the failed attempts reached the limit",
        Reason::Guard(Guard::ProtectedPath) => "a commit touches a protected path",
        Reason::Guard(Guard::IgnoredFile) => "a new ignored file outside the build directories",
        Reason::Guard(Guard::GitConfig) => "git's own configuration changed",
        Reason::Guard(Guard::Checkout) => "the user's checkout changed during the run",
        Reason::LandingRefused => "landing was refused",
        Reason::Setup => "the item's worktree could not be made or set up",
        Reason::Handshake => "the session's bosun MCP server did not start, twice",
        Reason::Identity => "the identity checks refused the session",
        Reason::Stale => "the daemon's view of the item was older than its stored history",
    }
}

fn park_text(reason: ParkReason) -> &'static str {
    match reason {
        ParkReason::Limit => "provider limit",
        ParkReason::Paused => "paused",
        ParkReason::Killed => "kill switch",
        ParkReason::DailyCap => "daily cap",
    }
}

fn run_text(run: RunResult) -> &'static str {
    match run {
        RunResult::Pass => "pass",
        RunResult::Fail => "fail",
        RunResult::Timeout => "timeout",
    }
}

/// `n` with thousands separators.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An eqv figure, rounded to whole tokens.
fn eqv(value: f64) -> String {
    if value.is_finite() && value >= 0.0 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "finite, non-negative and rounded; saturates above u64::MAX"
        )]
        let whole = value.round() as u64;
        group(whole)
    } else {
        "n/a".into()
    }
}

/// A duration in milliseconds, for a person.
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => format!("{ms} ms"),
        1..60 => format!("{}.{} s", s, (ms % 1000) / 100),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m {:02}s", s / 3600, (s % 3600) / 60, s % 60),
    }
}

#[cfg(test)]
mod tests;
