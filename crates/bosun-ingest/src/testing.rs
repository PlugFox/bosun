//! Test helpers: the fixture corpus and the sums of `expected.json` (#16, `fixtures/README.md`).
//!
//! Repository attribution here is the rule of the fixtures' README (the longest configured
//! root that is the cwd or an ancestor of it, on whole path components), written again for the
//! parser tests: the store applies `bosun_core::repo_root`, which the store-backed tests check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bosun_core::{Message, Tokens, canonical_path};
use serde_json::Value;

use crate::normalise::style_of;

pub(crate) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Every file under `dir`, recursively, sorted.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The directories of the cases under `fixtures/<provider>/`.
pub(crate) fn cases(provider: &str) -> Vec<PathBuf> {
    let mut cases: Vec<PathBuf> = std::fs::read_dir(fixtures().join(provider))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    cases
}

/// `path` relative to `base`, with `/` separators: the file key of a fixture file.
pub(crate) fn relative(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// A message count and the five categories.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sums {
    messages: u64,
    tokens: Tokens,
}

impl Sums {
    fn add(&mut self, tokens: Tokens) {
        self.messages += 1;
        self.tokens.input += tokens.input;
        self.tokens.cache_write_5m += tokens.cache_write_5m;
        self.tokens.cache_write_1h += tokens.cache_write_1h;
        self.tokens.cache_read += tokens.cache_read;
        self.tokens.output += tokens.output;
    }

    fn from_json(value: &Value) -> Self {
        let n = |key: &str| {
            value[key]
                .as_u64()
                .unwrap_or_else(|| panic!("`{key}` in {value}"))
        };
        Self {
            messages: n("messages"),
            tokens: Tokens {
                input: n("input"),
                cache_write_5m: n("cache_write_5m"),
                cache_write_1h: n("cache_write_1h"),
                cache_read: n("cache_read"),
                output: n("output"),
            },
        }
    }
}

/// Sums per context window, per repository and in total, in the terms of `expected.json`.
/// Entries of zeros are left out, as the shape allows.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    sessions: BTreeMap<(String, Option<String>), Sums>,
    repos: BTreeMap<Option<String>, Sums>,
    total: Sums,
}

impl Totals {
    /// Adds `messages` messages of window `window` attributed to `repo`, with token sums
    /// `tokens`: one row of a `GROUP BY` over the store.
    pub(crate) fn add_rows(
        &mut self,
        window: (String, Option<String>),
        repo: Option<String>,
        messages: u64,
        tokens: Tokens,
    ) {
        for sums in [
            self.sessions.entry(window).or_default(),
            self.repos.entry(repo).or_default(),
            &mut self.total,
        ] {
            sums.messages += messages;
            sums.tokens.input += tokens.input;
            sums.tokens.cache_write_5m += tokens.cache_write_5m;
            sums.tokens.cache_write_1h += tokens.cache_write_1h;
            sums.tokens.cache_read += tokens.cache_read;
            sums.tokens.output += tokens.output;
        }
    }

    /// The same totals with each repository named by its canonical key, as the store keeps it.
    pub(crate) fn canonical_repos(mut self) -> Self {
        self.repos = std::mem::take(&mut self.repos)
            .into_iter()
            .map(|(repo, sums)| (repo.map(|r| canonical_path(&r, style_of(&r))), sums))
            .collect();
        self
    }

    pub(crate) fn drop_zeros(mut self) -> Self {
        let zero = Sums::default();
        self.sessions.retain(|_, sums| *sums != zero);
        self.repos.retain(|_, sums| *sums != zero);
        self
    }
}

pub(crate) fn roots(case: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(case.join("repos.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    value["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|root| root.as_str().unwrap().to_owned())
        .collect()
}

pub(crate) fn expected(case: &Path) -> Totals {
    let text = std::fs::read_to_string(case.join("expected.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["version"], 1);
    let optional = |v: &Value| v.as_str().map(str::to_owned);
    let mut totals = Totals {
        total: Sums::from_json(&value["total"]),
        ..Totals::default()
    };
    for entry in value["sessions"].as_array().unwrap() {
        let key = (
            entry["session"].as_str().unwrap().to_owned(),
            optional(&entry["agent"]),
        );
        totals.sessions.insert(key, Sums::from_json(entry));
    }
    for entry in value["repos"].as_array().unwrap() {
        totals
            .repos
            .insert(optional(&entry["repo"]), Sums::from_json(entry));
    }
    totals.drop_zeros()
}

/// The longest root that is `cwd` or an ancestor of it, on whole components, with paths
/// canonicalised by the platform their text implies.
fn repo_of<'a>(cwd: &str, roots: &'a [String]) -> Option<&'a String> {
    let style = style_of(cwd);
    let cwd = canonical_path(cwd, style);
    roots
        .iter()
        .filter(|root| style_of(root) == style)
        .filter(|root| {
            let root = canonical_path(root, style);
            let separator = if root.contains('\\') { '\\' } else { '/' };
            cwd == root
                || cwd
                    .strip_prefix(&root)
                    .is_some_and(|rest| root.ends_with(separator) || rest.starts_with(separator))
        })
        .max_by_key(|root| canonical_path(root, style).len())
}

pub(crate) fn totals(messages: &[Message], roots: &[String]) -> Totals {
    let mut totals = Totals::default();
    for message in messages {
        let tokens = message.usage.tokens;
        let window = &message.origin.window;
        totals
            .sessions
            .entry((window.session_id.clone(), window.agent_id.clone()))
            .or_default()
            .add(tokens);
        totals
            .repos
            .entry(repo_of(&message.cwd, roots).cloned())
            .or_default()
            .add(tokens);
        totals.total.add(tokens);
    }
    totals.drop_zeros()
}
