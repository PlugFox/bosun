//! The text-level pieces of publication: invisible characters, hidden markers, the reference
//! wrapper, code spans and fences.

use std::sync::LazyLock;

use regex::Regex;

/// A character GitHub shows as nothing or uses to reorder text: zero-width characters,
/// bidirectional controls, Unicode tags (invisible "ASCII smuggling"), fillers, and the
/// control characters other than newline and tab.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    ) || (c.is_control() && c != '\n' && c != '\t')
}

/// `s` with line endings normalised to `\n` and every invisible character removed.
pub(super) fn strip_invisible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() != Some(&'\n') {
                out.push('\n');
            }
        } else if !is_invisible(c) {
            out.push(c);
        }
    }
    out
}

/// A whole hidden marker, or anything shaped like one.
static MARKER_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<!--\s*bosun\s*:\s*key.*?-->").expect("valid regex"));
/// The marker's token alone, wherever it appears.
static MARKER_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bosun\s*:\s*key").expect("valid regex"));

/// `s` with every hidden-marker imitation removed, to a fixed point (removing one occurrence
/// cannot assemble another).
pub(super) fn strip_markers(s: &str) -> String {
    let mut current = s.to_owned();
    loop {
        let once = MARKER_COMMENT.replace_all(&current, "");
        let once = MARKER_TOKEN.replace_all(&once, "").into_owned();
        if once == current {
            return current;
        }
        current = once;
    }
}

/// Invisible characters and hidden markers removed: the first step for any agent text.
pub(super) fn clean(s: &str) -> String {
    strip_markers(&strip_invisible(s))
}

/// What GitHub acts on in text, one named group per kind. The closing verb comes first so a
/// verb and its reference become one code span.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?im)",
        // The nine closing verbs, with an optional colon, and the reference they act on.
        r"(?P<verb>\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)[ \t]*:?[ \t]*",
        r"(?:https?://[^\s<>`]+|(?:[a-z0-9][a-z0-9-]*/[a-z0-9._-]+)?#[0-9]+|gh-[0-9]+))",
        // Full URLs, issue and pull request links among them.
        r"|(?P<url>(?:https?://|www\.)[^\s<>`]+)",
        // owner/repo#N and owner/repo@sha.
        r"|(?P<xref>[a-z0-9][a-z0-9-]*/[a-z0-9._-]+(?:#[0-9]+|@[0-9a-f]{7,40}))",
        r"|(?P<gh>\bgh-[0-9]+)",
        r"|(?P<num>#[0-9]+)",
        // @user and @org/team.
        r"|(?P<at>@[a-z0-9][a-z0-9_-]*(?:/[a-z0-9_.-]+)?)",
        // Slash commands: a slash word at the start of a line or after whitespace.
        r"|(?:^|\s)(?P<slash>/[a-z][a-z0-9_-]*)",
        // bors and homu commands.
        r"|(?P<bors>\bbors[ \t]+\S+)",
    ))
    .expect("valid regex")
});

/// A piece of agent text: literal text, or something GitHub would act on, to be wrapped in a
/// code span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Segment {
    /// Text to escape.
    Text(String),
    /// A reference, mention, URL or command: emitted as a code span.
    Code(String),
}

/// `text` split into literal text and the spans GitHub would act on.
pub(super) fn segments(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut literal_from = 0;
    while pos < text.len() {
        let Some(caps) = REFERENCE.captures_at(text, pos) else {
            break;
        };
        let group = ["verb", "url", "xref", "gh", "num", "at", "slash", "bors"]
            .iter()
            .find_map(|name| caps.name(name).map(|m| (*name, m)));
        let Some((name, m)) = group else { break };
        let mut end = m.end();
        if matches!(name, "verb" | "url") {
            // Sentence punctuation after a URL is not part of it.
            let trimmed = m
                .as_str()
                .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
            end = m.start() + trimmed.len();
        }
        if end <= m.start() {
            // Nothing left after trimming: step past this character.
            pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if literal_from < m.start() {
            out.push(Segment::Text(text[literal_from..m.start()].to_owned()));
        }
        out.push(Segment::Code(text[m.start()..end].to_owned()));
        literal_from = end;
        pos = end;
    }
    if literal_from < text.len() {
        out.push(Segment::Text(text[literal_from..].to_owned()));
    }
    out
}

/// The longest run of backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// The length of a backtick fence around `content`: longer than any backtick run in it, and at
/// least `min`.
pub(super) fn fence_len(content: &str, min: usize) -> usize {
    #[cfg(mutant_publish_short_fence)]
    {
        let _ = content;
        min
    }
    #[cfg(not(mutant_publish_short_fence))]
    {
        min.max(longest_backtick_run(content) + 1)
    }
}

/// `content` as an inline code span: cleaned, on one line, fenced by more backticks than it
/// holds. Empty content gives an empty string.
pub(super) fn code_span(content: &str) -> String {
    let content = clean(content).replace('\n', " ");
    if content.is_empty() {
        return String::new();
    }
    let fence = "`".repeat(fence_len(&content, 1));
    let all_spaces = content.chars().all(|c| c == ' ');
    let pad = content.starts_with('`')
        || content.ends_with('`')
        || (content.starts_with(' ') && content.ends_with(' ') && !all_spaces);
    if pad {
        format!("{fence} {content} {fence}")
    } else {
        format!("{fence}{content}{fence}")
    }
}

/// A fenced code block of `content` (already cleaned), every line after `prefix`, the fence
/// longer than any backtick run in the content. Ends without a newline.
pub(super) fn code_block(content: &str, info: &str, prefix: &str) -> String {
    let fence = "`".repeat(fence_len(content, 3));
    let mut out = format!("{fence}{info}\n");
    for line in content.strip_suffix('\n').unwrap_or(content).split('\n') {
        if !line.is_empty() {
            out.push_str(prefix);
            out.push_str(line);
        }
        out.push('\n');
    }
    out.push_str(prefix);
    out.push_str(&fence);
    out
}

/// The number of characters in `s`: GitHub's cap counts characters, not bytes.
pub(super) fn chars(s: &str) -> usize {
    s.chars().count()
}

/// The first `n` characters of `s`.
pub(super) fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(text: &str) -> Vec<String> {
        segments(text)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Code(c) => Some(c),
                Segment::Text(_) => None,
            })
            .collect()
    }

    #[test]
    fn invisible_characters_are_removed() {
        assert_eq!(
            strip_invisible("a\u{200B}b\u{202E}c\u{2066}d\u{FEFF}e\u{E0041}f\u{7}g"),
            "abcdefg"
        );
        assert_eq!(strip_invisible("a\r\nb\rc\td"), "a\nb\nc\td");
    }

    #[test]
    fn marker_imitations_are_removed_to_a_fixed_point() {
        assert_eq!(strip_markers("a <!-- bosun:key=x --> b"), "a  b");
        assert_eq!(strip_markers("BOSUN : KEY"), "");
        assert_eq!(strip_markers("bosun:bosun:keykey="), "=");
    }

    #[test]
    fn every_closing_verb_is_one_span_with_its_reference() {
        for verb in [
            "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
        ] {
            for form in [verb.to_owned(), verb.to_uppercase(), format!("{verb}:")] {
                assert_eq!(codes(&format!("x {form} #12 y")), [format!("{form} #12")]);
            }
        }
        assert_eq!(codes("Fixes: o/r#3."), ["Fixes: o/r#3"]);
    }

    #[test]
    fn references_mentions_urls_and_commands_are_spans() {
        assert_eq!(
            codes("see #1, GH-2, o/r#3, o/r@abcdef1, @user, @org/team"),
            ["#1", "GH-2", "o/r#3", "o/r@abcdef1", "@user", "@org/team"]
        );
        assert_eq!(
            codes("at https://github.com/o/r/issues/9. and www.example.com"),
            ["https://github.com/o/r/issues/9", "www.example.com"]
        );
        assert_eq!(
            codes("/merge\nthen /lgtm and bors r+"),
            ["/merge", "/lgtm", "bors r+"]
        );
        assert_eq!(codes("and/or a path a/b"), Vec::<String>::new());
    }

    #[test]
    fn a_code_span_outruns_its_backticks() {
        assert_eq!(code_span("a"), "`a`");
        assert_eq!(code_span("a``b"), "```a``b```");
        assert_eq!(code_span("`a"), "`` `a ``");
        assert_eq!(code_span(""), "");
    }
}
