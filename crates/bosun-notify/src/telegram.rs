//! The `telegram` kind (#9): a message to one chat through the Telegram Bot API, outbound only.
//!
//! A channel has one parameter, `chat`, and a secret, the bot's token:
//!
//! - The token comes from `@BotFather` in Telegram: `/newbot`, a name, a username; it answers
//!   with a token shaped like `123456789:AAAA...` (the bot's id, a colon, 35 characters).
//! - The chat: write the bot a first message (or add it to the group or the channel), then open
//!   `https://api.telegram.org/bot<token>/getUpdates` in a browser once; `message.chat.id` of
//!   the answer is the chat id (`123456789` for a person, `-1001234567890` for a group or a
//!   channel). A public channel or group may be named by its username instead (`@demo_alerts`).
//!   bosun itself never asks Telegram for updates.
//!
//! ```text
//! bosun notify add ops --kind telegram --param chat=-1001234567890 < token.txt
//! bosun notify test ops
//! ```
//!
//! Each message is `POST https://api.telegram.org/bot<token>/sendMessage` with the chat, the
//! text and the link preview off, over HTTPS with the operating system's trust store (no
//! bundled root list). Plain text only: no `parse_mode`, ever (#208: an escaping error drops a
//! message). The text goes through `bosun_core::publish::scrub` and is cut to [`TEXT_CAP`]
//! characters (Telegram's own cap is 4,096).
//!
//! The answer: `ok` is a delivery; a 429 waits as Telegram says (`retry_after`); another 5xx, a
//! timeout or a transport failure is retried; a 4xx (a wrong chat, a revoked token, a bot
//! removed from the chat) is permanent. The token is in the request's URL, so no error string
//! leaves this module with it: each names the request as
//! `POST https://api.telegram.org/bot[redacted]/sendMessage`, and the token is replaced in
//! whatever the client or the service said. (`ureq` logs through the `log` facade, which bosun
//! installs no logger for.)
//!
//! The base URL is a constant; only this crate's tests construct a channel with another one.
//! No `getUpdates`, no webhook endpoint: commands from a chat are remote control, an open
//! question of #9.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Notifier, SendError, check_keys};

/// The kind's name.
pub const KIND: &str = "telegram";

/// Its parameters.
const KEYS: &[&str] = &["chat"];

/// The Bot API.
const BASE: &str = "https://api.telegram.org";

/// The longest text a message carries, in characters (Telegram's cap is 4,096).
pub const TEXT_CAP: usize = 4_000;

/// What a cut text ends with.
const CUT_MARK: &str = "...";

/// What the token is replaced with in an error.
const REDACTED: &str = "[redacted]";

/// The longest answer read, in bytes.
const ANSWER_CAP: u64 = 64 * 1024;

/// The chat a channel posts to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chat {
    /// A chat id: a person, or a group or a channel (negative).
    Id(i64),
    /// `@` and a public username.
    Username(String),
}

impl Chat {
    fn parse(chat: &str) -> Result<Self, String> {
        if let Ok(id) = chat.parse::<i64>() {
            return Ok(Self::Id(id));
        }
        if let Some(name) = chat.strip_prefix('@')
            && (5..=32).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Ok(Self::Username(chat.to_owned()));
        }
        Err(format!(
            "the `chat` of a telegram channel is a chat id (an integer, negative for a group or \
             a channel) or `@` and a public username of 5 to 32 of A-Z, a-z, 0-9 and _, not \
             {chat:?}"
        ))
    }

    fn json(&self) -> Value {
        match self {
            Self::Id(id) => json!(id),
            Self::Username(name) => json!(name),
        }
    }
}

/// Whether `token` is shaped like a bot token: digits, a colon, then of A-Z, a-z, 0-9, _ and -.
fn token_shaped(token: &str) -> bool {
    token.split_once(':').is_some_and(|(id, key)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && key.len() >= 30
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// A `telegram` channel.
pub struct Telegram {
    agent: ureq::Agent,
    base: String,
    token: String,
    chat: Chat,
}

impl fmt::Debug for Telegram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telegram")
            .field("base", &self.base)
            .field("token", &REDACTED)
            .field("chat", &self.chat)
            .finish_non_exhaustive()
    }
}

impl Telegram {
    pub(crate) fn new(
        params: &BTreeMap<String, String>,
        secret: Option<&str>,
    ) -> Result<Self, String> {
        Self::with_base(BASE, params, secret)
    }

    /// A channel that talks to `base` instead of the Bot API: this crate's tests only.
    #[cfg(test)]
    pub(crate) fn for_test(
        base: &str,
        params: &BTreeMap<String, String>,
        secret: Option<&str>,
    ) -> Result<Self, String> {
        Self::with_base(base, params, secret)
    }

    fn with_base(
        base: &str,
        params: &BTreeMap<String, String>,
        secret: Option<&str>,
    ) -> Result<Self, String> {
        check_keys(KIND, params, KEYS)?;
        let chat = params
            .get("chat")
            .ok_or_else(|| {
                "a telegram channel needs its `chat` parameter; the known ones: chat".to_owned()
            })
            .and_then(|chat| Chat::parse(chat))?;
        let token = secret
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "a telegram channel needs its secret, the bot's token (from @BotFather), on stdin"
                    .to_owned()
            })?;
        if !token_shaped(token) {
            return Err(
                "the secret of a telegram channel is a bot token: the bot's id, a colon and its \
                 key (as @BotFather gives it)"
                    .to_owned(),
            );
        }
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Ok(Self {
            agent,
            base: base.to_owned(),
            token: token.to_owned(),
            chat,
        })
    }

    /// The request, as an error names it.
    fn request(&self) -> String {
        // Mutant (`mutant_telegram_token_in_error`): the URL with the token goes into errors.
        if cfg!(mutant_telegram_token_in_error) {
            return format!("POST {}/bot{}/sendMessage", self.base, self.token);
        }
        format!("POST {}/bot{REDACTED}/sendMessage", self.base)
    }

    /// `detail` with the token replaced, for an error.
    fn clean(&self, detail: &str) -> String {
        if cfg!(mutant_telegram_token_in_error) {
            return detail.to_owned();
        }
        detail.replace(&self.token, REDACTED)
    }

    /// What the answer of status `status` and body `body` means.
    fn answer(&self, status: u16, body: &str) -> Result<(), SendError> {
        let answer = serde_json::from_str::<Value>(body).ok();
        let ok = answer
            .as_ref()
            .and_then(|a| a.get("ok"))
            .and_then(Value::as_bool);
        if (200..300).contains(&status) && ok == Some(true) {
            return Ok(());
        }
        let description = answer
            .as_ref()
            .and_then(|a| a.get("description"))
            .and_then(Value::as_str)
            .map_or_else(|| "no description".to_owned(), |d| self.clean(d));
        let error = format!("{}: {status} {description}", self.request());
        match status {
            429 => {
                let after = answer
                    .as_ref()
                    .and_then(|a| a.pointer("/parameters/retry_after"))
                    .and_then(Value::as_u64)
                    .map(Duration::from_secs);
                // Mutant (`mutant_telegram_ignore_retry_after`): Telegram's wait is dropped.
                let after = if cfg!(mutant_telegram_ignore_retry_after) {
                    None
                } else {
                    after
                };
                Err(SendError::Retry { after, error })
            }
            // A server error, or a 2xx answer that is not the Bot API's (a proxy's page, a cut
            // body): try again.
            200..=299 | 500..=599 => Err(SendError::Retry { after: None, error }),
            _ => Err(SendError::Permanent(error)),
        }
    }
}

/// `text` as a message carries it: scrubbed and redacted, then cut to [`TEXT_CAP`] characters
/// on a character boundary, ending in `...`.
fn message(text: &str) -> String {
    // Mutant (`mutant_telegram_no_cap`): the text is posted uncut and unscrubbed.
    if cfg!(mutant_telegram_no_cap) {
        return text.to_owned();
    }
    let clean = bosun_core::publish::scrub(text);
    if clean.chars().count() <= TEXT_CAP {
        return clean;
    }
    let mut cut = clean
        .chars()
        .take(TEXT_CAP - CUT_MARK.len())
        .collect::<String>();
    cut.push_str(CUT_MARK);
    cut
}

impl Notifier for Telegram {
    fn send(&self, text: &str) -> Result<(), SendError> {
        #[allow(unused_mut, reason = "the mutant below changes it")]
        let mut body = json!({
            "chat_id": self.chat.json(),
            "text": message(text),
            "link_preview_options": {"is_disabled": true},
        });
        // Mutant (`mutant_telegram_parse_mode`): the request asks for a markup mode.
        if cfg!(mutant_telegram_parse_mode) {
            body["parse_mode"] = json!("MarkdownV2");
        }
        let url = format!("{}/bot{}/sendMessage", self.base, self.token);
        let response = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body.to_string().as_str());
        let mut response = match response {
            Ok(response) => response,
            Err(e) => {
                return Err(SendError::Retry {
                    after: None,
                    error: format!("{}: {}", self.request(), self.clean(&e.to_string())),
                });
            }
        };
        let status = response.status().as_u16();
        let mut answer = String::new();
        if let Err(e) = response
            .body_mut()
            .as_reader()
            .take(ANSWER_CAP)
            .read_to_string(&mut answer)
        {
            return Err(SendError::Retry {
                after: None,
                error: format!(
                    "{}: {status}, reading the answer: {}",
                    self.request(),
                    self.clean(&e.to_string())
                ),
            });
        }
        self.answer(status, &answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// A synthetic bot token.
    fn token() -> String {
        format!("123456789:{}", "A".repeat(35))
    }

    fn params(chat: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("chat".to_owned(), chat.to_owned())])
    }

    /// A fake Bot API on a local port that answers one request with `status` and `body`; the
    /// receiver gets the request line and the request body.
    fn serve(status: u16, body: &'static str) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} Canned\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
            tx.send((
                line.trim_end().to_owned(),
                String::from_utf8(request).unwrap(),
            ))
            .unwrap();
        });
        (base, rx)
    }

    fn channel(base: &str, chat: &str) -> Telegram {
        Telegram::for_test(base, &params(chat), Some(&token())).unwrap()
    }

    fn posted(rx: &mpsc::Receiver<(String, String)>) -> (String, Value) {
        let (line, body) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (line, serde_json::from_str(&body).unwrap())
    }

    #[test]
    fn a_message_is_posted_as_plain_text_to_the_chat() {
        let text = "demo#4 needs you (lint), attempt 2";
        let (base, rx) = serve(200, r#"{"ok":true,"result":{}}"#);
        channel(&base, "-1001234567890").send(text).unwrap();
        let (line, body) = posted(&rx);
        assert_eq!(
            line,
            format!("POST /bot{}/sendMessage HTTP/1.1", token()),
            "{line}"
        );
        assert_eq!(body["chat_id"], json!(-1_001_234_567_890_i64), "{body}");
        assert_eq!(body["text"], json!(text), "{body}");
        assert!(body.get("parse_mode").is_none(), "{body}");
        assert_eq!(body["link_preview_options"]["is_disabled"], json!(true));

        let (base, rx) = serve(200, r#"{"ok":true,"result":{}}"#);
        channel(&base, "@demo_alerts").send(text).unwrap();
        let (_, body) = posted(&rx);
        assert_eq!(body["chat_id"], json!("@demo_alerts"), "{body}");
        assert!(body.get("parse_mode").is_none(), "{body}");
    }

    #[test]
    fn a_long_message_is_cut_to_the_cap_and_its_secrets_redacted() {
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ");
        let text = format!("leaked {secret} here {}", "x".repeat(10_000));
        let text = text.chars().take(10_000).collect::<String>();
        let (base, rx) = serve(200, r#"{"ok":true,"result":{}}"#);
        channel(&base, "-100123").send(&text).unwrap();
        let (_, body) = posted(&rx);
        let sent = body["text"].as_str().unwrap();
        assert!(sent.chars().count() <= TEXT_CAP, "{}", sent.chars().count());
        assert!(sent.ends_with("..."), "{sent}");
        assert!(sent.contains("[redacted]"), "{sent}");
        assert!(!sent.contains(&secret), "{sent}");
    }

    #[test]
    fn a_rate_limit_waits_as_told_and_no_error_names_the_token() {
        let token = token();
        let prefix = "123456789:";
        let check = |error: &SendError| {
            let text = error.to_string();
            assert!(text.contains("bot[redacted]/sendMessage"), "{text}");
            assert!(!text.contains(&token), "{text}");
            assert!(!text.contains(prefix), "{text}");
        };

        let (base, _rx) = serve(
            429,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}"#,
        );
        let error = channel(&base, "-100123").send("hello").unwrap_err();
        assert!(
            matches!(&error, SendError::Retry { after: Some(after), .. } if *after == Duration::from_secs(7)),
            "{error:?}"
        );
        check(&error);

        let (base, _rx) = serve(
            401,
            r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
        );
        let error = channel(&base, "-100123").send("hello").unwrap_err();
        assert!(matches!(&error, SendError::Permanent(_)), "{error:?}");
        assert!(error.to_string().contains("Unauthorized"), "{error}");
        check(&error);

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = channel(&format!("http://127.0.0.1:{port}"), "-100123")
            .send("hello")
            .unwrap_err();
        assert!(
            matches!(&error, SendError::Retry { after: None, .. }),
            "{error:?}"
        );
        check(&error);
    }

    #[test]
    fn a_telegram_channel_is_built_from_its_row_and_refuses_a_bad_chat() {
        let token = token();
        let kinds = crate::Kinds::default();
        let built = crate::build(KIND, &params("-100123"), Some(&token), kinds).unwrap();
        assert!(!format!("{built:?}").contains(&token));
        crate::build(KIND, &params("@demo_alerts"), Some(&token), kinds).unwrap();

        let mut extra = params("-100123");
        extra.insert("parse_mode".to_owned(), "MarkdownV2".to_owned());
        let refusals = [
            crate::build(KIND, &params("hello"), Some(&token), kinds).unwrap_err(),
            crate::build(KIND, &params("@abc"), Some(&token), kinds).unwrap_err(),
            crate::build(KIND, &BTreeMap::new(), Some(&token), kinds).unwrap_err(),
            crate::build(KIND, &extra, Some(&token), kinds).unwrap_err(),
            crate::build(KIND, &params("-100123"), None, kinds).unwrap_err(),
            crate::build(KIND, &params("-100123"), Some("not a token"), kinds).unwrap_err(),
        ];
        for refused in &refusals {
            assert!(!refused.contains(&token), "{refused}");
            assert!(!refused.contains("123456789:"), "{refused}");
        }
        assert!(refusals[0].contains("\"hello\""), "{}", refusals[0]);
        assert!(refusals[2].contains("chat"), "{}", refusals[2]);
        assert!(
            refusals[3].contains("\"parse_mode\"") && refusals[3].contains("chat"),
            "{}",
            refusals[3]
        );
        assert!(refusals[4].contains("secret"), "{}", refusals[4]);
    }
}
