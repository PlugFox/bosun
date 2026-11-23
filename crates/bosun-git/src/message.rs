//! The commit-message checks of the verdict preconditions (#7 "Verdict"): closing keywords, CI
//! skip directives and forged trailers.
//!
//! Every check is case-insensitive and reads the whole message, not only the paragraph git
//! would parse as trailers: GitHub, CI services and git (whose trailer parsing a repository's
//! configuration can change) each read messages their own way, so a line any of them might act
//! on counts. Blocking a commit wrongly costs one attempt; letting one through can close an
//! issue, skip CI or attribute work under a forged run.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::Reason;

/// GitHub's closing keywords followed by an issue reference: `#N`, `owner/repo#N`, `GH-N` or
/// an issue URL, with an optional colon after the keyword.
static CLOSING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b\s*:?\s*(?:#\d+|[\w.-]+/[\w.-]+#\d+|gh-\d+|https?://\S+/issues/\d+)",
    )
    .expect("the closing-keyword pattern compiles")
});

/// The directives that make GitHub Actions (and most CI services) skip a push or a pull
/// request, and the `skip-checks: true` trailer.
static SKIP_CI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)\[\s*(?:skip\s+ci|ci\s+skip|no\s+ci|skip\s+actions|actions\s+skip)\s*\]|\*\*\*\s*no_ci\s*\*\*\*|^\s*skip-checks\s*:\s*true\b",
    )
    .expect("the skip-ci pattern compiles")
});

/// A line that reads as one of the guarded trailers.
static TRAILER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^[ \t]*(item|run|session|co-authored-by)[ \t]*:[ \t]*(.*?)[ \t]*\r?$")
        .expect("the trailer pattern compiles")
});

/// The trailer values a commit may carry: the item, run and session the daemon writes, and the
/// co-authors the provider adds. A trailer whose value is not in its set is forged; an empty
/// set means any trailer of that name is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// Values of `Item:`.
    pub item: BTreeSet<String>,
    /// Values of `Run:`.
    pub run: BTreeSet<String>,
    /// Values of `Session:`.
    pub session: BTreeSet<String>,
    /// Values of `Co-authored-by:`.
    pub co_authored_by: BTreeSet<String>,
}

impl Expected {
    fn allows(&self, key: &str, value: &str) -> bool {
        let set = match key.to_ascii_lowercase().as_str() {
            "item" => &self.item,
            "run" => &self.run,
            "session" => &self.session,
            _ => &self.co_authored_by,
        };
        set.contains(value)
    }
}

/// The problems of the message `body` of commit `sha`, in the order of the checks.
pub(crate) fn problems(sha: &str, body: &str, expected: &Expected) -> Vec<Reason> {
    let mut reasons = Vec::new();
    for found in CLOSING.find_iter(body) {
        reasons.push(Reason::ClosingKeyword {
            sha: sha.to_owned(),
            text: found.as_str().to_owned(),
        });
    }
    for found in SKIP_CI.find_iter(body) {
        reasons.push(Reason::SkipCi {
            sha: sha.to_owned(),
            text: found.as_str().trim().to_owned(),
        });
    }
    for (key, value) in trailers(body) {
        if !expected.allows(&key, &value) {
            reasons.push(Reason::ForgedTrailer {
                sha: sha.to_owned(),
                key,
                value,
            });
        }
    }
    reasons
}

/// Every line of `body` that reads as a guarded trailer: its name as written and its value.
pub(crate) fn trailers(body: &str) -> Vec<(String, String)> {
    TRAILER
        .captures_iter(body)
        .map(|captures| {
            let (_, [key, value]) = captures.extract();
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(body: &str, expected: &Expected) -> Vec<&'static str> {
        problems("c0ffee", body, expected)
            .iter()
            .map(|r| match r {
                Reason::ClosingKeyword { .. } => "close",
                Reason::SkipCi { .. } => "skip",
                Reason::ForgedTrailer { .. } => "trailer",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn closing_keywords_in_every_form() {
        let none = Expected::default();
        for body in [
            "Closes #1",
            "closes #1",
            "CLOSES #1",
            "close #12",
            "closed #12",
            "Fix #3",
            "fixes #3",
            "FIXED #3",
            "resolve #4",
            "Resolves: #4",
            "resolved #4",
            "This fixes owner/repo#7.",
            "fixes some-org/some.repo#7",
            "closes GH-9",
            "Fixes https://github.com/owner/repo/issues/10",
            "Subject\n\nBody text.\n\nFixes #2\n",
        ] {
            assert_eq!(kinds(body, &none), ["close"], "{body:?}");
        }
    }

    #[test]
    fn issue_references_without_a_keyword_pass() {
        let none = Expected::default();
        for body in [
            "Refs #1",
            "Part of #1",
            "prefixes #1",
            "Fix the parser",
            "fixes a crash in #frame handling",
            "See https://github.com/owner/repo/pull/3",
        ] {
            assert_eq!(kinds(body, &none), Vec::<&str>::new(), "{body:?}");
        }
    }

    #[test]
    fn skip_ci_directives_in_every_form() {
        let none = Expected::default();
        for body in [
            "Change [skip ci]",
            "Change [SKIP CI]",
            "[ci skip] change",
            "[No Ci]",
            "[skip actions]",
            "[Actions Skip]",
            "***NO_CI***",
            "***no_ci***",
            "Subject\n\nskip-checks: true\n",
            "Subject\n\nSkip-Checks:TRUE\n",
        ] {
            assert_eq!(kinds(body, &none), ["skip"], "{body:?}");
        }
        assert_eq!(kinds("skip CI later, maybe", &none), Vec::<&str>::new());
    }

    #[test]
    fn trailers_are_forged_unless_expected() {
        let expected = Expected {
            item: ["owner/repo#53".to_owned()].into(),
            run: ["r1".to_owned()].into(),
            session: BTreeSet::new(),
            co_authored_by: ["Bot <bot@example.invalid>".to_owned()].into(),
        };
        let good =
            "Subject\n\nItem: owner/repo#53\nRun: r1\nCo-authored-by: Bot <bot@example.invalid>\n";
        assert_eq!(kinds(good, &expected), Vec::<&str>::new());
        // Any letter case of the name, CRLF line ends, spaces before the colon.
        for body in [
            "Subject\n\nItem: other#1\n",
            "Subject\n\nITEM: other#1\n",
            "Subject\r\n\r\nitem : other#1\r\n",
            "Subject\n\nRun: r2\n",
            "Subject\n\nSession: s1\n",
            "Subject\n\nCo-Authored-By: Someone <someone@example.invalid>\n",
        ] {
            assert_eq!(kinds(body, &expected), ["trailer"], "{body:?}");
        }
        // An empty expected set forges every trailer of that name.
        assert_eq!(
            kinds("S\n\nItem: owner/repo#53\n", &Expected::default()),
            ["trailer"]
        );
        // Other trailers are not guarded.
        assert_eq!(
            kinds("S\n\nSigned-off-by: A <a@example.invalid>\n", &expected),
            Vec::<&str>::new()
        );
    }
}
