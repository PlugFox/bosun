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

/// Each pattern and its replacement; `${keep}` and `${tail}` keep the parts around the secret.
static PATTERNS: LazyLock<Vec<(Regex, String)>> = LazyLock::new(|| {
    let whole = |p: &str| (Regex::new(p).expect("valid regex"), REDACTED.to_owned());
    let keep = |p: &str| {
        (
            Regex::new(p).expect("valid regex"),
            format!("${{keep}}{REDACTED}${{tail}}"),
        )
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
        // A Telegram bot token: the bot's id, a colon, 35 characters.
        patterns.push(whole(r"\b\d{6,12}:[A-Za-z0-9_-]{35}\b"));
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
        for (pattern, replacement) in PATTERNS.iter() {
            out = pattern.replace_all(&out, replacement.as_str()).into_owned();
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
        let text = "lunch at 12:30, id 123456789:short";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "test tokens::lexer ... ok\ncommit 0123456789abcdef\nskip-ci and sk-short";
        assert_eq!(redact(text), text);
        assert_eq!(redact(&redact("TOKEN=abcdef123456")), "TOKEN=[redacted]");
    }
}
