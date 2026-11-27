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

/// The longest secret the credential store takes, in characters (Windows' credential blob is
/// at most 2,560 bytes).
pub const SECRET_CAP: usize = 2_048;

/// The credential-store user of the secret of channel `channel` of `identity`:
/// `notify/<identity>/<channel>`. It is what the channel's row keeps.
#[must_use]
pub fn entry(identity: &str, channel: &str) -> String {
    format!("notify/{identity}/{channel}")
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
        && secret.chars().count() > SECRET_CAP
    {
        return Err(format!("the secret is longer than {SECRET_CAP} characters"));
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
        assert_eq!(entry("a", "ops"), "notify/a/ops");
    }
}
