//! The JSON shape of a recount, and the comparison of two of them.
//!
//! ```json
//! {
//!   "version": 1,
//!   "sessions": [
//!     { "session": "<session id>", "agent": null, "messages": 2, "input": 7,
//!       "cache_write_5m": 90, "cache_write_1h": 1100, "cache_read": 13000, "output": 50010 },
//!     { "session": "<session id>", "agent": "<agent id>", "messages": 1, ... }
//!   ],
//!   "repos": [
//!     { "repo": "<root as configured>", "messages": 2, "input": 7, ... },
//!     { "repo": null, ... }
//!   ],
//!   "total": { "messages": 3, "input": 9, ... }
//! }
//! ```
//!
//! - `sessions` has one entry per context window, that is per transcript file: the main
//!   transcript of a session (`agent: null`) or one of its sub-agents (`agent: "<agent id>"`).
//!   A message is counted in the window where its id appears earliest.
//! - `repos` has one entry per repository root with at least one message; `repo: null` gathers
//!   messages whose `cwd` is under no root.
//! - Every count is a JSON integer. `messages` counts API messages (distinct message ids), not
//!   records. The five categories are those of #3.
//! - Order is not significant when comparing: recount writes sessions sorted by session then
//!   agent (`null` first) and repos sorted by root with `null` last.
//! - When comparing, an entry missing on one side is equal to an entry of zeros, so a producer
//!   may omit empty windows and repositories.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::count::{CATEGORIES, Report, Sums, Window};

pub(crate) const VERSION: u64 = 1;

fn sums_fields(sums: &Sums, object: &mut Map<String, Value>) {
    object.insert("messages".to_owned(), sums.messages.into());
    for (name, n) in CATEGORIES.iter().zip(sums.tokens) {
        object.insert((*name).to_owned(), n.into());
    }
}

fn entry(key: &str, value: Value, sums: &Sums) -> Value {
    let mut object = Map::new();
    object.insert(key.to_owned(), value);
    sums_fields(sums, &mut object);
    Value::Object(object)
}

/// The report as the JSON value described in the module documentation.
pub(crate) fn to_json(report: &Report) -> Value {
    let sessions: Vec<Value> = report
        .sessions
        .iter()
        .map(|(window, sums)| {
            let mut v = entry("session", window.session.clone().into(), sums);
            v["agent"] = window.agent.clone().map_or(Value::Null, Value::from);
            v
        })
        .collect();
    let named = report
        .repos
        .iter()
        .filter_map(|(repo, sums)| repo.as_ref().map(|r| entry("repo", r.clone().into(), sums)));
    let unnamed = report
        .repos
        .get(&None)
        .map(|sums| entry("repo", Value::Null, sums));
    let repos: Vec<Value> = named.chain(unnamed).collect();
    let mut total = Map::new();
    sums_fields(&report.total, &mut total);
    json!({
        "version": VERSION,
        "sessions": sessions,
        "repos": repos,
        "total": total,
    })
}

/// A parsed recount, keyed for comparison.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    sessions: BTreeMap<Window, Sums>,
    repos: BTreeMap<Option<String>, Sums>,
    total: Sums,
}

fn sums_from(object: &Map<String, Value>, at: &str) -> Result<Sums, String> {
    let count = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{at}: `{key}` is missing or not a non-negative integer"))
    };
    let mut sums = Sums {
        messages: count("messages")?,
        ..Sums::default()
    };
    for (slot, name) in sums.tokens.iter_mut().zip(CATEGORIES) {
        *slot = count(name)?;
    }
    Ok(sums)
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    at: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(format!("{at}: `{key}` must be a string or null")),
    }
}

fn array<'a>(root: &'a Map<String, Value>, key: &str) -> Result<&'a Vec<Value>, String> {
    root.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{key}` is missing or not an array"))
}

/// Reads a value of the documented shape.
pub(crate) fn parse(value: &Value) -> Result<Parsed, String> {
    let root = value.as_object().ok_or("the document is not an object")?;
    match root.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        other => return Err(format!("`version` is {other:?}, expected {VERSION}")),
    }
    let mut parsed = Parsed::default();
    for (i, item) in array(root, "sessions")?.iter().enumerate() {
        let at = format!("sessions[{i}]");
        let object = item.as_object().ok_or(format!("{at} is not an object"))?;
        let session =
            optional_string(object, "session", &at)?.ok_or(format!("{at}: `session` is null"))?;
        let agent = optional_string(object, "agent", &at)?;
        let window = Window { session, agent };
        if parsed
            .sessions
            .insert(window, sums_from(object, &at)?)
            .is_some()
        {
            return Err(format!("{at} repeats a session and agent"));
        }
    }
    for (i, item) in array(root, "repos")?.iter().enumerate() {
        let at = format!("repos[{i}]");
        let object = item.as_object().ok_or(format!("{at} is not an object"))?;
        let repo = optional_string(object, "repo", &at)?;
        if parsed.repos.insert(repo, sums_from(object, &at)?).is_some() {
            return Err(format!("{at} repeats a repository"));
        }
    }
    let total = root
        .get("total")
        .and_then(Value::as_object)
        .ok_or("`total` is missing or not an object")?;
    parsed.total = sums_from(total, "total")?;
    Ok(parsed)
}

fn compare_sums(label: &str, ours: Sums, theirs: Sums, out: &mut Vec<String>) {
    let fields = std::iter::once(("messages", ours.messages, theirs.messages)).chain(
        CATEGORIES
            .iter()
            .zip(ours.tokens.iter().zip(theirs.tokens))
            .map(|(name, (a, b))| (*name, *a, b)),
    );
    for (name, a, b) in fields {
        if a != b {
            out.push(format!("{label}: {name}: recount {a}, compared {b}"));
        }
    }
}

fn compare_maps<K: Ord + Clone>(
    ours: &BTreeMap<K, Sums>,
    theirs: &BTreeMap<K, Sums>,
    label: impl Fn(&K) -> String,
    out: &mut Vec<String>,
) {
    let mut keys: Vec<&K> = ours.keys().chain(theirs.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let a = ours.get(key).copied().unwrap_or_default();
        let b = theirs.get(key).copied().unwrap_or_default();
        compare_sums(&label(key), a, b, out);
    }
}

/// Every difference between two recounts, one line each. Empty means they agree.
pub(crate) fn differences(ours: &Parsed, theirs: &Parsed) -> Vec<String> {
    let mut out = Vec::new();
    compare_maps(
        &ours.sessions,
        &theirs.sessions,
        |w| {
            let mut s = format!("session {}", w.session);
            if let Some(agent) = &w.agent {
                let _ = write!(s, " agent {agent}");
            }
            s
        },
        &mut out,
    );
    compare_maps(
        &ours.repos,
        &theirs.repos,
        |r| format!("repo {}", r.as_deref().unwrap_or("(none)")),
        &mut out,
    );
    compare_sums("total", ours.total, theirs.total, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(output: u64, extra_session: bool) -> Value {
        let mut sessions = vec![
            json!({"session": "s", "agent": null, "messages": 1, "input": 1,
            "cache_write_5m": 0, "cache_write_1h": 0, "cache_read": 0, "output": output}),
        ];
        if extra_session {
            sessions.push(
                json!({"session": "s", "agent": "a", "messages": 0, "input": 0,
                "cache_write_5m": 0, "cache_write_1h": 0, "cache_read": 0, "output": 0}),
            );
        }
        json!({"version": 1, "sessions": sessions,
            "repos": [{"repo": null, "messages": 1, "input": 1, "cache_write_5m": 0,
                "cache_write_1h": 0, "cache_read": 0, "output": output}],
            "total": {"messages": 1, "input": 1, "cache_write_5m": 0, "cache_write_1h": 0,
                "cache_read": 0, "output": output}})
    }

    #[test]
    fn equal_documents_have_no_differences() {
        let a = parse(&doc(5, false)).unwrap();
        assert_eq!(differences(&a, &a), Vec::<String>::new());
    }

    #[test]
    fn a_missing_entry_equals_zeros() {
        let a = parse(&doc(5, false)).unwrap();
        let b = parse(&doc(5, true)).unwrap();
        assert_eq!(differences(&a, &b), Vec::<String>::new());
    }

    #[test]
    fn every_difference_is_listed() {
        let a = parse(&doc(5, false)).unwrap();
        let b = parse(&doc(6, false)).unwrap();
        assert_eq!(
            differences(&a, &b),
            [
                "session s: output: recount 5, compared 6",
                "repo (none): output: recount 5, compared 6",
                "total: output: recount 5, compared 6",
            ]
        );
    }

    #[test]
    fn malformed_documents_are_refused() {
        let mut bad = doc(5, false);
        bad["version"] = 2.into();
        assert!(parse(&bad).is_err());
        let mut bad = doc(5, false);
        bad["total"]["output"] = "5".into();
        assert!(parse(&bad).is_err());
        let bad = doc(5, true);
        let mut twice = bad.clone();
        twice["sessions"][1]["agent"] = Value::Null;
        assert!(parse(&twice).is_err());
    }
}
