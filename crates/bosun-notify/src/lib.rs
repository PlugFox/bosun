//! Notifications of bosun (#9, "Notifications"; #208): what every channel kind stands on.
//!
//! - [`Notifier`]: one channel, built and ready to send one plain-text message. It is
//!   synchronous; `bosund` calls it on its blocking pool.
//! - [`build`]: the kind registry. It checks a channel's non-secret parameters (an unknown kind
//!   or key is refused, with the known ones named) and builds its notifier. The kinds:
//!   [`telegram`] (a chat, through the Bot API; how to get a bot token and a chat id is in its
//!   docs), and [`fake`], for tests, which a running daemon refuses ([`Kinds::fake`]). The
//!   signed webhook comes next.
//! - [`secret`]: the OS credential store, where a channel's secret lives (Windows Credential
//!   Manager, macOS Keychain, Secret Service). The channel row in the identity's database keeps
//!   only the entry's name ([`entry`]), never the secret, so neither the database nor one of its
//!   backups reveals a token.
//! - [`text`]: what any message goes through before it is sent: plain text (no markup mode),
//!   the redactor of `bosun_core::publish`, and a length cap.
//! - [`render`]: the text of a queued [`Notice`], or of a coalesced burst of them: ids,
//!   states, reasons and numbers only.
//! - [`policy`]: when a channel's pending notices go out (the minimum gap, the coalescing, the
//!   maximum age, the backoff), pure.
//!
//! Channels belong to an identity (#2): an entry name carries the identity, and there is no
//! channel shared across identities.
//!
//! This crate never reaches `bosun-store` or the other side-effect crates (#12): `bosund`
//! composes it with the store.

pub mod fake;
pub mod policy;
pub mod render;
pub mod secret;
pub mod telegram;

pub use render::{Notice, render};

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// The service every credential-store entry of bosun is filed under.
pub const SERVICE: &str = "bosun";

/// The longest text a message carries, in characters; a longer one is cut and marked.
pub const TEXT_CAP: usize = 3_500;

/// The longest secret the credential store takes, in UTF-16 code units: Windows Credential
/// Manager keeps the blob as UTF-16 of at most 2,560 bytes. It holds on every platform, so a
/// channel that works on Linux works on Windows too.
pub const SECRET_CAP: usize = 1_280;

/// Whether `secret` is longer than [`SECRET_CAP`] UTF-16 code units (a character outside the
/// BMP counts two).
#[must_use]
pub fn secret_too_long(secret: &str) -> bool {
    // Mutant (`mutant_notify_secret_cap_chars`): the cap counts characters, at 2,048.
    if cfg!(mutant_notify_secret_cap_chars) {
        return secret.chars().count() > 2_048;
    }
    secret.encode_utf16().count() > SECRET_CAP
}

/// The longest one send may take: a kind's transport gives up after it. The daemon's drainer
/// waits that long, and a little more, for a send in flight before it stops.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// The credential-store user of the secret of channel `channel` of `identity`, whose row has
/// the random `id` ([`entry_id`]): `notify/<identity>/<channel>/<id>`. It is what the
/// channel's row keeps, and what is read back is that string, whatever its form: a row written
/// before the id (`notify/<identity>/<channel>`) keeps its name.
///
/// The id keeps two rows apart that would otherwise share an entry: Windows' target names are
/// case-insensitive (`Ops` and `ops`), and a channel removed and added again could meet a
/// stale entry.
#[must_use]
pub fn entry(identity: &str, channel: &str, id: &str) -> String {
    // Mutant (`mutant_notify_entry_without_id`): the entry name has no id.
    if cfg!(mutant_notify_entry_without_id) {
        return format!("notify/{identity}/{channel}");
    }
    format!("notify/{identity}/{channel}/{id}")
}

/// A fresh random id of a channel row: 16 bytes of the OS's random source, in hex.
///
/// # Errors
///
/// The OS's random source failed.
pub fn entry_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("the random source: {e}"))?;
    Ok(bytes.iter().fold(String::with_capacity(32), |mut out, b| {
        use fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    }))
}

/// Why a send did not go through. Neither variant ever carries the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// Try again later; `after` is the service's own wait when it gives one.
    Retry {
        after: Option<Duration>,
        error: String,
    },
    /// Trying again will not help: a refused credential or a bad parameter.
    Permanent(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry {
                after: Some(after),
                error,
            } => write!(f, "{error} (retry after {} s)", after.as_secs()),
            Self::Retry { after: None, error } => write!(f, "{error} (retry)"),
            Self::Permanent(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for SendError {}

/// One channel, ready to send.
pub trait Notifier: Send + Sync + fmt::Debug {
    /// Sends `text`, as [`text`] made it, now.
    ///
    /// # Errors
    ///
    /// [`SendError::Retry`] for a failure that may pass, [`SendError::Permanent`] otherwise.
    fn send(&self, text: &str) -> Result<(), SendError>;
}

/// The kinds a caller admits besides the real ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kinds {
    /// [`fake`], for tests: `bosund` passes `cfg!(test)`, so a running daemon refuses it.
    pub fake: bool,
}

/// The kinds this version knows, under `kinds`.
#[must_use]
pub fn known_kinds(kinds: Kinds) -> Vec<&'static str> {
    let mut known = vec![telegram::KIND];
    if kinds.fake {
        known.push(fake::KIND);
    }
    known
}

/// Checks a channel of `kind` with `params` (and `secret`, for a kind that needs one) and
/// builds its notifier.
///
/// # Errors
///
/// An unknown kind, an unknown or missing parameter, or a bad value; the text names the known
/// kinds or keys and never the secret.
pub fn build(
    kind: &str,
    params: &BTreeMap<String, String>,
    secret: Option<&str>,
    kinds: Kinds,
) -> Result<Box<dyn Notifier>, String> {
    if let Some(secret) = secret
        && secret_too_long(secret)
    {
        return Err(format!(
            "the secret is longer than {SECRET_CAP} UTF-16 units"
        ));
    }
    match kind {
        fake::KIND if kinds.fake => Ok(Box::new(fake::Fake::new(params)?)),
        telegram::KIND => Ok(Box::new(telegram::Telegram::new(params, secret)?)),
        _ => Err(format!(
            "unknown channel kind {kind:?}; the known kinds: {}",
            known_kinds(kinds).join(", ")
        )),
    }
}

/// Refuses a key of `params` that is not in `known`, naming the known ones.
pub(crate) fn check_keys(
    kind: &str,
    params: &BTreeMap<String, String>,
    known: &[&str],
) -> Result<(), String> {
    let unknown = params
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .map(|k| format!("{k:?}"))
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unknown parameter {} of a {kind} channel; the known ones: {}",
            unknown.join(", "),
            known.join(", ")
        ))
    }
}

/// `text` as a message carries it: plain text (no markup mode is ever asked for), with the
/// invisible characters gone and the secrets redacted (`bosun_core::publish::scrub`), and cut
/// to [`TEXT_CAP`] characters with a marker.
#[must_use]
pub fn text(text: &str) -> String {
    let clean = bosun_core::publish::scrub(text);
    if clean.chars().count() <= TEXT_CAP {
        return clean;
    }
    let mut cut = clean.chars().take(TEXT_CAP - 1).collect::<String>();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::secret::Secrets as _;
    use super::*;

    #[test]
    fn an_unknown_kind_or_key_is_refused_with_the_known_ones() {
        let none = BTreeMap::new();
        let refused = build("fake", &none, None, Kinds::default()).unwrap_err();
        assert!(refused.contains("unknown channel kind"), "{refused}");
        let refused = build("pager", &none, None, Kinds { fake: true }).unwrap_err();
        assert!(refused.contains("fake"), "{refused}");
        let params = BTreeMap::from([("colour".to_owned(), "red".to_owned())]);
        let refused = build("fake", &params, None, Kinds { fake: true }).unwrap_err();
        assert!(
            refused.contains("\"colour\"") && refused.contains("sink"),
            "{refused}"
        );
    }

    #[test]
    fn a_long_text_is_cut_and_marked() {
        let long = "x".repeat(TEXT_CAP + 10);
        let cut = text(&long);
        assert_eq!(cut.chars().count(), TEXT_CAP);
        assert!(cut.ends_with('…'));
        assert_eq!(text("bosun a: test"), "bosun a: test");
    }

    #[test]
    fn an_entry_names_the_identity_and_the_channel() {
        assert_eq!(entry("a", "ops", "0f"), "notify/a/ops/0f");
        let id = entry_id().unwrap();
        assert_eq!(id.len(), 32, "{id}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    }

    #[test]
    fn channels_differing_in_case_get_different_entries() {
        let upper = entry("a", "Ops", &entry_id().unwrap());
        let lower = entry("a", "ops", &entry_id().unwrap());
        // Windows compares target names without case.
        assert!(!upper.eq_ignore_ascii_case(&lower), "{upper} {lower}");
        // A channel removed and added again gets another entry too.
        let again = entry("a", "ops", &entry_id().unwrap());
        assert!(!again.eq_ignore_ascii_case(&lower), "{again} {lower}");
    }

    #[test]
    fn the_secret_cap_counts_utf16_units() {
        let at_cap = "x".repeat(1_280);
        assert!(!secret_too_long(&at_cap));
        assert!(secret_too_long(&"x".repeat(1_281)));
        // 700 characters, 1,400 units: each is outside the BMP.
        let wide = "\u{1F600}".repeat(700);
        assert_eq!(wide.chars().count(), 700);
        assert!(secret_too_long(&wide));

        let store = secret::MemorySecrets::new();
        store.put("notify/a/ops/0f", &at_cap).unwrap();
        assert!(store.put("notify/a/ops/0f", &wide).is_err());
        let refused =
            build("fake", &BTreeMap::new(), Some(&wide), Kinds { fake: true }).unwrap_err();
        assert!(refused.contains("1280 UTF-16 units"), "{refused}");
    }
}
