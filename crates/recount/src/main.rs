//! `recount`: an independent recount of the token sums of Claude Code transcripts (#16).
//!
//! It reads the transcripts as raw JSON values and shares no code with bosun's parser (a
//! dependency rule forbids it any workspace crate), so that the two can check each other: the
//! synthetic corpus in `fixtures/claude/` (#16) and, by hand, a real history (#6).
//!
//! ```text
//! recount [--repos <file>] [--compare <file>] <dir>
//! ```
//!
//! `<dir>` contains `projects/` (a Claude Code configuration directory, or a fixture case).
//! `--repos` names a JSON file `{"roots": ["<repository root>", ...]}`; without it,
//! `<dir>/repos.json` is used when it exists. The output, on stdout, is the JSON shape
//! documented in `shape.rs`. With `--compare`, the output is instead compared with a file of
//! the same shape: the differences are listed and the exit code is 1 if there are any.
//!
//! The rules (#3, "Claude Code transcript format" and "Dedup and assembly"):
//!
//! - Only `assistant` records carry usage. Other record types are skipped.
//! - An `assistant` record counts only with a string `message.id`, an object `message.usage`
//!   with integer `input_tokens` and `output_tokens`, and a string `timestamp`; the other
//!   counts may be absent (zero) but are integers when present. Anything else is a gap and is
//!   skipped, as is a line that is not JSON.
//! - Categories: `input_tokens`; `cache_creation.ephemeral_5m_input_tokens` and
//!   `ephemeral_1h_input_tokens` (without `cache_creation`, all of
//!   `cache_creation_input_tokens` is a 5 min write); `cache_read_input_tokens`;
//!   `output_tokens`.
//! - A last line without its newline is still being written and is not read.
//! - One API message is several records with one `message.id`, across all files: it is
//!   counted once, with the usage of the record with the largest `output_tokens` (ties: the
//!   larger other counts, in the order input, 5 min write, 1 h write, read; then the larger
//!   `cwd`).
//! - It belongs to the context window (file) where its id appears earliest by `timestamp`
//!   (ties: the smaller session id, then the main transcript before sub-agents and the smaller
//!   agent id, then the path, then the earlier line). A fork copies records with their
//!   timestamps, and the tie goes to the session id wherever its file lies.
//! - It belongs to the repository whose root is the counted record's `cwd` or an ancestor of
//!   it (the longest such root); separators are unified, drive-letter paths compare ignoring
//!   case.
//! - Sub-agent files are context windows of their own and add to the total.

mod count;
mod shape;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

const USAGE: &str = "usage: recount [--repos <file>] [--compare <file>] <dir>";

#[derive(Debug, Default)]
struct Args {
    repos: Option<PathBuf>,
    compare: Option<PathBuf>,
    dir: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} needs a file"))
        };
        match arg.as_str() {
            "--repos" => parsed.repos = Some(value("--repos")?),
            "--compare" => parsed.compare = Some(value("--compare")?),
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag}")),
            _ if parsed.dir.is_some() => return Err("more than one directory".to_owned()),
            dir => parsed.dir = Some(PathBuf::from(dir)),
        }
    }
    Ok(parsed)
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", path.display()))
}

/// The repository roots of `{"roots": [...]}`.
fn read_roots(path: &Path) -> Result<Vec<String>, String> {
    read_json(path)?
        .get("roots")
        .and_then(Value::as_array)
        .and_then(|roots| {
            roots
                .iter()
                .map(|r| r.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or_else(|| format!("{}: expected {{\"roots\": [<string>...]}}", path.display()))
}

/// The roots for `dir`: `--repos`, else `<dir>/repos.json` if present, else none.
fn roots_for(dir: &Path, repos: Option<&Path>) -> Result<Vec<String>, String> {
    match repos {
        Some(file) => read_roots(file),
        None if dir.join("repos.json").is_file() => read_roots(&dir.join("repos.json")),
        None => Ok(Vec::new()),
    }
}

fn run(args: Args) -> Result<bool, String> {
    let dir = args.dir.ok_or(USAGE)?;
    let roots = roots_for(&dir, args.repos.as_deref())?;
    let report = count::recount(&dir, &roots)?;
    let lines = report.lines;
    eprintln!(
        "recount: {} files, {} lines, {} usage records, {} gaps, {} not JSON, {} partial last lines",
        lines.files, lines.complete, lines.usage_records, lines.gaps, lines.not_json, lines.partial
    );
    let ours = shape::to_json(&report);
    let Some(other) = args.compare else {
        let text = serde_json::to_string_pretty(&ours).map_err(|e| e.to_string())?;
        println!("{text}");
        return Ok(true);
    };
    let theirs =
        shape::parse(&read_json(&other)?).map_err(|e| format!("{}: {e}", other.display()))?;
    let found = shape::differences(&shape::parse(&ours)?, &theirs);
    for line in &found {
        println!("{line}");
    }
    if found.is_empty() {
        println!("recount equals {}", other.display());
    }
    Ok(found.is_empty())
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude")
    }

    /// Recounts one fixture case; `Err` describes how it differs from its `expected.json`.
    fn check_case(dir: &Path) -> Result<(), String> {
        let roots = roots_for(dir, None)?;
        let ours = shape::to_json(&count::recount(dir, &roots)?);
        let expected = read_json(&dir.join("expected.json"))?;
        if ours == expected {
            return Ok(());
        }
        let found = shape::differences(&shape::parse(&ours)?, &shape::parse(&expected)?);
        Err(format!("{}: {found:?}", dir.display()))
    }

    #[test]
    fn every_fixture_equals_its_expected() {
        let mut cases: Vec<PathBuf> = std::fs::read_dir(fixtures())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        cases.sort();
        // An empty corpus would pass for the wrong reason.
        assert!(cases.len() >= 9, "found only {} fixture cases", cases.len());
        let failures: Vec<String> = cases.iter().filter_map(|c| check_case(c).err()).collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn a_split_message_counts_once() {
        check_case(&fixtures().join("split-message")).unwrap();
    }

    /// A fork's copy of a message ties with the original on timestamp; the original's session id
    /// is smaller but its file sorts later by path (#6, found on a real history).
    #[test]
    fn a_timestamp_tie_goes_to_the_smaller_session() {
        check_case(&fixtures().join("fork-tie")).unwrap();
    }

    #[test]
    fn arguments() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| (*s).to_owned()));
        let args = parse(&["--compare", "x.json", "dir"]).unwrap();
        assert_eq!(args.compare, Some(PathBuf::from("x.json")));
        assert_eq!(args.dir, Some(PathBuf::from("dir")));
        assert!(parse(&["--compare"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["--x"]).is_err());
    }
}
