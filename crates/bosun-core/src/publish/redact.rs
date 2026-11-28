//! The secret redactor (#5): token-shaped strings in text the agent influenced are replaced
//! before publication.
//!
//! Shapes, not knowledge: the redactor knows the formats of the common credentials (GitHub,
//! Anthropic, `OpenAI`, AWS, Slack, Google, GitLab, npm and Telegram bot tokens, JWTs, private
//! keys, credentials in URLs, `Authorization: Bearer`) and the `name = value` assignment of
//! anything named like a secret. It is a second line: the agent's environment holds no GitHub write credential (#2).

use std::sync::LazyLock;

use regex::Regex;

/// What a redacted secret is replaced with.
const REDACTED: &str = "[redacted]";

/// Each pattern, its replacement (`${keep}` and `${tail}` keep the parts around the secret) and
/// whether it is applied until the text no longer changes.
///
/// A pattern that consumes a character after the secret (its `tail`) repeats: `replace_all`
/// resumes after that character, so a second secret that needs it as its `keep` (`TOK1 TOK2`)
/// would stay in clear after one pass. It terminates: every replacement shortens the text.
static PATTERNS: LazyLock<Vec<(Regex, String, bool)>> = LazyLock::new(|| {
    let whole = |p: &str| {
        (
            Regex::new(p).expect("valid regex"),
            REDACTED.to_owned(),
            false,
        )
    };
    let keep = |p: &str| {
        (
            Regex::new(p).expect("valid regex"),
            format!("${{keep}}{REDACTED}${{tail}}"),
            false,
        )
    };
    let repeat = |p: &str| {
        let (regex, replacement, _) = keep(p);
        (regex, replacement, true)
    };
    #[allow(unused_mut, reason = "the mutant below changes it")]
    let mut patterns = vec![
        whole(
            r"-----BEGIN[A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END[A-Z0-9 ]*PRIVATE KEY-----|\z)",
        ),
        whole(r"\bgh[pousr]_[A-Za-z0-9]{30,}"),
        whole(r"\bgithub_pat_[A-Za-z0-9_]{30,}"),
        whole(r"\bsk-[A-Za-z0-9_-]{20,}"),
        whole(r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
        whole(r"\bAIza[0-9A-Za-z_-]{35,}"),
        whole(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        whole(r"\bglpat-[A-Za-z0-9_-]{20,}"),
        whole(r"\bnpm_[A-Za-z0-9]{36}"),
        whole(r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"),
        keep(r"(?i)(?P<keep>\bbearer\s+)[A-Za-z0-9._~+/=-]{16,}"),
        keep(r"(?i)(?P<keep>\b[a-z][a-z0-9+.-]*://[^\s:/@]+:)[^\s@/]+(?P<tail>@)"),
        keep(concat!(
            r"(?i)(?P<keep>\b[a-z0-9_.-]*(?:password|passwd|pwd|secret|token|api[_-]?key|",
            r#"access[_-]?key|private[_-]?key|credential)[a-z0-9_.-]*["']?\s*[:=]\s*["']?)"#,
            r#"[^\s"'`,;:][^\s"'`,;]{5,}"#,
        )),
    ];
    // Mutant (`mutant_redact_skip_telegram_token`): the Telegram bot-token shape is left out.
    if !cfg!(mutant_redact_skip_telegram_token) {
        // A Telegram bot token: the bot's id, a colon, 35 characters. Its context is explicit,
        // not `\b`: in a Bot API URL it follows `bot` (`t` then a digit is no word boundary),
        // and its last character may be `-` or `_` (then neither `/`, a space nor the end is
        // one). `bot` and what follows survive: `.../bot[redacted]/sendMessage`.
        // Mutant (`mutant_redact_telegram_word_boundary`): the `\b` form is back.
        patterns.push(if cfg!(mutant_redact_telegram_word_boundary) {
            whole(r"\b\d{6,12}:[A-Za-z0-9_-]{35}\b")
        } else {
            repeat(r"(?P<keep>^|[^A-Za-z0-9]|bot)[0-9]{6,12}:[A-Za-z0-9_-]{35}(?P<tail>[^A-Za-z0-9_-]|$)")
        });
    }
    patterns
});

/// `text` with every token-shaped string replaced by `[redacted]`.
#[must_use]
pub fn redact(text: &str) -> String {
    #[cfg(mutant_publish_no_redact)]
    {
        text.to_owned()
    }
    #[cfg(not(mutant_publish_no_redact))]
    {
        let mut out = text.to_owned();
        for (pattern, replacement, repeat) in PATTERNS.iter() {
            // Mutant (`mutant_redact_telegram_single_pass`): every pattern is applied once, so
            // the second of two tokens one character apart stays in clear.
            let repeat = *repeat && !cfg!(mutant_redact_telegram_single_pass);
            loop {
                let next = pattern.replace_all(&out, replacement.as_str());
                let std::borrow::Cow::Owned(next) = next else {
                    break;
                };
                out = next;
                if !repeat {
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic secrets of each shape, none of them a real credential, as a prefix and a body
    /// joined at run time: the source holds no token-shaped literal for a secret scanner to
    /// report.
    const SECRETS: &[(&str, &str)] = &[
        ("ghp", "_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"),
        ("gho", "_0123456789abcdefghijABCDEFGHIJ012345"),
        (
            "github_pat",
            "_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
        ),
        ("sk-ant", "-api03-abcdefghijklmnopqrstuvwxyz0123456789"),
        ("sk-proj", "-abcdefghijklmnopqrstuvwxyz0123"),
        ("xoxb", "-1234567890-abcdefghij"),
        ("AIza", "SyA-abcdefghijklmnopqrstuvwxyz012345"),
        ("AKIA", "ABCDEFGHIJKLMNOP"),
        ("glpat", "-abcdefghijklmnopqrst"),
        ("npm", "_abcdefghijklmnopqrstuvwxyz0123456789"),
        (
            "eyJzeW50aGV0aWMi",
            ".eyJzeW50aGV0aWMi.c3ludGhldGljLXNpZ25hdHVyZQ",
        ),
    ];

    #[test]
    fn every_token_shape_is_redacted() {
        for (prefix, body) in SECRETS {
            let secret = format!("{prefix}{body}");
            let out = redact(&format!("before {secret} after"));
            assert_eq!(out, "before [redacted] after", "{secret}");
        }
    }

    #[test]
    fn assignments_bearer_headers_url_credentials_and_keys_are_redacted() {
        assert_eq!(
            redact("GITHUB_TOKEN=abcdef123456 next"),
            "GITHUB_TOKEN=[redacted] next"
        );
        assert_eq!(
            redact(r#"{"api_key": "abcdefgh1234"}"#),
            r#"{"api_key": "[redacted]"}"#
        );
        assert_eq!(
            redact("Authorization: Bearer abcdefghijklmnop1234"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(
            redact("cloning https://user:hunter2secret@example.com/r.git"),
            "cloning https://user:[redacted]@example.com/r.git"
        );
        assert_eq!(
            redact(&format!(
                "a\n-----BEGIN RSA {k}-----\nMIIsynthetic\n-----END RSA {k}-----\nb",
                k = "PRIVATE KEY"
            )),
            "a\n[redacted]\nb"
        );
    }

    #[test]
    fn a_telegram_bot_token_is_redacted() {
        let token = format!("123456789:{}", "A".repeat(35));
        assert_eq!(
            redact(&format!("the bot {token} end")),
            "the bot [redacted] end"
        );
        assert_eq!(redact(&token), "[redacted]");
        let dashed = telegram_token("-");
        assert_eq!(
            redact(&format!("the bot {dashed} end")),
            "the bot [redacted] end"
        );
        assert_eq!(redact(&format!("({dashed})")), "([redacted])");
        let text = "lunch at 12:30, id 123456789:short";
        assert_eq!(redact(text), text);
        let longer = format!("123456789:{}", "A".repeat(36));
        assert_eq!(redact(&longer), longer);
    }

    /// A synthetic bot token whose 35 characters end in `last`, joined at run time as
    /// [`SECRETS`] are.
    fn telegram_token(last: &str) -> String {
        format!("{}:{}{last}", "123456789", "A".repeat(35 - last.len()))
    }

    #[test]
    fn a_telegram_token_inside_a_bot_url_is_redacted() {
        for last in ["A", "-", "_"] {
            let token = telegram_token(last);
            assert_eq!(
                redact(&format!(
                    "POST https://api.telegram.org/bot{token}/sendMessage failed"
                )),
                "POST https://api.telegram.org/bot[redacted]/sendMessage failed",
                "{token}"
            );
            assert_eq!(redact(&format!("token {token}")), "token [redacted]");
        }
    }

    #[test]
    fn two_telegram_tokens_one_character_apart_are_both_redacted() {
        let first = telegram_token("A");
        let second = telegram_token("-");
        for (text, expected) in [
            (
                format!("{first} {second}"),
                "[redacted] [redacted]".to_owned(),
            ),
            (
                format!("{first}\n{second}"),
                "[redacted]\n[redacted]".to_owned(),
            ),
            (
                format!("a,{first},{second}"),
                "a,[redacted],[redacted]".to_owned(),
            ),
        ] {
            assert_eq!(redact(&text), expected, "{text:?}");
        }
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "test tokens::lexer ... ok\ncommit 0123456789abcdef\nskip-ci and sk-short";
        assert_eq!(redact(text), text);
        assert_eq!(redact(&redact("TOKEN=abcdef123456")), "TOKEN=[redacted]");
    }
}
