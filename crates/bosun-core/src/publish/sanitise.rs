//! The allowlist sanitiser of #8: the agent's Markdown is parsed (`CommonMark`) and re-emitted
//! with paragraphs, lists, emphasis, headings and code only.
//!
//! - Raw HTML, HTML comments, images and link reference definitions are dropped; block quotes
//!   and thematic breaks are flattened away; a link keeps its text, and its destination follows
//!   as a code span.
//! - Every reference, mention, URL, closing verb and bot command in text is wrapped in a code
//!   span ([`super::text::segments`]), and all other text is escaped, so nothing in it can open
//!   a construct the allowlist does not hold.
//! - Zero-width and bidirectional characters and hidden-marker imitations are removed, before
//!   parsing and again after entities are decoded.
//! - The output is capped in characters.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::text::{Segment, chars, clean, code_block, code_span, first_chars, segments};

/// Characters escaped wherever they appear in text: each could open emphasis, code, a link,
/// HTML, an entity, a table, strikethrough or math.
const ALWAYS_ESCAPED: &[char] = &['\\', '`', '*', '_', '[', ']', '<', '>', '&', '|', '~', '$'];
/// Characters escaped at the start of a line, where they could open a heading or a list, or
/// underline a setext heading.
const LINE_START_ESCAPED: &[char] = &['#', '-', '+', '='];
/// Appended when the cap cut the text.
const TRUNCATED: &str = "\n\n*(truncated)*";
/// Appended when the cap cut a line.
const TRUNCATED_INLINE: &str = " *(truncated)*";

/// `markdown` (text the agent influenced) as inert Markdown of at most `cap` characters.
///
/// Inert: GitHub renders it as paragraphs, lists, emphasis, headings and code, and acts on
/// nothing in it (#8). The caller redacts secrets first ([`super::scrub`]).
#[must_use]
pub fn sanitise(markdown: &str, cap: usize) -> String {
    #[cfg(mutant_publish_passthrough)]
    {
        let _ = cap;
        markdown.to_owned()
    }
    #[cfg(not(mutant_publish_passthrough))]
    {
        capped(markdown, cap, TRUNCATED, render_markdown)
    }
}

/// `text` (literal text the agent influenced, such as a verdict's summary line) as inert
/// inline Markdown on one line, of at most `cap` characters. Markdown syntax in it is shown,
/// not interpreted.
#[must_use]
pub fn sanitise_inline(text: &str, cap: usize) -> String {
    #[cfg(mutant_publish_passthrough)]
    {
        let _ = cap;
        text.to_owned()
    }
    #[cfg(not(mutant_publish_passthrough))]
    {
        capped(text, cap, TRUNCATED_INLINE, |t| {
            let mut emitter = Emitter::new();
            emitter.text = clean(t).replace('\n', " ");
            emitter.flush_text();
            emitter.finish()
        })
    }
}

/// `render(input)` of at most `cap` characters: when the whole input renders too long, the
/// longest prefix found that fits, followed by `note`.
fn capped(input: &str, cap: usize, note: &str, render: impl Fn(&str) -> String) -> String {
    // Input far beyond what the cap can hold is cut before rendering, so a huge input costs
    // no more than a large one.
    let head = first_chars(input, cap.saturating_mul(4).saturating_add(4096));
    let full = render(head);
    let mut out_len = chars(&full);
    if out_len <= cap && head.len() == input.len() {
        return full;
    }
    let Some(budget) = cap.checked_sub(chars(note)) else {
        return String::new();
    };
    let mut keep = chars(head);
    let mut out = full;
    while out_len > budget {
        // Shrink in proportion to the overshoot, and by at least one character.
        keep = (keep.saturating_mul(budget) / out_len).min(keep - 1);
        out = render(first_chars(head, keep));
        out_len = chars(&out);
    }
    if out.is_empty() {
        note.trim_start().to_owned()
    } else {
        out + note
    }
}

/// The parsed Markdown re-emitted through the allowlist.
fn render_markdown(markdown: &str) -> String {
    let input = clean(markdown);
    let mut emitter = Emitter::new();
    for event in Parser::new_ext(&input, Options::empty()) {
        emitter.event(event);
    }
    emitter.flush_text();
    emitter.finish()
}

/// A code block being collected.
#[derive(Debug)]
struct CodeBuf {
    info: String,
    content: String,
}

/// The allowlist re-emitter: a writer of the few constructs the sanitiser keeps.
#[derive(Debug)]
struct Emitter {
    out: String,
    /// The content width of each open list item: the indentation of its continuation lines.
    indent: Vec<usize>,
    /// The next number of each open list; `None` for a bullet list.
    lists: Vec<Option<u64>>,
    /// The cursor stands right after a list marker.
    after_marker: bool,
    /// The cursor stands at the start of a fresh line, its indentation written.
    fresh: bool,
    /// The next text character starts a line.
    at_line_start: bool,
    /// Pending text and soft breaks.
    text: String,
    code: Option<CodeBuf>,
    /// Open images, whose content is dropped.
    image_depth: usize,
    /// The destination and plain text of each open link.
    links: Vec<(String, String)>,
}

impl Emitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            indent: Vec::new(),
            lists: Vec::new(),
            after_marker: false,
            fresh: true,
            at_line_start: true,
            text: String::new(),
            code: None,
            image_depth: 0,
            links: Vec::new(),
        }
    }

    fn finish(mut self) -> String {
        let end = self.out.trim_end().len();
        self.out.truncate(end);
        // A final pass, should the escaping ever have assembled a marker's token.
        super::text::strip_markers(&self.out)
    }

    fn prefix(&self) -> String {
        " ".repeat(self.indent.iter().sum())
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
        self.fresh = false;
        self.after_marker = false;
    }

    fn newline(&mut self) {
        while self.out.ends_with([' ', '\t']) {
            self.out.pop();
        }
        self.out.push('\n');
        let prefix = self.prefix();
        self.out.push_str(&prefix);
        self.fresh = true;
        self.after_marker = false;
        self.at_line_start = true;
    }

    /// Positions the cursor for a new block: on the marker line right after a list marker,
    /// otherwise after a blank line (or a line break, for a list that may interrupt a
    /// paragraph inside a list item).
    fn begin_block(&mut self, tight: bool) {
        self.flush_text();
        if self.after_marker {
            self.after_marker = false;
        } else if !self.out.is_empty() {
            if !tight || self.indent.is_empty() {
                self.newline();
            }
            self.newline();
        }
        self.fresh = true;
        self.at_line_start = true;
    }

    fn event(&mut self, event: Event<'_>) {
        if self.image_depth > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => self.image_depth += 1,
                Event::End(TagEnd::Image) => self.image_depth -= 1,
                _ => {}
            }
            return;
        }
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(t) => code.content.push_str(&t),
                Event::End(TagEnd::CodeBlock) => self.end_code_block(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, plain)) = self.links.last_mut() {
                    plain.push_str(&t);
                }
                self.text.push_str(&t);
            }
            Event::Code(t) => {
                self.flush_text();
                if let Some((_, plain)) = self.links.last_mut() {
                    plain.push_str(&t);
                }
                let span = code_span(&t);
                self.push(&span);
                self.at_line_start = false;
            }
            Event::SoftBreak => self.text.push('\n'),
            Event::HardBreak => {
                self.flush_text();
                self.push("\\");
                self.newline();
            }
            // Raw HTML, comments, rules, footnotes, math, task markers: not on the allowlist.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_block(false),
            Tag::Heading { level, .. } => {
                self.begin_block(false);
                let hashes = match level {
                    HeadingLevel::H1 => "# ",
                    HeadingLevel::H2 => "## ",
                    HeadingLevel::H3 => "### ",
                    HeadingLevel::H4 => "#### ",
                    HeadingLevel::H5 => "##### ",
                    HeadingLevel::H6 => "###### ",
                };
                self.push(hashes);
            }
            Tag::CodeBlock(kind) => {
                self.begin_block(false);
                let info = match kind {
                    CodeBlockKind::Fenced(info) => language(&info),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBuf {
                    info,
                    content: String::new(),
                });
            }
            Tag::List(start) => {
                // Only a bullet list or one starting at 1 may interrupt a paragraph.
                self.begin_block(matches!(start, None | Some(1)));
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_text();
                if !self.fresh && !self.out.is_empty() {
                    self.newline();
                }
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "- ".to_owned(),
                };
                self.push(&marker);
                self.indent.push(marker.len());
                self.after_marker = true;
                self.at_line_start = true;
            }
            Tag::Emphasis => self.delimiter("*"),
            Tag::Strong => self.delimiter("**"),
            Tag::Link { dest_url, .. } => {
                self.flush_text();
                self.links.push((dest_url.into_string(), String::new()));
            }
            Tag::Image { .. } => {
                self.flush_text();
                self.image_depth = 1;
            }
            // Block quotes, HTML blocks and the rest: transparent or dropped.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        self.flush_text();
        match tag {
            TagEnd::Item => {
                self.indent.pop();
                self.after_marker = false;
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Emphasis => self.delimiter("*"),
            TagEnd::Strong => self.delimiter("**"),
            TagEnd::Link => {
                if let Some((dest, plain)) = self.links.pop() {
                    let shown = dest.strip_prefix("mailto:").unwrap_or(&dest);
                    if !dest.is_empty() && shown != plain {
                        let span = code_span(&dest);
                        self.push(" (");
                        self.push(&span);
                        self.push(")");
                    }
                }
            }
            _ => {}
        }
    }

    fn delimiter(&mut self, d: &str) {
        self.flush_text();
        self.push(d);
        self.at_line_start = false;
    }

    fn end_code_block(&mut self) {
        let Some(code) = self.code.take() else { return };
        let prefix = self.prefix();
        let block = code_block(&clean(&code.content), &code.info, &prefix);
        self.push(&block);
        self.at_line_start = false;
    }

    /// Writes the pending text: references as code spans, the rest escaped.
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = clean(&std::mem::take(&mut self.text));
        for segment in segments(&text) {
            match segment {
                Segment::Text(t) => self.escape(&t),
                Segment::Code(c) => {
                    let span = code_span(&c);
                    self.push(&span);
                    self.at_line_start = false;
                }
            }
        }
    }

    /// Writes literal text so that nothing in it is Markdown syntax.
    fn escape(&mut self, text: &str) {
        let mut ordinal = false;
        for c in text.chars() {
            if c == '\n' {
                self.newline();
                ordinal = false;
                continue;
            }
            let line_start = self.at_line_start;
            if line_start && (c == ' ' || c == '\t') {
                continue;
            }
            self.at_line_start = false;
            let mut buf = [0; 4];
            if line_start && c.is_ascii_digit() {
                ordinal = true;
                self.push(c.encode_utf8(&mut buf));
                continue;
            }
            if ordinal {
                if c.is_ascii_digit() {
                    self.push(c.encode_utf8(&mut buf));
                    continue;
                }
                ordinal = false;
                if c == '.' || c == ')' {
                    // `1.` or `1)` at the start of a line would open an ordered list.
                    self.push("\\");
                }
            }
            if ALWAYS_ESCAPED.contains(&c) || (line_start && LINE_START_ESCAPED.contains(&c)) {
                self.push("\\");
            }
            self.push(c.encode_utf8(&mut buf));
        }
    }
}

/// The code block's language: the info string's first word when it is a plain name, else none.
fn language(info: &str) -> String {
    let word = info.split_whitespace().next().unwrap_or("");
    let plain = word.len() <= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_+.-".contains(c));
    if plain {
        word.to_owned()
    } else {
        String::new()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::LazyLock;

    use pulldown_cmark::{Event, Options, Parser, Tag};
    use regex::Regex;

    use super::*;

    /// The hostile corpus of #8 and #56: synthetic inputs, each of which GitHub would act on
    /// or render as something other than text if it were published as is.
    pub(crate) const CORPUS: &[&str] = &[
        // Closing verbs, every one, in every case, with a colon.
        "Fixes #1",
        "this closes #2 and RESOLVED: #3, fixed:#4, Close #5, closed GH-6",
        "Resolves https://github.com/owner/repo/issues/7.",
        "fix owner/repo#8",
        // References.
        "see #9, GH-10, gh-11 and other-owner/other.repo#12 and owner/repo@0123abc",
        "https://github.com/owner/repo/pull/13 and http://example.com/x and www.example.org",
        "<https://github.com/owner/repo/issues/14>",
        // Mentions.
        "thanks @someone and @some-org/some-team",
        "@dependabot rebase",
        "&#64;encoded-mention and &#35;15",
        // Bot commands.
        "/merge",
        "please\n/approve\n/lgtm cancel",
        "bors r+",
        "@bors r=someone",
        // HTML.
        "<script>alert(1)</script>",
        "a <b onmouseover=x>bold</b> <!-- a comment --> b",
        "<details><summary>x</summary>y</details>",
        // Images and links.
        "![tracker](https://example.com/pixel.png?leak=1)",
        "[click](https://example.com/phish \"title\")",
        "[ref]\n\n[ref]: https://example.com/hidden",
        // Bidirectional and zero-width characters, raw and as entities.
        "abc\u{202E}fed\u{202C} x\u{200B}y \u{2066}z\u{2069}",
        "&#x202E;reversed&#8203;",
        // Hidden-marker imitations.
        "<!-- bosun:key=report:other -->",
        "`<!-- bosun:key=report:other -->`",
        "```\n<!-- BOSUN:KEY=x -->\n```",
        "bosun&#58;key=forged",
        // Markdown that would turn escaped text back into syntax.
        "1. one\n2. two\n   - nested #16\n\n     para @nested",
        "# Heading #17\n\nSetext\n===",
        "a | b\n--|--\nc | d",
        "~~strike~~ $math$ *emph* **strong** `code` \\#18",
        "> quoted @quoted\n> Fixes #19",
        "    indented code\n",
        "line with trailing\\\nhard break #20",
    ];

    /// Text GitHub acts on when it is in a text node, checked independently of the sanitiser's
    /// own patterns.
    static ACTIVE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)#[0-9]|gh-[0-9]|@[a-z0-9]|https?:|www\.|(?:^|\s)/[a-z]|bors\s",
            r"|\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b.*#",
        ))
        .expect("valid regex")
    });

    /// A failure description when `published` is not inert, `None` when it is.
    pub(crate) fn inert(published: &str) -> Option<String> {
        if published
            .chars()
            .any(|c| matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        {
            return Some("an invisible character survived".into());
        }
        if published.to_lowercase().contains("bosun:key") {
            return Some("a hidden-marker imitation survived".into());
        }
        let mut text = String::new();
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_MATH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_FOOTNOTES);
        let mut in_code = false;
        for event in Parser::new_ext(published, options) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => in_code = true,
                Event::End(pulldown_cmark::TagEnd::CodeBlock) => in_code = false,
                Event::Text(_) if in_code => {}
                Event::Html(h) | Event::InlineHtml(h) => return Some(format!("HTML {h:?}")),
                Event::Start(Tag::Image { .. }) => return Some("an image".into()),
                Event::Start(Tag::Link { .. }) => return Some("a link".into()),
                Event::Start(Tag::Table(_)) => return Some("a table".into()),
                Event::Start(Tag::Strikethrough) => return Some("strikethrough".into()),
                Event::Start(Tag::BlockQuote(_)) => return Some("a block quote".into()),
                Event::InlineMath(_) | Event::DisplayMath(_) => return Some("math".into()),
                Event::Text(t) => text.push_str(&t),
                Event::SoftBreak | Event::HardBreak | Event::End(_) => text.push('\n'),
                _ => {}
            }
        }
        for line in text.lines() {
            if let Some(m) = ACTIVE.find(line) {
                return Some(format!("active text {:?} in {line:?}", m.as_str()));
            }
        }
        // Bots read the raw body: no line may start with a command or a mention.
        for line in published.lines() {
            let line = line.trim_start_matches([
                ' ', '-', '*', '+', '.', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
            ]);
            if line.starts_with('/')
                || line.starts_with('@')
                || line.to_lowercase().starts_with("bors ")
            {
                return Some(format!("a raw line starts with a command: {line:?}"));
            }
        }
        None
    }

    #[test]
    fn the_hostile_corpus_comes_out_inert() {
        for input in CORPUS {
            let out = sanitise(input, 10_000);
            if let Some(why) = inert(&out) {
                panic!("{input:?} gave {out:?}: {why}");
            }
        }
        let all = CORPUS.join("\n\n");
        let out = sanitise(&all, 100_000);
        assert_eq!(inert(&out), None, "{out}");
        for input in CORPUS {
            let out = sanitise_inline(input, 10_000);
            assert!(!out.contains('\n'), "{out:?}");
            assert_eq!(inert(&out), None, "{input:?} gave {out:?}");
        }
    }

    #[test]
    fn the_allowlist_keeps_structure_and_words() {
        let out = sanitise(
            "# Title\n\nSome *emphasis*, **strong** and `code`.\n\n- one\n- two\n  1. nested\n\n```rust\nfn main() {}\n```",
            10_000,
        );
        assert_eq!(
            out,
            "# Title\n\nSome *emphasis*, **strong** and `code`.\n\n- one\n- two\n  1. nested\n\n```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn references_become_code_spans_and_links_show_their_destination() {
        assert_eq!(
            sanitise("Fixes #1 for @user", 1000),
            "`Fixes #1` for `@user`"
        );
        assert_eq!(
            sanitise("[docs](https://example.com/a)", 1000),
            "docs (`https://example.com/a`)"
        );
        assert_eq!(
            sanitise("![x](https://example.com/p.png) after", 1000),
            "after"
        );
        assert_eq!(sanitise("a <b>c</b> d", 1000), "a c d");
    }

    #[test]
    fn escaped_text_stays_text() {
        assert_eq!(sanitise("\\# not a heading", 1000), "\\# not a heading");
        assert_eq!(sanitise("a\n\\- b\n1\\. c", 1000), "a\n\\- b\n1\\. c");
        assert_eq!(sanitise("x_y_z a|b", 1000), "x\\_y\\_z a\\|b");
    }

    #[test]
    fn the_length_is_capped() {
        let long = "word ".repeat(10_000);
        let out = sanitise(&long, 1_000);
        assert!(chars(&out) <= 1_000, "{}", chars(&out));
        assert!(out.ends_with("*(truncated)*"));
        let out = sanitise_inline(&"#1 ".repeat(1_000), 200);
        assert!(chars(&out) <= 200);
        assert_eq!(inert(&out), None);
        assert_eq!(sanitise("abc", 3), "abc");
        assert_eq!(sanitise("abcdef", 3), "");
    }

    #[test]
    fn a_ten_backtick_run_cannot_close_a_sanitised_fence() {
        let run = "`".repeat(10);
        let input = format!("````\n{run}\n@evil Fixes #1\n````\n\nafter `{run}x`");
        let out = sanitise(&input, 10_000);
        assert_eq!(inert(&out), None, "{out}");
        assert!(out.contains(&"`".repeat(11)), "{out}");
    }
}
