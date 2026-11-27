//! What a queued notice says (#9, "Content is ids, states and numbers by default"; #208).
//!
//! A [`Notice`] is what one row of the identity's notification outbox holds: the event, the
//! repository, the item and a few facts the daemon wrote (the item machine's reason, the
//! attempt, the breaker's figures). It has no field for a card's title or for anything an agent
//! wrote, so no message can carry one. [`render`] turns one notice, or a coalesced burst of
//! them, into the plain text of one message: no markup mode, capped, and passed through
//! `bosun_core::publish::scrub` as a whole.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One queued notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// `needs_you`, `repo_paused`, `ready_drained` or `resource_unhealthy`.
    pub event: String,
    /// The repository's configuration name.
    pub repo: String,
    /// The item id (`demo#4`), for an event of an item.
    pub item: Option<String>,
    /// The event's numbers and words, as the daemon wrote them: `reason` and `attempt` of
    /// `needs_you`; `sessions`, `within_s` and `item` of `repo_paused`; `resource` (the
    /// instance) and `failure` (`exit <code>`, `timeout` or `cannot start`, never the probe's
    /// output) of `resource_unhealthy`.
    pub facts: BTreeMap<String, String>,
}

impl Notice {
    fn fact(&self, key: &str) -> &str {
        self.facts.get(key).map_or("?", String::as_str)
    }

    /// The notice in one line, without the `bosun <identity>:` prefix.
    fn line(&self) -> String {
        let item = self.item.as_deref().unwrap_or("?");
        match self.event.as_str() {
            "needs_you" => {
                let mut line = format!("{item} needs you ({})", self.fact("reason"));
                if let Some(attempt) = self.facts.get("attempt").filter(|a| *a != "0") {
                    line.push_str(", attempt ");
                    line.push_str(attempt);
                }
                line
            }
            "repo_paused" => format!(
                "repository {} paused: {} sessions in a row ended within {} s of their spawn \
                 (the last: {})",
                self.repo,
                self.fact("sessions"),
                self.fact("within_s"),
                self.facts.get("item").map_or(item, String::as_str),
            ),
            "ready_drained" => format!("repository {}: Ready drained after {item}", self.repo),
            "resource_unhealthy" => format!(
                "resource {} fails its probe ({}); {item} waits",
                self.fact("resource"),
                self.fact("failure"),
            ),
            other => match &self.item {
                Some(item) => format!("repository {}: {other} ({item})", self.repo),
                None => format!("repository {}: {other}", self.repo),
            },
        }
    }
}

/// The text of one message carrying `notices` (oldest first) of `identity`, at most `cap`
/// characters.
///
/// One notice is one line: `bosun <identity>: demo#4 needs you (lint), attempt 2`. Several (a
/// burst coalesced into one message) are a first line `bosun <identity>: <n> notices`, then one
/// line each, oldest first, as many as fit, and a last line `... and <k> more` for the rest.
#[must_use]
pub fn render(identity: &str, notices: &[Notice], cap: usize) -> String {
    let lines: Vec<String> = notices
        .iter()
        .map(|n| bosun_core::publish::scrub(&n.line()))
        .collect();
    let text = if let [one] = lines.as_slice() {
        format!("bosun {identity}: {one}")
    } else {
        let mut text = format!("bosun {identity}: {} notices", lines.len());
        let len = |s: &str| s.chars().count();
        let mut used = len(&text);
        let mut shown = 0;
        for (i, line) in lines.iter().enumerate() {
            let rest = lines.len() - i - 1;
            let more = if rest == 0 {
                0
            } else {
                1 + len(&format!("... and {rest} more"))
            };
            if used + 1 + len(line) + more > cap {
                break;
            }
            text.push('\n');
            text.push_str(line);
            used += 1 + len(line);
            shown += 1;
        }
        if shown < lines.len() {
            let _ = write!(text, "\n... and {} more", lines.len() - shown);
        }
        text
    };
    let text = bosun_core::publish::scrub(&text);
    if text.chars().count() <= cap {
        text
    } else {
        text.chars().take(cap).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs_you(item: &str, reason: &str, attempt: u32) -> Notice {
        Notice {
            event: "needs_you".to_owned(),
            repo: "demo".to_owned(),
            item: Some(item.to_owned()),
            facts: BTreeMap::from([
                ("reason".to_owned(), reason.to_owned()),
                ("attempt".to_owned(), attempt.to_string()),
            ]),
        }
    }

    #[test]
    fn a_message_carries_ids_states_and_numbers_only() {
        assert_eq!(
            render("a", &[needs_you("demo#4", "lint", 2)], 3_500),
            "bosun a: demo#4 needs you (lint), attempt 2"
        );

        let four: Vec<Notice> = (1..=4)
            .map(|i| needs_you(&format!("demo#{i}"), "lint", 1))
            .collect();
        let two_lines = "bosun a: 4 notices\n\
                         demo#1 needs you (lint), attempt 1\n\
                         demo#2 needs you (lint), attempt 1\n\
                         ... and 2 more";
        let cap = two_lines.chars().count();
        assert_eq!(render("a", &four, cap), two_lines);
        assert_eq!(
            render("a", &four, cap + 5),
            two_lines,
            "a third line does not fit"
        );

        let paused = Notice {
            event: "repo_paused".to_owned(),
            repo: "demo".to_owned(),
            item: None,
            facts: BTreeMap::from([
                ("sessions".to_owned(), "3".to_owned()),
                ("within_s".to_owned(), "60".to_owned()),
                ("item".to_owned(), "demo#1".to_owned()),
            ]),
        };
        assert_eq!(
            render("a", &[paused], 3_500),
            "bosun a: repository demo paused: 3 sessions in a row ended within 60 s of their \
             spawn (the last: demo#1)"
        );
        let drained = Notice {
            event: "ready_drained".to_owned(),
            repo: "demo".to_owned(),
            item: Some("demo#7".to_owned()),
            facts: BTreeMap::new(),
        };
        assert_eq!(
            render("a", &[drained], 3_500),
            "bosun a: repository demo: Ready drained after demo#7"
        );
        let unhealthy = Notice {
            event: "resource_unhealthy".to_owned(),
            repo: "demo".to_owned(),
            item: Some("demo#4".to_owned()),
            facts: BTreeMap::from([
                ("resource".to_owned(), "rig".to_owned()),
                ("failure".to_owned(), "exit 1".to_owned()),
            ]),
        };
        assert_eq!(
            render("a", &[unhealthy], 3_500),
            "bosun a: resource rig fails its probe (exit 1); demo#4 waits"
        );

        let token = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let leaky = needs_you("demo#5", &token, 1);
        let text = render("a", &[leaky], 3_500);
        assert_eq!(text, "bosun a: demo#5 needs you ([redacted]), attempt 1");
        assert!(!text.contains("ghp_"), "{text}");
    }
}
