//! Publication of text the agent influenced (#5, #8): the allowlist sanitiser, the secret
//! redactor and the renderer of the sticky item report.
//!
//! GitHub acts on text: a closing verb closes an issue, a reference creates a cross-reference
//! event in another repository, a mention notifies, a bot command runs under the user's login.
//! Everything the agent influenced (its summary, verdict output) therefore passes
//! [`redact`] and then [`sanitise`] (Markdown) or [`sanitise_inline`] (literal text) before it
//! reaches a comment, and verdict tails go into code fences that outrun every backtick run in
//! them. [`render_report`] composes those pieces into the body of the sticky comment, under
//! GitHub's 65,536-character cap ([`BODY_CAP`]).
//!
//! The daemon's own comments carry a hidden key ([`marker`]) on their first line: the backend
//! finds a comment to edit by it (#5) and the prompt builder skips the daemon's comments by it
//! (#2). The sanitiser strips every imitation of it from agent text.
//!
//! [`pr`] builds the texts of the item's pull request: its body, the update of an edited body,
//! the squash message.

use std::fmt::Write as _;

pub mod pr;
mod redact;
mod report;
mod sanitise;
mod text;

pub use redact::redact;
pub use report::{
    AgentSummary, AttemptFacts, BODY_CAP, Bill, DiffStat, Landing, LandingMode, ParkedFacts,
    ReportFacts, ReportOptions, VerdictFacts, render_report, report_key,
};
pub use sanitise::{sanitise, sanitise_inline};

/// The opening of the hidden key marker; the key follows, then [`MARKER_CLOSE`].
const MARKER_OPEN: &str = "<!-- bosun:key=";
/// The end of the hidden key marker.
const MARKER_CLOSE: &str = " -->";

/// The hidden key marker for `key`: an HTML comment, invisible on GitHub, that names the
/// comment for idempotent writes (#5) and marks it as the daemon's (#2).
///
/// Characters outside `[A-Za-z0-9._:/#]` are percent-encoded, so no key can close the comment
/// early and two different keys never give the same marker.
#[must_use]
pub fn marker(key: &str) -> String {
    let mut out = String::from(MARKER_OPEN);
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || b"._:/#".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out.push_str(MARKER_CLOSE);
    out
}

/// The key of the hidden marker on the body's first line, as [`marker`] encoded it; `None`
/// when the first line is not a marker.
///
/// Only the first line is read: a marker imitation further down (quoted text, say) never
/// makes a comment the daemon's.
#[must_use]
pub fn find_key(body: &str) -> Option<&str> {
    let first = body.lines().next()?;
    let key = first
        .strip_prefix(MARKER_OPEN)?
        .strip_suffix(MARKER_CLOSE)?;
    let valid = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/#%".contains(&b));
    valid.then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_round_trips_through_find_key() {
        let m = marker("report:owner/repo#12");
        assert_eq!(m, "<!-- bosun:key=report:owner/repo#12 -->");
        assert_eq!(
            find_key(&format!("{m}\nbody")),
            Some("report:owner/repo#12")
        );
    }

    #[test]
    fn a_key_cannot_close_the_comment_early() {
        let m = marker("a --> <img src=x> b");
        assert_eq!(m.matches("-->").count(), 1, "{m}");
        assert!(m.ends_with(" -->"));
        assert_ne!(marker("a b"), marker("a_b"));
        assert_eq!(
            find_key(&m),
            Some("a%20%2D%2D%3E%20%3Cimg%20src%3Dx%3E%20b")
        );
    }

    #[test]
    fn only_the_first_line_is_a_marker() {
        let body = format!("hello\n{}", marker("report:1"));
        assert_eq!(find_key(&body), None);
        assert_eq!(find_key(""), None);
        assert_eq!(find_key("<!-- bosun:key= -->"), None);
    }
}
