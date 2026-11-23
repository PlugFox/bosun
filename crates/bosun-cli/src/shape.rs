//! `recount`'s compare shape, version 1 (#16), which `bosun tokens --by session --format json`
//! emits so that `recount --compare` reads it as it is:
//!
//! ```json
//! {
//!   "version": 1,
//!   "sessions": [
//!     { "session": "<session id>", "agent": null, "messages": 2, "input": 7,
//!       "cache_write_5m": 90, "cache_write_1h": 1100, "cache_read": 13000, "output": 50010 }
//!   ],
//!   "repos": [
//!     { "repo": "<root as configured>", "messages": 2, ... },
//!     { "repo": null, ... }
//!   ],
//!   "total": { "messages": 2, ... }
//! }
//! ```
//!
//! `sessions` are the rows of `tokens.by_session`, `repos` the rows of `tokens.by_repo` keyed
//! by the root as configured (`null` for messages in no repository), and `total` the total of
//! `tokens.by_session`. The shape carries no cost and no eqv; the other `--by` give them.
//!
//! The two methods are two requests: while the daemon ingests a transcript that is still being
//! written, `repos` may hold a message `sessions` does not. Over settled transcripts they agree.

use bosun_api::{RepoRow, SessionRow, TokenSums};
use serde_json::{Map, Value, json};

/// The version `recount` reads.
pub(crate) const VERSION: u64 = 1;

fn counts(sums: &TokenSums, object: &mut Map<String, Value>) {
    for (key, n) in [
        ("messages", sums.messages),
        ("input", sums.input),
        ("cache_write_5m", sums.cache_write_5m),
        ("cache_write_1h", sums.cache_write_1h),
        ("cache_read", sums.cache_read),
        ("output", sums.output),
    ] {
        object.insert(key.to_owned(), n.into());
    }
}

fn entry(key: &str, value: Value, sums: &TokenSums) -> Value {
    let mut object = Map::new();
    object.insert(key.to_owned(), value);
    counts(sums, &mut object);
    Value::Object(object)
}

/// The version 1 document of one identity's sessions, repositories and total.
pub(crate) fn v1(sessions: &[SessionRow], repos: &[RepoRow], total: &TokenSums) -> Value {
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|row| {
            let mut value = entry("session", row.session.clone().into(), &row.sums);
            value["agent"] = row.agent.clone().map_or(Value::Null, Value::from);
            value
        })
        .collect();
    let repos: Vec<Value> = repos
        .iter()
        .map(|row| {
            let repo = row
                .repo
                .as_ref()
                .map_or(Value::Null, |r| r.path.clone().into());
            entry("repo", repo, &row.sums)
        })
        .collect();
    let mut sum = Map::new();
    counts(total, &mut sum);
    json!({
        "version": VERSION,
        "sessions": sessions,
        "repos": repos,
        "total": sum,
    })
}

#[cfg(test)]
mod tests {
    use bosun_api::{Picodollars, RepoRef};

    use super::*;

    fn sums(output: u64) -> TokenSums {
        TokenSums {
            messages: 1,
            input: 2,
            cache_write_5m: 3,
            cache_write_1h: 4,
            cache_read: 5,
            output,
            cost: Picodollars(7),
            eqv: 8.5,
            unpriced_messages: 0,
        }
    }

    #[test]
    fn the_document_has_exactly_the_shape() {
        let sessions = [
            SessionRow {
                session: "s".to_owned(),
                agent: None,
                sums: sums(6),
            },
            SessionRow {
                session: "s".to_owned(),
                agent: Some("a".to_owned()),
                sums: sums(1),
            },
        ];
        let repos = [
            RepoRow {
                repo: Some(RepoRef {
                    name: Some("x".to_owned()),
                    path: "/r".to_owned(),
                }),
                sums: sums(6),
            },
            RepoRow {
                repo: None,
                sums: sums(1),
            },
        ];
        let counts = |output: u64| {
            json!({"messages": 1, "input": 2, "cache_write_5m": 3, "cache_write_1h": 4,
                   "cache_read": 5, "output": output})
        };
        let with = |mut value: Value, key: &str, v: Value| {
            value[key] = v;
            value
        };
        let expected = json!({
            "version": 1,
            "sessions": [
                with(with(counts(6), "session", "s".into()), "agent", Value::Null),
                with(with(counts(1), "session", "s".into()), "agent", "a".into()),
            ],
            "repos": [
                with(counts(6), "repo", "/r".into()),
                with(counts(1), "repo", Value::Null),
            ],
            "total": counts(7),
        });
        assert_eq!(v1(&sessions, &repos, &sums(7)), expected);
    }
}
