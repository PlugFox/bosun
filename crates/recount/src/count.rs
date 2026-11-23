//! Reading transcripts and summing their usage, from raw JSON values.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The five token categories of #3, in output order.
pub(crate) const CATEGORIES: [&str; 5] = [
    "input",
    "cache_write_5m",
    "cache_write_1h",
    "cache_read",
    "output",
];
const OUTPUT: usize = 4;

/// A message count and one sum per category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sums {
    pub(crate) messages: u64,
    pub(crate) tokens: [u64; 5],
}

impl Sums {
    fn add(&mut self, tokens: &[u64; 5]) {
        self.messages += 1;
        for (sum, n) in self.tokens.iter_mut().zip(tokens) {
            *sum += n;
        }
    }
}

/// One context window: a main transcript (`agent: None`) or a sub-agent transcript.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Window {
    pub(crate) session: String,
    pub(crate) agent: Option<String>,
}

/// The recount of one directory.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) sessions: BTreeMap<Window, Sums>,
    /// Keyed by repository root as given; `None` is a message under no root.
    pub(crate) repos: BTreeMap<Option<String>, Sums>,
    pub(crate) total: Sums,
    pub(crate) lines: Lines,
}

/// What happened to the lines read, for the summary on stderr.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Lines {
    pub(crate) files: usize,
    pub(crate) complete: usize,
    /// Files whose last line has no newline yet; that line is not read.
    pub(crate) partial: usize,
    pub(crate) not_json: usize,
    /// `assistant` records with an unexpected shape.
    pub(crate) gaps: usize,
    pub(crate) usage_records: usize,
}

/// The transcript files under `<dir>/projects`, sorted by path: `<project>/<session>.jsonl`
/// and `<project>/<session>/subagents/agent-<agent>.jsonl`. Anything else is ignored.
pub(crate) fn windows(dir: &Path) -> Result<Vec<(Window, PathBuf)>, String> {
    let projects = dir.join("projects");
    let mut out = Vec::new();
    for project in sorted_entries(&projects)? {
        if !project.is_dir() {
            continue;
        }
        for entry in sorted_entries(&project)? {
            if entry.is_file() {
                if let Some(session) = stem(&entry, "", ".jsonl") {
                    out.push((
                        Window {
                            session,
                            agent: None,
                        },
                        entry,
                    ));
                }
            } else if entry.is_dir() {
                let Some(session) = entry.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let subagents = entry.join("subagents");
                if !subagents.is_dir() {
                    continue;
                }
                for file in sorted_entries(&subagents)? {
                    if let Some(agent) = stem(&file, "agent-", ".jsonl").filter(|_| file.is_file())
                    {
                        out.push((
                            Window {
                                session: session.to_owned(),
                                agent: Some(agent),
                            },
                            file,
                        ));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    Ok(entries)
}

fn stem(path: &Path, prefix: &str, suffix: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let inner = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!inner.is_empty()).then(|| inner.to_owned())
}

/// One `assistant` record with a well-formed usage block.
#[derive(Debug)]
struct Observation {
    window: usize,
    /// The window's rank in the tiebreak order: by session id, then agent id (main first),
    /// then path.
    rank: usize,
    line: usize,
    id: String,
    timestamp: String,
    cwd: Option<String>,
    tokens: [u64; 5],
}

impl Observation {
    /// Order of appearance: by timestamp, then window (session id, then agent id), then path,
    /// then line. A fork copies records with their timestamps; the tie goes to the smaller
    /// session id wherever its file lies (#3).
    fn place(&self) -> (&str, usize, usize) {
        (&self.timestamp, self.rank, self.line)
    }

    /// The order that picks the record whose usage a message keeps: the largest output, then
    /// the larger remaining counts, then the larger cwd. Ties beyond that change no sum.
    fn usage_key(&self) -> (u64, [u64; 5], Option<&str>) {
        (self.tokens[OUTPUT], self.tokens, self.cwd.as_deref())
    }
}

enum Record {
    /// Not an `assistant` record: skipped by type.
    Other,
    /// An `assistant` record whose shape is not the expected one.
    Gap,
    Usage {
        id: String,
        timestamp: String,
        cwd: Option<String>,
        tokens: [u64; 5],
    },
}

/// A token count that may be absent (zero) but, when present, is a non-negative integer.
fn optional(object: &Map<String, Value>, key: &str) -> Option<u64> {
    match object.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(value) => value.as_u64(),
    }
}

fn usage_of(record: &Map<String, Value>) -> Option<Record> {
    let message = record.get("message")?.as_object()?;
    let id = message.get("id")?.as_str()?;
    let usage = message.get("usage")?.as_object()?;
    let timestamp = record.get("timestamp")?.as_str()?;
    let cwd = match record.get("cwd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_str()?.to_owned()),
    };
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    let read = optional(usage, "cache_read_input_tokens")?;
    // Without the split, every cache write is a 5 min write (#3).
    let (five_minutes, one_hour) = match usage.get("cache_creation") {
        None | Some(Value::Null) => (optional(usage, "cache_creation_input_tokens")?, 0),
        Some(Value::Object(split)) => (
            optional(split, "ephemeral_5m_input_tokens")?,
            optional(split, "ephemeral_1h_input_tokens")?,
        ),
        Some(_) => return None,
    };
    Some(Record::Usage {
        id: id.to_owned(),
        timestamp: timestamp.to_owned(),
        cwd,
        tokens: [input, five_minutes, one_hour, read, output],
    })
}

fn classify(value: &Value) -> Record {
    let Some(record) = value.as_object() else {
        return Record::Other;
    };
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return Record::Other;
    }
    usage_of(record).unwrap_or(Record::Gap)
}

/// The complete lines of `bytes`: everything up to the last newline. A last line without its
/// newline is still being written and is not read.
fn complete_lines(bytes: &[u8]) -> (impl Iterator<Item = &[u8]>, bool) {
    let end = bytes.iter().rposition(|&b| b == b'\n');
    let complete = end.map_or(&bytes[..0], |i| &bytes[..i]);
    let partial = end.map_or(!bytes.is_empty(), |i| i + 1 < bytes.len());
    let lines = complete
        .split(|&b| b == b'\n')
        .filter(move |_| end.is_some())
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    (lines, partial)
}

fn observe(files: &[(Window, PathBuf)]) -> Result<(Vec<Observation>, Lines), String> {
    let mut lines = Lines {
        files: files.len(),
        ..Lines::default()
    };
    // `files` is sorted by path; the rank orders it by window first.
    let mut order: Vec<usize> = (0..files.len()).collect();
    if !cfg!(mutant_recount_tie_by_path) {
        // The mutant skips this sort: ties then go to the file that sorts first by path.
        order.sort_by(|&a, &b| files[a].0.cmp(&files[b].0).then(a.cmp(&b)));
    }
    let mut rank = vec![0; files.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }
    let mut out = Vec::new();
    for (window, (_, path)) in files.iter().enumerate() {
        let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let (complete, partial) = complete_lines(&bytes);
        lines.partial += usize::from(partial);
        for (line, text) in complete.enumerate() {
            lines.complete += 1;
            let Ok(value) = serde_json::from_slice::<Value>(text) else {
                lines.not_json += 1;
                continue;
            };
            match classify(&value) {
                Record::Other => {}
                Record::Gap => lines.gaps += 1,
                Record::Usage {
                    id,
                    timestamp,
                    cwd,
                    tokens,
                } => {
                    lines.usage_records += 1;
                    out.push(Observation {
                        window,
                        rank: rank[window],
                        line,
                        id,
                        timestamp,
                        cwd,
                        tokens,
                    });
                }
            }
        }
    }
    Ok((out, lines))
}

/// One counted message: the observation whose usage counts and the one that places it.
struct Message {
    usage: usize,
    home: usize,
}

/// The dedup and assembly rule of #3: per message id, the usage of the record with the largest
/// `output_tokens`; the message belongs to the file where its id appears earliest.
fn messages(observations: &[Observation]) -> Vec<Message> {
    if cfg!(mutant_recount_no_dedup) {
        // Mutant: every record is a message of its own.
        return (0..observations.len())
            .map(|i| Message { usage: i, home: i })
            .collect();
    }
    if cfg!(mutant_recount_last_block) {
        // Mutant: the old bill, the last usage block of each file.
        let mut last: BTreeMap<usize, usize> = BTreeMap::new();
        for (i, o) in observations.iter().enumerate() {
            last.insert(o.window, i);
        }
        return last
            .into_values()
            .map(|i| Message { usage: i, home: i })
            .collect();
    }
    let mut by_id: BTreeMap<&str, Message> = BTreeMap::new();
    for (i, o) in observations.iter().enumerate() {
        let entry = by_id.entry(&o.id).or_insert(Message { usage: i, home: i });
        let usage = &observations[entry.usage];
        if o.usage_key() > usage.usage_key()
            || (o.usage_key() == usage.usage_key() && o.place() < usage.place())
        {
            entry.usage = i;
        }
        if o.place() < observations[entry.home].place() {
            entry.home = i;
        }
    }
    by_id.into_values().collect()
}

/// Separators unified and trailing ones dropped; a drive-letter path is compared ignoring
/// case, as Windows does.
fn normalise(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        p.make_ascii_lowercase();
    }
    p
}

/// The repository of a `cwd`: the longest root that is the cwd or one of its ancestors.
fn repo_of<'a>(cwd: &str, roots: &'a [(String, String)]) -> Option<&'a str> {
    let cwd = normalise(cwd);
    roots
        .iter()
        .filter(|(_, root)| {
            cwd == *root
                || cwd
                    .strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with('/') || root.ends_with('/'))
        })
        .max_by_key(|(_, root)| root.len())
        .map(|(given, _)| given.as_str())
}

/// Recounts the transcripts under `<dir>/projects`, attributing messages to `roots`.
pub(crate) fn recount(dir: &Path, roots: &[String]) -> Result<Report, String> {
    let files = windows(dir)?;
    let (observations, lines) = observe(&files)?;
    let roots: Vec<(String, String)> = roots.iter().map(|r| (r.clone(), normalise(r))).collect();

    let mut report = Report {
        lines,
        ..Report::default()
    };
    for (window, _) in &files {
        report.sessions.entry(window.clone()).or_default();
    }
    for message in messages(&observations) {
        let usage = &observations[message.usage];
        let window = &files[observations[message.home].window].0;
        let repo = usage
            .cwd
            .as_deref()
            .and_then(|cwd| repo_of(cwd, &roots))
            .map(str::to_owned);
        report
            .sessions
            .entry(window.clone())
            .or_default()
            .add(&usage.tokens);
        report.repos.entry(repo).or_default().add(&usage.tokens);
        report.total.add(&usage.tokens);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_lines_are_read() {
        let lines = |b: &'static [u8]| {
            let (it, partial) = complete_lines(b);
            (it.collect::<Vec<_>>(), partial)
        };
        assert_eq!(lines(b""), (vec![], false));
        assert_eq!(lines(b"{}"), (vec![], true));
        assert_eq!(lines(b"a\n"), (vec![&b"a"[..]], false));
        assert_eq!(lines(b"a\r\nb\n{"), (vec![&b"a"[..], &b"b"[..]], true));
    }

    #[test]
    fn roots_match_on_component_boundaries() {
        let roots: Vec<(String, String)> = ["/r/a", "/r/a/sub", r"C:\R\b"]
            .iter()
            .map(|r| ((*r).to_owned(), normalise(r)))
            .collect();
        assert_eq!(repo_of("/r/a", &roots), Some("/r/a"));
        assert_eq!(repo_of("/r/a/x", &roots), Some("/r/a"));
        assert_eq!(repo_of("/r/a/sub/x", &roots), Some("/r/a/sub"));
        assert_eq!(repo_of("/r/ab", &roots), None);
        assert_eq!(repo_of(r"c:\r\b\src", &roots), Some(r"C:\R\b"));
        assert_eq!(repo_of("C:/R/b", &roots), Some(r"C:\R\b"));
        assert_eq!(repo_of("/R/A", &roots), None);
    }

    #[test]
    fn legacy_usage_is_all_five_minute_writes() {
        let record = serde_json::json!({
            "type": "assistant", "timestamp": "t",
            "message": {"id": "m", "usage": {
                "input_tokens": 1, "output_tokens": 2,
                "cache_creation_input_tokens": 3, "cache_read_input_tokens": 4}}
        });
        let Record::Usage { tokens, .. } = classify(&record) else {
            panic!("not read as usage");
        };
        assert_eq!(tokens, [1, 3, 0, 4, 2]);
    }

    #[test]
    fn unexpected_shapes_are_gaps_and_other_types_are_skipped() {
        let gap = serde_json::json!({"type": "assistant", "timestamp": "t",
            "message": {"id": "m", "usage": {"input_tokens": "1", "output_tokens": 2}}});
        assert!(matches!(classify(&gap), Record::Gap));
        let other = serde_json::json!({"type": "user", "timestamp": "t",
            "message": {"id": "m", "usage": {"input_tokens": 1, "output_tokens": 2}}});
        assert!(matches!(classify(&other), Record::Other));
    }
}
