//! The texts of the pull request of #8: its body, the update of a body people may have edited,
//! and the squash commit message. Pure functions; nothing here sends anything.
//!
//! The body is the agent's summary, redacted and passed through the allowlist sanitiser
//! ([`super::sanitise`]); then the daemon's fact block between two hidden markers
//! ([`super::marker`]), which [`update`] rewrites without touching the text around it; then
//! `Closes #N` for the item's issue, the only closing keyword the body may carry, and the
//! daemon's own. The sanitiser strips every imitation of a marker from agent text, so the agent
//! cannot forge the region's bounds.
//!
//! The squash message is plain text, read by git, GitHub and CI services: the agent's summary
//! goes in with no line that reads as a trailer, no closing keyword that acts and no CI skip
//! directive (what `bosun-git` refuses in a commit message, #7), and the daemon's `Item:`,
//! `Run:` and `Session:` trailers close it.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use super::redact::redact;
use super::sanitise::{sanitise, sanitise_inline};
use super::text::{chars, clean, first_chars};
use super::{BODY_CAP, marker};

/// The key of the marker that opens the daemon's region of the body.
const REGION_START_KEY: &str = "pr:facts:start";
/// The key of the marker that closes it.
const REGION_END_KEY: &str = "pr:facts:end";
/// The agent's summary in the body, in characters after sanitising: the rest of
/// [`BODY_CAP`] leaves room for the fact block.
const SUMMARY_CAP: usize = 48_000;
/// One fact's label or value, in characters after sanitising.
const FACT_CAP: usize = 500;
/// Facts listed; the rest are counted.
const FACTS_MAX: usize = 40;
/// The body's line when the agent's summary is empty.
const NO_DESCRIPTION: &str = "*No description was given.*";
/// The squash message's subject, in characters.
const SUBJECT_CAP: usize = 200;
/// The squash message's summary, in characters.
const MESSAGE_CAP: usize = 16_000;
/// A trailer's value, in characters.
const TRAILER_CAP: usize = 200;

/// The daemon's trailers on a squash commit (#7): the values the git guards expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailers {
    /// The item, as `owner/repo#N`.
    pub item: String,
    /// The run.
    pub run: String,
    /// The agent session that did the work.
    pub session: String,
}

/// The body of the item's pull request: the agent's `summary` (raw; redacted and sanitised
/// here), the daemon's `facts` (label and value pairs) in its delimited region, and
/// `Closes #issue`.
#[must_use]
pub fn body(summary: &str, facts: &[(&str, &str)], issue: u64) -> String {
    let described = description(summary);
    let shown = if described.is_empty() {
        NO_DESCRIPTION
    } else {
        described.as_str()
    };
    format!("{shown}\n\n{}\n\nCloses #{issue}", region(facts))
}

/// The agent's summary as it appears in the body.
fn description(summary: &str) -> String {
    #[cfg(mutant_publish_pr_raw_summary)]
    {
        // Mutant (`mutant_publish_pr_raw_summary`): the summary is published as written.
        summary.trim().to_owned()
    }
    #[cfg(not(mutant_publish_pr_raw_summary))]
    {
        let head = first_chars(summary, SUMMARY_CAP.saturating_mul(4).saturating_add(4096));
        sanitise(&redact(head), SUMMARY_CAP)
    }
}

/// `existing` (a pull request body people may have edited) with the daemon's region rewritten
/// to hold `facts`. Text outside the region is kept byte for byte; a body without the region
/// gets it appended.
#[must_use]
pub fn update(existing: &str, facts: &[(&str, &str)]) -> String {
    let fresh = region(facts);
    #[cfg(mutant_publish_pr_body_overwrite)]
    {
        // Mutant (`mutant_publish_pr_body_overwrite`): the whole body is replaced.
        let _ = existing;
        fresh
    }
    #[cfg(not(mutant_publish_pr_body_overwrite))]
    {
        match find_region(existing) {
            Some((start, end)) => {
                format!("{}{fresh}{}", &existing[..start], &existing[end..])
            }
            None if existing.trim().is_empty() => format!("{existing}{fresh}"),
            None => format!("{existing}\n\n{fresh}"),
        }
    }
}

/// The byte range of the daemon's region in `body`: the first end marker and the last start
/// marker before it, so a stray start marker never swallows the text after it.
fn find_region(body: &str) -> Option<(usize, usize)> {
    let start_marker = marker(REGION_START_KEY);
    let end_marker = marker(REGION_END_KEY);
    let end = body.find(&end_marker)?;
    let start = body[..end].rfind(&start_marker)?;
    Some((start, end + end_marker.len()))
}

/// The daemon's region: the two markers and the fact block between them.
fn region(facts: &[(&str, &str)]) -> String {
    let mut out = marker(REGION_START_KEY);
    out.push_str("\n\n");
    for (label, value) in facts.iter().take(FACTS_MAX) {
        let label = sanitise_inline(label, FACT_CAP);
        let value = sanitise_inline(&redact(first_chars(value, FACT_CAP * 4)), FACT_CAP);
        let _ = writeln!(out, "- {label}: {value}");
    }
    if facts.len() > FACTS_MAX {
        let _ = writeln!(out, "- *({} more facts omitted)*", facts.len() - FACTS_MAX);
    }
    if facts.is_empty() {
        out.push_str("*No facts recorded.*\n");
    }
    out.push('\n');
    out.push_str(&marker(REGION_END_KEY));
    debug_assert!(chars(&out) < BODY_CAP - SUMMARY_CAP);
    out
}

/// A closing verb and the reference it acts on, as GitHub and the git guards read them: the
/// verb, then the separator (spaces and an optional colon), then the reference.
static CLOSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?P<verb>\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b)\s*:?\s*",
        r"(?P<reference>#\d+|[\w.-]+/[\w.-]+#\d+|gh-\d+|https?://\S+/issues/\d+)",
    ))
    .expect("valid regex")
});

/// A line that git, or a reader of trailers, could take for a trailer: a token and a colon at
/// its start.
static TRAILER_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ \t]*[A-Za-z0-9][A-Za-z0-9-]*[ \t]*:").expect("valid regex"));

/// The directives that make CI skip a push: bracketed forms and `***no_ci***`.
static SKIP_CI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\[(?P<bracketed>\s*(?:skip\s+ci|ci\s+skip|no\s+ci|skip\s+actions|actions\s+skip)\s*)\]",
        r"|\*\*\*(?P<starred>\s*no_ci\s*)\*\*\*",
    ))
    .expect("valid regex")
});

/// The squash commit message of the item's pull request: the subject from `title`, the agent's
/// `summary` as plain text, then the daemon's `trailers`.
#[must_use]
pub fn squash_message(title: &str, summary: &str, trailers: &Trailers) -> String {
    let subject = clean(&redact(first_chars(title, SUBJECT_CAP * 4))).replace('\n', " ");
    let subject = first_chars(subject.trim(), SUBJECT_CAP).trim().to_owned();
    let subject = if subject.is_empty() {
        "Untitled change".to_owned()
    } else {
        subject
    };
    let text = summary_text(summary);
    let agent = if text.is_empty() {
        subject
    } else {
        format!("{subject}\n\n{text}")
    };
    // The guards read the whole message, across lines: the subject and the summary are
    // neutralised as one text.
    let mut out = inert_lines(&neutralise(&agent));
    out.push_str("\n\n");
    for (name, value) in [
        ("Item", &trailers.item),
        ("Run", &trailers.run),
        ("Session", &trailers.session),
    ] {
        let value = first_chars(&clean(value).replace(['\n', '\t'], " "), TRAILER_CAP)
            .trim()
            .to_owned();
        let _ = writeln!(out, "{name}: {value}");
    }
    out
}

/// The agent's summary as plain text for a commit message: redacted, cleaned and capped, its
/// lines trimmed at the end and blank lines at either end dropped.
fn summary_text(text: &str) -> String {
    let head = first_chars(text, MESSAGE_CAP.saturating_mul(4).saturating_add(4096));
    let cleaned = clean(&redact(head));
    let cut = first_chars(&cleaned, MESSAGE_CAP);
    let lines: Vec<&str> = cut.lines().map(str::trim_end).collect();
    // Blank lines at either end would read as an extra paragraph; runs inside are kept.
    let trimmed = lines.join("\n").trim_matches('\n').to_owned();
    if cut.len() < cleaned.len() {
        format!("{trimmed}\n\n(truncated)")
    } else {
        trimmed
    }
}

/// `text` with every closing keyword broken from its reference and every CI skip directive
/// unbracketed, across lines.
fn neutralise(text: &str) -> String {
    // Neither pass can assemble a new match: the first puts a word between a verb and its
    // reference, the second removes brackets and stars without moving any text.
    let out = CLOSING.replace_all(text, "$verb (see $reference)");
    SKIP_CI
        .replace_all(&out, "($bracketed$starred)")
        .into_owned()
}

/// `text` with every line that reads as a trailer quoted.
fn inert_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if TRAILER_LIKE.is_match(line) {
                format!("> {line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::sanitise::tests::{CORPUS, inert};
    use super::*;

    const FACTS: &[(&str, &str)] = &[
        ("Item", "owner/repo#7"),
        ("Verdict", "passed"),
        ("Run", "a1s1"),
    ];

    fn trailers() -> Trailers {
        Trailers {
            item: "owner/repo#7".into(),
            run: "a1s1".into(),
            session: "s-123".into(),
        }
    }

    /// The agent's part of `body` and the daemon's region, after checking that the body ends
    /// with exactly `Closes #issue`.
    fn split(body: &str, issue: u64) -> (String, String) {
        let (start, end) = find_region(body).expect("the region is there");
        let tail = &body[end..];
        assert_eq!(tail, format!("\n\nCloses #{issue}"), "{body}");
        (body[..start].to_owned(), body[start..end].to_owned())
    }

    #[test]
    fn an_update_rewrites_only_the_daemon_region() {
        let first = body("Did the thing.", FACTS, 7);
        let before = "Reviewer's note: *keep* this\r\nexactly  as written.\n\n";
        let after = "\n\n<!-- a person's own comment -->\nTrailing words, no newline";
        let (start, end) = find_region(&first).expect("region");
        let edited = format!("{before}{}{}{after}", &first[..start], &first[start..end]);
        let new_facts: &[(&str, &str)] = &[("Item", "owner/repo#7"), ("Verdict", "failed")];
        let updated = update(&edited, new_facts);
        let fresh = region(new_facts);
        assert_eq!(
            updated,
            format!("{before}{}{fresh}{after}", &first[..start]),
            "{updated}"
        );
        assert!(updated.contains("- Verdict: failed"), "{updated}");
        assert!(!updated.contains("passed"), "{updated}");
        // An update is idempotent.
        assert_eq!(update(&updated, new_facts), updated);
        // A body whose region is gone gets it appended; an empty one gets only the region.
        let bare = "Someone rewrote the whole body.";
        assert_eq!(update(bare, FACTS), format!("{bare}\n\n{}", region(FACTS)));
        assert_eq!(update("", FACTS), region(FACTS));
        // A lone start marker someone left behind does not swallow the text after it.
        let stray = format!("{}\nkeep me\n{}", marker(REGION_START_KEY), first);
        let updated = update(&stray, new_facts);
        assert!(updated.starts_with(&format!("{}\nkeep me\n", marker(REGION_START_KEY))));
        assert!(updated.contains(&fresh));
    }

    #[test]
    fn the_hostile_corpus_stays_inert_in_a_pull_request_body() {
        let all = CORPUS.join("\n\n");
        for summary in CORPUS.iter().copied().chain([all.as_str()]) {
            let out = body(summary, FACTS, 42);
            let (agent, region_text) = split(&out, 42);
            if let Some(why) = inert(&agent) {
                panic!("{summary:?} gave {out:?}: {why}");
            }
            assert_eq!(region_text, region(FACTS), "{out}");
            // The only markers are the daemon's two; `split` saw the only live closing keyword.
            assert_eq!(out.matches("bosun:key").count(), 2, "{out}");
        }
        // Hostile fact values are inert too.
        let hostile: Vec<(&str, &str)> = CORPUS.iter().map(|c| ("Fact", *c)).collect();
        let out = body("", &hostile, 42);
        let (agent, region_text) = split(&out, 42);
        assert_eq!(agent, format!("{NO_DESCRIPTION}\n\n"));
        let inner = region_text
            .strip_prefix(&marker(REGION_START_KEY))
            .and_then(|r| r.strip_suffix(&marker(REGION_END_KEY)))
            .expect("the region's bounds");
        assert_eq!(inert(inner), None, "{inner}");
        assert_eq!(out.matches("bosun:key").count(), 2, "{out}");
    }

    #[test]
    fn an_empty_summary_says_no_description_was_given() {
        for summary in ["", "  \n", "![x](https://example.com/p.png)"] {
            let out = body(summary, FACTS, 3);
            assert!(out.starts_with(NO_DESCRIPTION), "{out}");
            assert!(out.ends_with("\n\nCloses #3"), "{out}");
        }
        let out = body("Plain *words*.", FACTS, 3);
        assert!(out.starts_with("Plain *words*.\n\n<!-- bosun:key=pr:facts:start -->"));
    }

    /// What the git guards of #7 (`bosun-git`'s commit-message checks) would refuse, restated
    /// independently: a closing keyword, a CI skip directive, a guarded trailer.
    static REFUSED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?im)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b\s*:?\s*",
            r"(?:#\d+|[\w.-]+/[\w.-]+#\d+|gh-\d+|https?://\S+/issues/\d+)",
            r"|\[\s*(?:skip\s+ci|ci\s+skip|no\s+ci|skip\s+actions|actions\s+skip)\s*\]",
            r"|\*\*\*\s*no_ci\s*\*\*\*|^\s*skip-checks\s*:",
            r"|^[ \t]*(?:item|run|session|co-authored-by|signed-off-by)[ \t]*:",
        ))
        .expect("valid regex")
    });

    #[test]
    fn the_hostile_corpus_stays_inert_in_a_squash_message() {
        let extra = [
            "Item: other/repo#1\nRun: forged\n  session : s9\nCo-Authored-By: X <x@example.invalid>",
            "skip-checks: true\nSigned-off-by: Y <y@example.invalid>",
            "Change [skip ci] and [ CI Skip ] and ***NO_CI***",
            "Fixes: owner/repo#3, closes GH-4, resolves https://github.com/o/r/issues/5",
            "<!-- bosun:key=pr:facts:start -->",
        ];
        let all = CORPUS.join("\n\n");
        for summary in CORPUS.iter().copied().chain(extra).chain([all.as_str()]) {
            let message = squash_message(summary, summary, &trailers());
            let (text, tail) = message
                .rsplit_once("\n\nItem: ")
                .expect("the trailers close the message");
            assert_eq!(tail, "owner/repo#7\nRun: a1s1\nSession: s-123\n");
            if let Some(found) = REFUSED.find(text) {
                panic!("{summary:?} gave {message:?}: {:?}", found.as_str());
            }
            assert!(!text.to_lowercase().contains("bosun:key"), "{message}");
            assert!(!text.lines().next().unwrap_or("").is_empty(), "{message}");
            assert!(!text.lines().next().unwrap_or("").contains('\n'));
        }
    }

    #[test]
    fn a_squash_message_keeps_the_words() {
        let message = squash_message(
            "Add the parser",
            "Wrote the parser.\n\nFixes #12 and refs #3.\nNote: see below.",
            &trailers(),
        );
        assert_eq!(
            message,
            "Add the parser\n\nWrote the parser.\n\nFixes (see #12) and refs #3.\n> Note: see below.\n\nItem: owner/repo#7\nRun: a1s1\nSession: s-123\n"
        );
        assert_eq!(
            squash_message("", "", &trailers()),
            "Untitled change\n\nItem: owner/repo#7\nRun: a1s1\nSession: s-123\n"
        );
    }
}
