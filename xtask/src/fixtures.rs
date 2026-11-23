//! `cargo xtask lint-fixtures` (#16): everything under `fixtures/` is synthetic.
//!
//! The repository is public, so a real transcript, path or id must never land in a fixture.
//! Every id follows a synthetic template and every path sits under a synthetic root; anything
//! else fails. The templates are documented in `fixtures/README.md`; this file is their
//! executable form. Two passes run over every file except Markdown:
//!
//! 1. A text scan of every line (and of the file's own path) for anything shaped like an id, a
//!    timestamp, an absolute path, a home directory or an e-mail address. Each one found must
//!    match its template. The scan sees lines that are not JSON too, such as a torn last line.
//! 2. A structural check of the JSON values (each line of a `.jsonl` file, the whole of a
//!    `.json` file): fields that hold ids, paths and timestamps must match their template even
//!    when the value is not shaped like a real one.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// The templates.
static UUID: LazyLock<Regex> = LazyLock::new(|| re(r"^00000000-0000-4000-8000-[0-9]{12}$"));
static PREFIXED: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:msg|req|toolu|srvtoolu)_0{16}[0-9]{8}$"));
static AGENT: LazyLock<Regex> = LazyLock::new(|| re(r"^a0{8}[0-9]{8}$"));
static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| re(r"^2000-01-[0-3][0-9]T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$"));
/// Epoch milliseconds of January 2000, the only month fixtures live in.
const EPOCH_MS: std::ops::Range<u64> = 946_684_800_000..949_363_200_000;
/// Project directory names: Claude Code's encoding of a cwd under a synthetic root.
static PROJECT: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:-fixture|C--fixture)(?:-.*)?$"));

/// What the text scan looks for.
static ANY_UUID: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"));
static ANY_PREFIXED: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b(?:msg|req|toolu|srvtoolu)_[A-Za-z0-9]+"));
static ANY_AGENT: LazyLock<Regex> = LazyLock::new(|| re(r"\ba[0-9a-f]{16}\b"));
static ANY_DATE: LazyLock<Regex> = LazyLock::new(|| re(r"\b([0-9]{4}-[0-9]{2})-[0-9]{2}T"));
static ANY_EPOCH_MS: LazyLock<Regex> = LazyLock::new(|| re(r"\b1[0-9]{12}\b"));
static ANY_DRIVE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"\b([A-Za-z]):(?:\\\\|\\|/)+([^\\/"'\s]*)"#));
static ANY_ROOTED: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:^|[\s"'=(\[,])/([A-Za-z0-9._~-]+)"#));
static ANY_HOME: LazyLock<Regex> = LazyLock::new(|| re(r"~[\\/]"));
static ANY_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| re(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)"));
const EMAIL_DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "example.net",
    "example.invalid",
];

/// Object keys whose string value is a path.
const PATH_KEYS: &[&str] = &[
    "cwd",
    "file_path",
    "filePath",
    "notebook_path",
    "path",
    "realParentDir",
    "repo",
    "trackingPath",
];

fn is_id_key(key: &str) -> bool {
    matches!(key, "id" | "uuid" | "session" | "agent")
        || ["Id", "ID", "_id", "Uuid", "UUID"]
            .iter()
            .any(|suffix| key.ends_with(suffix))
}

fn is_template_id(value: &str) -> bool {
    UUID.is_match(value) || PREFIXED.is_match(value) || AGENT.is_match(value)
}

/// Under `/fixture` or `C:\fixture` (either separator).
fn is_synthetic_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    ["/fixture", "C:/fixture"]
        .iter()
        .any(|root| p == *root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/')))
}

/// Problems in one line of text, from the text scan.
fn scan_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in ANY_UUID.find_iter(text) {
        if !UUID.is_match(m.as_str()) {
            out.push(format!("uuid {:?} is not a template id", m.as_str()));
        }
    }
    for m in ANY_PREFIXED.find_iter(text) {
        if !PREFIXED.is_match(m.as_str()) {
            out.push(format!("id {:?} is not a template id", m.as_str()));
        }
    }
    for m in ANY_AGENT.find_iter(text) {
        if !AGENT.is_match(m.as_str()) {
            out.push(format!("agent id {:?} is not a template id", m.as_str()));
        }
    }
    for c in ANY_DATE.captures_iter(text) {
        if &c[1] != "2000-01" {
            out.push(format!("timestamp in {:?} is not in 2000-01", &c[1]));
        }
    }
    for m in ANY_EPOCH_MS.find_iter(text) {
        out.push(format!(
            "{:?} looks like real epoch milliseconds",
            m.as_str()
        ));
    }
    for c in ANY_DRIVE.captures_iter(text) {
        if &c[1] != "C" || &c[2] != "fixture" {
            out.push(format!("path {:?} is not under C:\\fixture", &c[0]));
        }
    }
    for c in ANY_ROOTED.captures_iter(text) {
        if &c[1] != "fixture" {
            out.push(format!("path /{} is not under /fixture", &c[1]));
        }
    }
    if ANY_HOME.is_match(text) {
        out.push("a home-relative path (~/)".to_owned());
    }
    for c in ANY_EMAIL.captures_iter(text) {
        if !EMAIL_DOMAINS.contains(&&c[1]) {
            out.push(format!("e-mail address at {:?}", &c[1]));
        }
    }
    out
}

/// Problems in a JSON value, from the structural check.
fn check_value(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, v) in object {
                match (key.as_str(), v) {
                    (k, Value::String(s)) if PATH_KEYS.contains(&k) && !is_synthetic_path(s) => {
                        out.push(format!("`{k}` {s:?} is not under a synthetic root"));
                    }
                    (k, Value::String(s)) if is_id_key(k) && !is_template_id(s) => {
                        out.push(format!("`{k}` {s:?} is not a template id"));
                    }
                    ("timestamp", Value::String(s)) if !TIMESTAMP.is_match(s) => {
                        out.push(format!("`timestamp` {s:?} is not a template timestamp"));
                    }
                    ("startTime", Value::Number(n))
                        if !n.as_u64().is_some_and(|ms| EPOCH_MS.contains(&ms)) =>
                    {
                        out.push(format!("`startTime` {n} is not in 2000-01"));
                    }
                    ("roots", Value::Array(roots)) => {
                        for root in roots {
                            if !root.as_str().is_some_and(is_synthetic_path) {
                                out.push(format!("root {root} is not under a synthetic root"));
                            }
                        }
                    }
                    _ => check_value(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| check_value(v, out)),
        _ => {}
    }
}

/// Problems in a file's path relative to the fixtures directory, `/`-separated.
fn check_path(rel: &str, is_dir: bool) -> Vec<String> {
    let mut out = scan_text(rel);
    let parts: Vec<&str> = rel.split('/').collect();
    let Some(at) = parts.iter().position(|p| *p == "projects") else {
        return out;
    };
    let below = &parts[at + 1..];
    if let Some(project) = below.first()
        && !PROJECT.is_match(project)
    {
        out.push(format!(
            "project directory {project:?} does not encode a synthetic root"
        ));
    }
    let name = |n: &str| -> bool {
        n.strip_prefix("agent-")
            .map_or_else(|| UUID.is_match(n), |agent| AGENT.is_match(agent))
    };
    match below {
        [_, file] if !is_dir => {
            if !file
                .strip_suffix(".jsonl")
                .is_some_and(|s| UUID.is_match(s))
            {
                out.push(format!("transcript {file:?} is not <template uuid>.jsonl"));
            }
        }
        [_, dir, ..] if !UUID.is_match(dir) => {
            out.push(format!("session directory {dir:?} is not a template uuid"));
        }
        [_, _, _, file] if !is_dir => {
            let stem = file
                .strip_suffix(".meta.json")
                .or_else(|| file.strip_suffix(".jsonl"));
            if !stem.is_some_and(name) {
                out.push(format!("file {file:?} is not named after a template id"));
            }
        }
        _ => {}
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, bool)>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        let is_dir = path.is_dir();
        out.push((path.clone(), is_dir));
        if is_dir {
            walk(&path, out)?;
        }
    }
    Ok(())
}

/// Every problem under `dir`, one line each. Empty means the fixtures are synthetic.
pub(crate) fn lint(dir: &Path) -> Result<Vec<String>, String> {
    if cfg!(mutant_fixture_lint_off) {
        // Mutant: the lint accepts everything.
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    walk(dir, &mut entries)?;
    let mut out = Vec::new();
    for (path, is_dir) in entries {
        let rel = path
            .strip_prefix(dir)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let mut report = |line: Option<usize>, found: Vec<String>| {
            let at = line.map_or_else(|| rel.clone(), |n| format!("{rel}:{n}"));
            out.extend(found.into_iter().map(|f| format!("{at}: {f}")));
        };
        report(None, check_path(&rel, is_dir));
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if is_dir || extension == "md" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            report(None, vec!["not UTF-8 text".to_owned()]);
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            report(Some(n + 1), scan_text(line));
            if extension == "jsonl"
                && let Ok(value) = serde_json::from_str::<Value>(line)
            {
                let mut found = Vec::new();
                check_value(&value, &mut found);
                report(Some(n + 1), found);
            }
        }
        if extension == "json" {
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    let mut found = Vec::new();
                    check_value(&value, &mut found);
                    report(None, found);
                }
                Err(e) => report(None, vec![format!("not JSON: {e}")]),
            }
        }
    }
    Ok(out)
}

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let dir = match args {
        [] => crate::mutants::workspace_root().join("fixtures"),
        [dir] => PathBuf::from(dir),
        _ => return Err("lint-fixtures takes at most one directory".to_owned()),
    };
    let found = lint(&dir)?;
    if found.is_empty() {
        println!("fixtures are synthetic ({})", dir.display());
        return Ok(());
    }
    for line in &found {
        eprintln!("  {line}");
    }
    Err(format!("{} fixture problem(s)", found.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("bosun-lint-fixtures-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SESSION: &str = "00000000-0000-4000-8000-000100000000";

    /// One record in the shape of an `assistant` transcript line, with every value synthetic.
    fn record() -> Value {
        serde_json::json!({
            "parentUuid": null,
            "cwd": "/fixture/repo-a",
            "sessionId": SESSION,
            "type": "assistant",
            "uuid": "00000000-0000-4000-8000-100100000001",
            "timestamp": "2000-01-01T00:00:01.000Z",
            "requestId": "req_000000000000000000100001",
            "message": {
                "id": "msg_000000000000000000100001",
                "content": [{"type": "tool_use", "id": "toolu_000000000000000000190001",
                    "name": "Read", "input": {"file_path": "C:\\fixture\\repo-a\\src\\lib.rs"}}],
                "usage": {"input_tokens": 1, "output_tokens": 2}
            }
        })
    }

    /// Writes a fixture case holding `line` as its only transcript line.
    fn fixture(name: &str, line: &str) -> TempDir {
        let dir = TempDir::new(name);
        let project = dir.0.join("claude/case/projects/-fixture-repo-a");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{SESSION}.jsonl")),
            format!("{line}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.0.join("claude/case/repos.json"),
            r#"{"roots": ["/fixture/repo-a"]}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_good_fixture_passes() {
        let dir = fixture("good", &record().to_string());
        assert_eq!(lint(&dir.0).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn the_committed_fixtures_pass() {
        let found = lint(&crate::mutants::workspace_root().join("fixtures")).unwrap();
        assert_eq!(found, Vec::<String>::new());
    }

    /// Each edit makes the good record real-looking in one way; each must be rejected. The
    /// values are assembled at run time so this source holds none of them whole.
    #[test]
    fn a_bad_fixture_is_rejected() {
        type Edit = Box<dyn Fn(&mut Value)>;
        let hex = "9f3c".repeat(8);
        let real_uuid = format!(
            "{}-{}-4{}-a{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..15],
            &hex[15..18],
            &hex[18..30]
        );
        let edits: Vec<(&str, Edit)> = vec![
            (
                "message id",
                Box::new(|r| r["message"]["id"] = format!("msg_01{}", "Xk7".repeat(8)).into()),
            ),
            ("session id", {
                let id = real_uuid.clone();
                Box::new(move |r| r["sessionId"] = id.clone().into())
            }),
            (
                "free-form id",
                Box::new(|r| r["uuid"] = "session-of-someone".into()),
            ),
            (
                "agent id",
                Box::new(|r| r["agentId"] = format!("a{}", "4be1".repeat(4)).into()),
            ),
            (
                "posix path",
                Box::new(|r| r["cwd"] = ["", "home", "someone", "project"].join("/").into()),
            ),
            (
                "windows path",
                Box::new(|r| {
                    r["message"]["content"][0]["input"]["file_path"] =
                        ["D:", "work", "project", "main.rs"].join("\\").into();
                }),
            ),
            (
                "relative cwd",
                Box::new(|r| r["cwd"] = "work/project".into()),
            ),
            (
                "path in text",
                Box::new(|r| {
                    r["message"]["content"][0]["input"]["command"] =
                        format!("cat {}", ["", "Users", "someone", "notes.txt"].join("/")).into();
                }),
            ),
            (
                "timestamp",
                Box::new(|r| r["timestamp"] = "2031-07-14T09:26:53.589Z".into()),
            ),
            (
                "e-mail",
                Box::new(|r| r["email"] = ["someone", "mail.test"].join("@").into()),
            ),
        ];
        for (what, edit) in edits {
            let mut bad = record();
            edit(&mut bad);
            let dir = fixture("bad", &bad.to_string());
            let found = lint(&dir.0).unwrap();
            assert!(
                !found.is_empty(),
                "a fixture with a real-looking {what} passed"
            );
        }

        // A file named after a real-looking session, and a project directory outside the roots.
        let dir = fixture("bad-names", &record().to_string());
        let project = dir.0.join("claude/case/projects/-fixture-repo-a");
        std::fs::rename(
            project.join(format!("{SESSION}.jsonl")),
            project.join(format!("{real_uuid}.jsonl")),
        )
        .unwrap();
        std::fs::rename(&project, dir.0.join("claude/case/projects/-srv-project")).unwrap();
        let found = lint(&dir.0).unwrap();
        assert!(found.len() >= 2, "{found:?}");
    }

    #[test]
    fn templates_accept_their_own_examples() {
        assert!(is_template_id(SESSION));
        assert!(is_template_id("a0000000003100001"));
        assert!(is_synthetic_path("/fixture"));
        assert!(is_synthetic_path("C:\\fixture\\x"));
        assert!(!is_synthetic_path("/fixtures/x"));
        assert_eq!(
            scan_text("see https://example.com/a and src/lib.rs"),
            Vec::<String>::new()
        );
    }
}
