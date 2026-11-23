//! The hidden key of a daemon comment (#5): an HTML comment on the body's first line, invisible
//! in rendered Markdown, by which a replay finds the comment it already posted.
//!
//! ```text
//! <!-- bosun:key=item-7/sticky -->
//! The rest of the body.
//! ```
//!
//! Backends that store comments as plain text (GitHub) embed and extract it here; the local
//! file keeps the key in a field of its own.

const OPEN: &str = "<!-- bosun:key=";
const CLOSE: &str = " -->";

/// Whether `key` can be carried: 1 to 128 ASCII letters, digits and `-_.:/`. Anything else
/// could end the marker early or be mangled by a backend.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}

/// `body` with the marker of `key` on its first line. `key` must be [`valid_key`].
#[must_use]
pub fn embed(key: &str, body: &str) -> String {
    debug_assert!(valid_key(key), "invalid comment key {key:?}");
    format!("{OPEN}{key}{CLOSE}\n{body}")
}

/// The key and the rest of the body, if the first line of `text` is a marker.
#[must_use]
pub fn extract(text: &str) -> Option<(&str, &str)> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let key = first
        .trim_end_matches('\r')
        .strip_prefix(OPEN)?
        .strip_suffix(CLOSE)?;
    valid_key(key).then_some((key, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_and_extract_round_trip() {
        let text = embed("item-7/sticky", "attempt 2\nverdict passed");
        assert_eq!(
            extract(&text),
            Some(("item-7/sticky", "attempt 2\nverdict passed"))
        );
        assert_eq!(extract(&embed("k", "")), Some(("k", "")));
    }

    #[test]
    fn plain_and_forged_text_has_no_key() {
        assert_eq!(extract("a user comment"), None);
        assert_eq!(extract("<!-- bosun:key=a b -->\nbody"), None);
        assert_eq!(extract("x\n<!-- bosun:key=k -->"), None);
        assert!(!valid_key(""));
        assert!(!valid_key("a-->b"));
        assert!(!valid_key(&"k".repeat(129)));
    }
}
