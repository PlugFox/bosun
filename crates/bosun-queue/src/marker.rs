//! The hidden key of a daemon comment (#5): an HTML comment on the body's first line, invisible
//! in rendered Markdown, by which a replay finds the comment it already posted.
//!
//! ```text
//! <!-- bosun:key=report:7 -->
//! The rest of the body.
//! ```
//!
//! The format is the one of [`bosun_core::publish::marker`] and [`bosun_core::publish::find_key`]
//! (#56), so the sticky report the renderer writes, the comments the backends post and the
//! prompt builder's skip all read one marker. Characters outside `[A-Za-z0-9._:/#]` are
//! percent-encoded in the marker; [`extract`] decodes them, so a key reads back as it was
//! written.
//!
//! Backends that store comments as plain text (GitHub) embed and extract it here; the local
//! file keeps the key in a field of its own and stores the body without the marker.

use bosun_core::publish;

/// Whether `key` can be carried: 1 to 128 ASCII letters, digits and `-_.:/#`. The marker would
/// encode anything, but a key is also a field of the local file and a log line, and keys are
/// the daemon's own (`report:7`, `claim:host/user`).
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/#".contains(&b))
}

/// `body` with the marker of `key` on its first line, once: a body that already starts with
/// that marker (the sticky report as [`publish::render_report`] renders it) keeps its own.
/// `key` must be [`valid_key`].
#[must_use]
pub fn embed(key: &str, body: &str) -> String {
    debug_assert!(valid_key(key), "invalid comment key {key:?}");
    format!("{}\n{}", publish::marker(key), without(key, body))
}

/// `body` without a leading marker of `key`; any other body as it is.
#[must_use]
pub fn without<'b>(key: &str, body: &'b str) -> &'b str {
    match extract(body) {
        Some((found, rest)) if found == key => rest,
        _ => body,
    }
}

/// The key (decoded) and the rest of the body, if the first line of `text` is a marker of a
/// [`valid_key`].
#[must_use]
pub fn extract(text: &str) -> Option<(String, &str)> {
    let encoded = publish::find_key(text)?;
    let key = decode(encoded)?;
    let rest = text.split_once('\n').map_or("", |(_, rest)| rest);
    valid_key(&key).then_some((key, rest))
}

/// Undoes the percent-encoding of [`publish::marker`].
fn decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = encoded.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_and_extract_round_trip() {
        let text = embed("item-7/sticky_1", "attempt 2\nverdict passed");
        assert!(
            text.starts_with(&publish::marker("item-7/sticky_1")),
            "the format of bosun_core::publish: {text}"
        );
        let (key, rest) = extract(&text).unwrap();
        assert_eq!(key, "item-7/sticky_1");
        assert_eq!(rest, "attempt 2\nverdict passed");
        assert_eq!(extract(&embed("k", "")), Some(("k".to_owned(), "")));
        assert_eq!(
            extract("<!-- bosun:key=report:7 -->\r\nbody"),
            Some(("report:7".to_owned(), "body"))
        );
    }

    #[test]
    fn a_body_that_carries_its_marker_is_not_doubled() {
        let rendered = format!("{}\n### bosun: a title", publish::marker("report:7"));
        assert_eq!(embed("report:7", &rendered), rendered);
        assert_eq!(without("report:7", &rendered), "### bosun: a title");
        let other = embed("report:8", &rendered);
        assert_eq!(
            other.matches("bosun:key=").count(),
            2,
            "another key's marker stays"
        );
        assert_eq!(without("report:9", "plain"), "plain");
    }

    #[test]
    fn plain_and_forged_text_has_no_key() {
        assert_eq!(extract("a user comment"), None);
        assert_eq!(extract("<!-- bosun:key=a b -->\nbody"), None);
        assert_eq!(
            extract("<!-- bosun:key=a%20b -->\nbody"),
            None,
            "not a valid key"
        );
        assert_eq!(
            extract("<!-- bosun:key=a%2 -->\nbody"),
            None,
            "broken encoding"
        );
        assert_eq!(extract("x\n<!-- bosun:key=k -->"), None);
        assert!(!valid_key(""));
        assert!(!valid_key("a-->b"));
        assert!(!valid_key(&"k".repeat(129)));
        assert!(valid_key("report:owner/repo#12"));
    }
}
