//! The texts of a pull request and of its squash commit (#8, stage 2): a pure function from the
//! agent's summary and the daemon's facts to the body of the pull request and to the message of
//! the landing commit. Nothing here opens a pull request or writes a commit; the landing does.
//!
//! - A pull request body is Markdown that GitHub acts on, so the agent's summary is redacted and
//!   sanitised ([`super::redact`], [`super::sanitise`]) like every other text the agent
//!   influenced. The daemon's own text follows it: a region of facts between two hidden markers
//!   ([`body`]) and the `Closes #N` of the item's issue, which is the daemon's reference and
//!   sits outside the sanitised text.
//! - People edit pull request bodies, so [`update`] rewrites only the daemon's region and keeps
//!   every other byte.
//! - A commit message is plain text, not Markdown, so the summary in it is defanged instead:
//!   the closing keywords, skip-CI directives and trailer lines that the commit-message checks
//!   of `bosun-git` refuse are broken up, and the trailers the daemon writes come last.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::text::{clean, first_chars};
use super::{marker, redact, sanitise, sanitise_inline};

/// The summary's cap in a pull request body, in characters.
const SUMMARY_CAP: usize = 16_000;
/// One fact of the facts block, in characters.
const FACT_CAP: usize = 1_000;
/// The commit subject, in characters.
const SUBJECT_CAP: usize = 256;
/// What a pull request body says in place of an empty summary.
const NO_SUMMARY: &str = "No description was given.";

/// The hidden marker that opens the daemon's region of a pull request body.
fn begin_marker() -> String {
    marker("pr:facts:begin")
}

/// The hidden marker that closes the daemon's region of a pull request body.
fn end_marker() -> String {
    marker("pr:facts:end")
}

/// A closing keyword and the reference it acts on, as GitHub and the commit-message checks of
/// `bosun-git` read them.
static CLOSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?P<verb>\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b\s*:?\s*)(?P<reference>#\d+|[\w.-]+/[\w.-]+#\d+|gh-\d+|https?://\S+/issues/\d+)",
    )
    .expect("the closing-keyword pattern compiles")
});

/// The directives that make CI services skip a push or a pull request.
static SKIP_CI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\[\s*(?:skip\s+ci|ci\s+skip|no\s+ci|skip\s+actions|actions\s+skip)\s*\]|\*\*\*\s*no_ci\s*\*\*\*",
    )
    .expect("the skip-ci pattern compiles")
});

/// A line that reads as a trailer, or as the skip-checks trailer.
static TRAILER_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[ \t]*(?:item|run|session|co-authored-by|skip-checks)[ \t]*:")
        .expect("the trailer-line pattern compiles")
});

/// The trailers of a landing commit (#8): the item, the run and the session it came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Trailers {
    /// `Item:`, the item's key.
    pub item: String,
    /// `Run:`, the run's id.
    pub run: String,
    /// `Session:`, the session's id.
    pub session: String,
}

/// The body of a pull request: the agent's summary (redacted and sanitised, or a line saying
/// none was given), the daemon's region of `facts`, and `Closes #issue`.
///
/// `facts` are the daemon's own lines, one fact each; each is sanitised too, so none of them can
/// act on GitHub either.
#[must_use]
pub fn body(summary: &str, facts: &[String], issue: u64) -> String {
    let mut out = summary_section(summary);
    out.push_str("\n\n");
    out.push_str(&region(facts));
    let _ = write!(out, "\n\nCloses #{issue}");
    out
}

/// The body `existing` (a pull request body people may have edited) with its daemon region
/// rewritten to `facts`. Everything outside the region is kept byte for byte. A body whose
/// region is missing gets one appended.
#[must_use]
pub fn update(existing: &str, facts: &[String]) -> String {
    // Mutant (`mutant_publish_pr_body_overwrite`): the body is replaced by the region alone, so
    // the text people wrote around it is lost on every update.
    #[cfg(mutant_publish_pr_body_overwrite)]
    {
        let _ = existing;
        region(facts)
    }
    #[cfg(not(mutant_publish_pr_body_overwrite))]
    {
        let fresh = region(facts);
        // The last opening marker: a dangling one left by an earlier edit is never the region.
        let Some(start) = existing.rfind(&begin_marker()) else {
            return append(existing, &fresh);
        };
        let Some(close) = existing[start..].find(&end_marker()) else {
            return append(existing, &fresh);
        };
        let stop = start + close + end_marker().len();
        format!("{}{fresh}{}", &existing[..start], &existing[stop..])
    }
}

/// The message of a landing commit (#8): the title as the subject, the summary as plain text,
/// then the `Item:`, `Run:` and `Session:` trailers.
///
/// The commit-message checks of `bosun-git` refuse closing keywords, skip-CI directives and
/// trailers the daemon did not write; the summary and the title are defanged so that none of
/// them survives as one.
#[must_use]
pub fn squash_message(title: &str, summary: &str, trailers: &Trailers) -> String {
    let mut paragraphs = Vec::new();
    let subject = subject(title);
    if !subject.is_empty() {
        paragraphs.push(subject);
    }
    let text = plain(summary);
    if !text.is_empty() {
        paragraphs.push(text);
    }
    paragraphs.push(format!(
        "Item: {}\nRun: {}\nSession: {}",
        one_line(&trailers.item),
        one_line(&trailers.run),
        one_line(&trailers.session),
    ));
    let mut out = paragraphs.join("\n\n");
    out.push('\n');
    out
}

/// The summary section of a body: sanitised, or a line saying none was given.
fn summary_section(summary: &str) -> String {
    // Mutant (`mutant_publish_pr_raw_summary`): the summary reaches the body as the agent wrote
    // it, so its HTML, images, closing keywords and marker imitations act on GitHub.
    #[cfg(mutant_publish_pr_raw_summary)]
    let text = summary.to_owned();
    #[cfg(not(mutant_publish_pr_raw_summary))]
    let text = sanitise(&redact(summary), SUMMARY_CAP);
    if text.trim().is_empty() {
        NO_SUMMARY.to_owned()
    } else {
        text
    }
}

/// The daemon's region: the opening marker, one bullet per fact, the closing marker.
fn region(facts: &[String]) -> String {
    let mut out = begin_marker();
    for fact in facts {
        out.push_str("\n- ");
        out.push_str(&sanitise_inline(&redact(fact), FACT_CAP));
    }
    out.push('\n');
    out.push_str(&end_marker());
    out
}

/// `existing` with `region` after it, separated by a blank line.
#[cfg_attr(mutant_publish_pr_body_overwrite, allow(dead_code))]
fn append(existing: &str, region: &str) -> String {
    let separator = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{existing}{separator}{region}")
}

/// The commit subject: the title on one line, defanged.
fn subject(title: &str) -> String {
    let one = clean(title)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    defang(first_chars(&one, SUBJECT_CAP))
}

/// The summary as plain text for a commit message: cleaned, redacted, capped and defanged.
fn plain(summary: &str) -> String {
    let text = redact(&clean(summary));
    defang(first_chars(&text, SUMMARY_CAP).trim_end())
        .trim()
        .to_owned()
}

/// A trailer value on one line, defanged.
fn one_line(value: &str) -> String {
    defang_inline(&clean(value).replace('\n', " "))
        .trim()
        .to_owned()
}

/// `text` defanged line by line: a line that reads as a trailer is quoted, so git reads it as
/// text.
fn defang(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let line = defang_inline(line);
            if TRAILER_LINE.is_match(&line) {
                format!("> {line}")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` with its skip-CI directives and closing keywords broken up. A directive loses its
/// brackets and asterisks; a closing keyword's reference becomes a code span, which no keyword
/// is followed by.
fn defang_inline(text: &str) -> String {
    let text = SKIP_CI.replace_all(text, |caps: &Captures<'_>| {
        caps[0].replace(['[', ']', '*'], "")
    });
    CLOSING
        .replace_all(&text, "${verb}`${reference}`")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use regex::Regex;

    use super::*;
    use crate::publish::sanitise::tests::{CORPUS, inert};

    fn facts() -> Vec<String> {
        vec!["Verdict: `cargo test`".to_owned(), "Attempts: 1".to_owned()]
    }

    fn trailers() -> Trailers {
        Trailers {
            item: "owner/repo#8".to_owned(),
            run: "r1".to_owned(),
            session: "s1".to_owned(),
        }
    }

    /// The refusals `bosun-git`'s commit-message checks would make of `message`. Mirrors
    /// `bosun-git::message`, which bosun-core cannot depend on.
    static REFUSED_CLOSING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b\s*:?\s*(?:#\d+|[\w.-]+/[\w.-]+#\d+|gh-\d+|https?://\S+/issues/\d+)",
        )
        .expect("valid regex")
    });
    static REFUSED_SKIP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)\[\s*(?:skip\s+ci|ci\s+skip|no\s+ci|skip\s+actions|actions\s+skip)\s*\]|\*\*\*\s*no_ci\s*\*\*\*|^\s*skip-checks\s*:\s*true\b",
        )
        .expect("valid regex")
    });
    static REFUSED_TRAILER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^[ \t]*(item|run|session|co-authored-by)[ \t]*:[ \t]*(.*?)[ \t]*\r?$")
            .expect("valid regex")
    });

    fn refusals(message: &str) -> Vec<String> {
        let mut out: Vec<String> = REFUSED_CLOSING
            .find_iter(message)
            .map(|m| m.as_str().to_owned())
            .collect();
        out.extend(
            REFUSED_SKIP
                .find_iter(message)
                .map(|m| m.as_str().to_owned()),
        );
        for caps in REFUSED_TRAILER.captures_iter(message) {
            let key = caps[1].to_ascii_lowercase();
            let value = &caps[2];
            let allowed = match key.as_str() {
                "item" => value == "owner/repo#8",
                "run" => value == "r1",
                "session" => value == "s1",
                _ => false,
            };
            if !allowed {
                out.push(format!("{key}: {value}"));
            }
        }
        out
    }

    #[test]
    fn an_update_rewrites_only_the_daemon_region() {
        let before = "A person wrote this first.\r\n\n";
        let after = "\n\nAnd this after, with `code` and @someone.\n";
        let original = format!("{before}{}{after}", region(&facts()));
        let attempts = vec!["Verdict: `cargo test`".to_owned(), "Attempts: 2".to_owned()];
        assert_eq!(
            update(&original, &attempts),
            format!("{before}{}{after}", region(&attempts))
        );

        // The same holds for a body the daemon wrote and a person then edited.
        let written = body("Did it.", &facts(), 8);
        let edited = format!(
            "Reviewer note first.\n\n{}\n\nThanks.",
            written.replace("Did it.", "Did it, edited.")
        );
        let expected = format!(
            "Reviewer note first.\n\nDid it, edited.\n\n{}\n\nCloses #8\n\nThanks.",
            region(&attempts)
        );
        assert_eq!(update(&edited, &attempts), expected);
    }

    #[test]
    fn a_body_without_the_region_gets_it_appended() {
        assert_eq!(update("", &facts()), region(&facts()));
        assert_eq!(
            update("Written by hand.\n", &facts()),
            format!("Written by hand.\n\n{}", region(&facts()))
        );
        assert_eq!(
            update("Written by hand.", &facts()),
            format!("Written by hand.\n\n{}", region(&facts()))
        );
    }

    #[test]
    fn an_empty_summary_gives_the_facts_and_a_note() {
        assert_eq!(
            body("  \n\t", &facts(), 8),
            format!("{NO_SUMMARY}\n\n{}\n\nCloses #8", region(&facts()))
        );
        assert_eq!(
            body("\u{200B}", &facts(), 8),
            format!("{NO_SUMMARY}\n\n{}\n\nCloses #8", region(&facts()))
        );
    }

    #[test]
    fn the_hostile_corpus_stays_inert_in_a_pull_request_body() {
        for input in CORPUS {
            let published = body(input, &facts(), 8);
            assert_eq!(
                published.to_lowercase().matches("bosun:key").count(),
                2,
                "only the daemon's two markers may name the key: {published}"
            );
            let daemon_free = published
                .replace(&begin_marker(), "")
                .replace(&end_marker(), "");
            let agent_and_facts = daemon_free
                .strip_suffix("\n\nCloses #8")
                .unwrap_or_else(|| {
                    panic!("the body ends with the daemon's reference: {published}")
                });
            if let Some(why) = inert(agent_and_facts) {
                panic!("{input:?} gave {published:?}: {why}");
            }
        }
        let all = CORPUS.join("\n\n");
        let published = body(&all, &facts(), 8);
        let daemon_free = published
            .replace(&begin_marker(), "")
            .replace(&end_marker(), "");
        let agent_and_facts = daemon_free
            .strip_suffix("\n\nCloses #8")
            .expect("the reference");
        assert_eq!(inert(agent_and_facts), None, "{published}");
    }

    #[test]
    fn a_squash_message_is_the_title_the_summary_and_the_trailers() {
        assert_eq!(
            squash_message(
                "Add the lease model (#8)",
                "Did **this**.\n\n- one\n- two",
                &trailers()
            ),
            "Add the lease model (#8)\n\nDid **this**.\n\n- one\n- two\n\nItem: owner/repo#8\nRun: r1\nSession: s1\n"
        );
        assert_eq!(
            squash_message("Title", "", &trailers()),
            "Title\n\nItem: owner/repo#8\nRun: r1\nSession: s1\n"
        );
    }

    #[test]
    fn a_closing_keyword_or_trailer_in_the_summary_is_not_one() {
        let summary = "Fixes #3 and closes owner/repo#4.\n[skip ci] now\nItem: other#1\n  Session: s9\nSkip-Checks: true";
        let message = squash_message("Fixes #5", summary, &trailers());
        assert_eq!(refusals(&message), Vec::<String>::new(), "{message}");
        assert!(message.starts_with("Fixes `#5`\n\n"), "{message}");
        assert!(message.contains("Fixes `#3`"), "{message}");
        assert!(message.contains("closes `owner/repo#4`"), "{message}");
        assert!(message.contains("> Item: other#1"), "{message}");
        assert!(message.ends_with("\n\nItem: owner/repo#8\nRun: r1\nSession: s1\n"));
    }

    #[test]
    fn the_hostile_corpus_stays_inert_in_a_squash_message() {
        for input in CORPUS {
            let message = squash_message(input, input, &trailers());
            assert_eq!(
                refusals(&message),
                Vec::<String>::new(),
                "{input:?} gave {message:?}"
            );
        }
        let all = CORPUS.join("\n\n");
        let message = squash_message("Title", &all, &trailers());
        assert_eq!(refusals(&message), Vec::<String>::new(), "{message}");
    }
}
