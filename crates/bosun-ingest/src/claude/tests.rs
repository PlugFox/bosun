use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::assemble::Assembly;
use crate::line::{Batch, parse_lines};
use crate::testing::{cases, expected, files, fixtures, relative, roots, totals};

/// What the parser made of one fixture case.
struct Case {
    assembly: Assembly,
    /// Per transcript, by file key.
    batches: Vec<(String, Batch)>,
    metas: Vec<WindowMeta>,
}

/// Feeds every file of a case to the parser as a tailer would: the whole file in one read,
/// so that only complete lines are parsed.
fn parse_case(case: &Path) -> Case {
    let projects = case.join("projects");
    let mut parsed = Case {
        assembly: Assembly::default(),
        batches: Vec::new(),
        metas: Vec::new(),
    };
    for path in files(&projects) {
        let key = relative(&path, &projects);
        let located = locate(Path::new(&key)).unwrap_or_else(|| panic!("cannot locate {key}"));
        let bytes = std::fs::read(&path).unwrap();
        match located.kind {
            FileKind::Meta => parsed
                .metas
                .push(parse_meta(&located.window, &bytes).expect("a well-formed meta file")),
            FileKind::Transcript => {
                let source = Source {
                    file: key.clone(),
                    window: located.window,
                };
                let batch = parse_lines(&Parser, &source, 0, &bytes);
                for record in batch.records() {
                    parsed.assembly.add(record.clone());
                }
                parsed.batches.push((key, batch));
            }
        }
    }
    parsed
}

fn case(name: &str) -> PathBuf {
    fixtures().join("claude").join(name)
}

#[test]
fn every_fixture_equals_its_expected() {
    let cases = cases("claude");
    assert!(cases.len() >= 8, "the corpus of #16 is missing");
    for case in cases {
        let parsed = parse_case(&case);
        let got = totals(&parsed.assembly.messages(), &roots(&case));
        assert_eq!(got, expected(&case), "{}", case.display());
    }
}

#[test]
fn a_partial_last_line_waits_for_its_newline() {
    let case = case("partial-last-line");
    let parsed = parse_case(&case);
    let (key, batch) = &parsed.batches[0];
    let bytes = std::fs::read(case.join("projects").join(key)).unwrap();
    let end = usize::try_from(batch.end).unwrap();
    assert!(end < bytes.len(), "the torn tail was parsed");
    assert_eq!(parsed.assembly.messages().len(), 2);

    // The newline arrives: the next read starts at the stored end and takes the line.
    let mut tail = bytes[end..].to_vec();
    tail.push(b'\n');
    let source = Source {
        file: key.clone(),
        window: locate(Path::new(key)).unwrap().window,
    };
    let next = parse_lines(&Parser, &source, batch.end, &tail);
    assert_eq!(next.lines.len(), 1);
    assert_eq!(next.lines[0].offset, batch.end);
    assert_eq!(next.end, batch.end + u64::try_from(tail.len()).unwrap());
    assert!(matches!(next.records().next(), Some(Record::Message(_))));
}

const SESSION: &str = "00000000-0000-4000-8000-000900000000";

fn source() -> Source {
    Source {
        file: "test".to_owned(),
        window: WindowKey {
            session_id: SESSION.to_owned(),
            agent_id: None,
        },
    }
}

/// A well-formed `assistant` record of message `id` with the given output.
fn assistant_line(id: &str, output: u64) -> serde_json::Value {
    json!({
        "type": "assistant",
        "sessionId": SESSION,
        "cwd": "/fixture/repo-a",
        "timestamp": "2000-01-09T00:00:01.000Z",
        "message": {
            "id": id,
            "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": "Synthetic."}],
            "usage": {"input_tokens": 1, "cache_read_input_tokens": 10, "output_tokens": output}
        }
    })
}

/// Lines of known types whose shape is unexpected, each holding `sentinel` in a value.
fn bad_lines(sentinel: &str) -> Vec<String> {
    let mut lines = vec![
        format!("not JSON {sentinel}"),
        format!("{{\"type\":\"assistant\",\"message\":{{\"id\":\"{sentinel}\""),
        json!([sentinel]).to_string(),
        json!({"type": 5, "note": sentinel}).to_string(),
        json!({"type": "assistant", "sessionId": SESSION, "message": sentinel}).to_string(),
        json!({"type": "user", "sessionId": SESSION, "message": {"content": [
            {"type": "tool_result", "tool_use_id": 7, "content": sentinel}]}})
        .to_string(),
        json!({"type": "cost-state", "sessionId": SESSION, "modelUsage": sentinel}).to_string(),
        json!({"type": "assistant", "sessionId": sentinel, "message": {}}).to_string(),
    ];
    let mut wrong_count = assistant_line("msg_000000000000000009000001", 5);
    wrong_count["message"]["usage"]["input_tokens"] = sentinel.into();
    lines.push(wrong_count.to_string());
    let mut wrong_block = assistant_line("msg_000000000000000009000001", 5);
    wrong_block["message"]["content"] = json!([{"type": "tool_use", "name": sentinel}]);
    lines.push(wrong_block.to_string());
    let mut wrong_time = assistant_line("msg_000000000000000009000001", 5);
    wrong_time["timestamp"] = sentinel.into();
    lines.push(wrong_time.to_string());
    lines
}

#[test]
fn a_gap_holds_none_of_its_line() {
    let sentinel = "SENTINEL-0451";
    for line in bad_lines(sentinel) {
        let Outcome::Gap(gap) = Parser.parse_line(&source(), 40, line.as_bytes()) else {
            panic!("not a gap: {line}");
        };
        assert_eq!(gap.offset, 40);
        assert_eq!(gap.length, u64::try_from(line.len()).unwrap());
        assert_ne!(gap.reason, "");
        let mut fields = vec![gap.reason.clone()];
        fields.extend(gap.record_type.clone());
        fields.extend(gap.top_level_keys.clone());
        for field in fields {
            assert!(!field.contains(sentinel), "{line} leaks into {field:?}");
        }
        assert!(!format!("{gap:?}").contains(sentinel), "{line}");
    }

    let window = WindowKey {
        session_id: SESSION.to_owned(),
        agent_id: Some("a0000000000900001".to_owned()),
    };
    for meta in [
        format!("{{\"agentType\": \"{sentinel}"),
        json!({"agentType": 5, "toolUseId": sentinel}).to_string(),
        json!({"spawnDepth": sentinel}).to_string(),
    ] {
        let gap = parse_meta(&window, meta.as_bytes()).unwrap_err();
        assert!(!format!("{gap:?}").contains(sentinel), "{meta}");
    }
}

#[test]
fn the_rest_of_a_file_parses_after_a_gap() {
    let mut text = String::new();
    text.push_str(&assistant_line("msg_000000000000000009000001", 5).to_string());
    text.push('\n');
    for line in bad_lines("synthetic") {
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(&assistant_line("msg_000000000000000009000002", 7).to_string());
    text.push('\n');

    let batch = parse_lines(&Parser, &source(), 0, text.as_bytes());
    assert_eq!(batch.end, u64::try_from(text.len()).unwrap());
    assert_eq!(batch.gaps().count(), bad_lines("synthetic").len());
    let ids: Vec<&str> = batch
        .records()
        .filter_map(|record| match record {
            Record::Message(message) => Some(message.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        [
            "msg_000000000000000009000001",
            "msg_000000000000000009000002"
        ]
    );
}

#[test]
fn unknown_types_are_skipped_and_counted() {
    let parsed = parse_case(&case("skipped-records"));
    let (_, batch) = &parsed.batches[0];
    assert_eq!(
        batch.skipped(),
        BTreeMap::from([("fixture-future-kind", 1)])
    );
    // A string count, a string `message` and a torn line; known types without usage are
    // neither gaps nor skips.
    let gaps: Vec<(Option<&str>, &str)> = batch
        .gaps()
        .map(|gap| (gap.record_type.as_deref(), gap.reason.as_str()))
        .collect();
    assert_eq!(
        gaps,
        [
            (Some("assistant"), "a count is not a non-negative integer"),
            (Some("assistant"), "`message` is not an object"),
            (None, "not JSON"),
        ]
    );
}

#[test]
fn usage_categories_are_taken_field_by_field() {
    let mut line = assistant_line("msg_000000000000000009000001", 5);
    line["message"]["usage"]["cache_creation_input_tokens"] = 30.into();
    let parse = |line: &serde_json::Value| match Parser.parse_line(
        &source(),
        0,
        line.to_string().as_bytes(),
    ) {
        Outcome::Records(records) => match &records[0] {
            Record::Message(message) => message.usage.tokens,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    // Without `cache_creation`, every write is a 5 min write.
    let tokens = parse(&line);
    assert_eq!((tokens.cache_write_5m, tokens.cache_write_1h), (30, 0));
    assert_eq!((tokens.input, tokens.cache_read, tokens.output), (1, 10, 5));

    line["message"]["usage"]["cache_creation"] =
        json!({"ephemeral_5m_input_tokens": 10, "ephemeral_1h_input_tokens": 20});
    let tokens = parse(&line);
    assert_eq!((tokens.cache_write_5m, tokens.cache_write_1h), (10, 20));
}

#[test]
fn tool_calls_carry_targets_results_and_touches() {
    let parsed = parse_case(&case("report"));
    let messages = parsed.assembly.messages();
    let call = |id: &str| {
        messages
            .iter()
            .find_map(|message| message.tool_calls.get(id))
            .unwrap_or_else(|| panic!("no call {id}"))
    };

    let grep = call("toolu_000000000000000008190001");
    assert_eq!(grep.name, "Grep");
    assert_eq!(grep.target.as_deref(), Some("/fixture/repo-a/src"));
    assert_eq!(
        grep.result_bytes,
        u64::try_from("src/lib.rs".len()).unwrap()
    );
    assert!(!grep.is_error);
    assert!(grep.input_bytes > 0);

    let bash = call("toolu_000000000000000008190004");
    assert_eq!(bash.target.as_deref(), Some("cargo"));
    assert!(bash.is_error);
    let output = "Exit code 101\nsynthetic test failure";
    assert_eq!(bash.result_bytes, u64::try_from(output.len()).unwrap());

    // The thinking record and the tool_use record of one message merge into one message that
    // keeps the call.
    let split = messages
        .iter()
        .find(|m| m.id == "msg_000000000000000008100002")
        .unwrap();
    assert_eq!(split.usage.tokens.output, 60);
    assert_eq!(split.tool_calls.len(), 1);

    let touches: Vec<(&str, &str, TouchKind)> = parsed
        .assembly
        .touches()
        .map(|t| (t.tool_use_id.as_str(), t.path.as_str(), t.kind))
        .collect();
    let lib = "/fixture/repo-a/src/lib.rs";
    assert_eq!(
        touches,
        [
            (
                "toolu_000000000000000008190001",
                "/fixture/repo-a/src",
                TouchKind::Search
            ),
            ("toolu_000000000000000008190002", lib, TouchKind::Read),
            ("toolu_000000000000000008190003", lib, TouchKind::Read),
            ("toolu_000000000000000008190005", lib, TouchKind::Write),
            ("toolu_000000000000000008290001", lib, TouchKind::Read),
            (
                "toolu_000000000000000008290002",
                "/fixture/repo-a/src/new.rs",
                TouchKind::Write
            ),
        ]
    );
}

#[test]
fn the_last_cost_state_is_the_session_check() {
    let parsed = parse_case(&case("subagent-cost-state"));
    let checks: Vec<&SessionCheck> = parsed.assembly.checks().collect();
    assert_eq!(checks.len(), 1);
    let check = checks[0];
    assert_eq!(check.session_id, "00000000-0000-4000-8000-003100000000");
    let opus = check.models["claude-opus-5-5"].tokens;
    assert_eq!(opus.output, 50_010);
    assert_eq!((opus.cache_write_5m, opus.cache_write_1h), (90, 1100));
    assert_eq!(check.models["claude-sonnet-5-5"].tokens.cache_read, 11_000);

    // Each cost-state line is its own record; the sums never include them.
    let (_, main) = parsed
        .batches
        .iter()
        .find(|(key, _)| !key.contains("subagents"))
        .unwrap();
    let count = main
        .records()
        .filter(|r| matches!(r, Record::SessionCheck(_)))
        .count();
    assert_eq!(count, 2);
}

#[test]
fn a_meta_file_names_the_parent_and_the_spawning_call() {
    let parsed = parse_case(&case("subagent-cost-state"));
    let session = "00000000-0000-4000-8000-003100000000";
    assert_eq!(
        parsed.metas,
        [WindowMeta {
            window: ContextWindow {
                key: WindowKey {
                    session_id: session.to_owned(),
                    agent_id: Some("a0000000003100001".to_owned()),
                },
                parent: Some(WindowKey {
                    session_id: session.to_owned(),
                    agent_id: None,
                }),
                kind: WindowKind::Interactive,
            },
            agent_type: Some("general-purpose".to_owned()),
            spawn_depth: Some(1),
            spawned_by: Some("toolu_000000000000000003190001".to_owned()),
        }]
    );

    let nested = parse_meta(
        &parsed.metas[0].window.key,
        json!({"parentAgentId": "a0000000003100002"})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        nested.window.parent.unwrap().agent_id.as_deref(),
        Some("a0000000003100002")
    );
}

#[test]
fn files_are_located_by_the_path_layout() {
    let main = locate(Path::new("-fixture-repo-a/s1.jsonl")).unwrap();
    assert_eq!(main.kind, FileKind::Transcript);
    assert_eq!(main.window.session_id, "s1");
    assert_eq!(main.window.agent_id, None);

    for (path, kind) in [
        (
            "-fixture-repo-a/s1/subagents/agent-a1.jsonl",
            FileKind::Transcript,
        ),
        (
            "-fixture-repo-a/s1/subagents/agent-a1.meta.json",
            FileKind::Meta,
        ),
    ] {
        let located = locate(Path::new(path)).unwrap();
        assert_eq!(located.kind, kind);
        assert_eq!(located.window.session_id, "s1");
        assert_eq!(located.window.agent_id.as_deref(), Some("a1"));
    }

    for other in [
        "-fixture-repo-a/s1/tool-results/x.txt",
        "-fixture-repo-a/s1/elsewhere/agent-a1.jsonl",
        "-fixture-repo-a/s1.meta.json",
    ] {
        assert_eq!(locate(Path::new(other)), None, "{other}");
    }
}

#[test]
fn a_record_of_another_session_is_a_gap() {
    let mut line = assistant_line("msg_000000000000000009000001", 5);
    line["sessionId"] = "00000000-0000-4000-8000-000900000001".into();
    let outcome = Parser.parse_line(&source(), 0, line.to_string().as_bytes());
    assert!(matches!(outcome, Outcome::Gap(_)), "{outcome:?}");

    line["sessionId"] = SESSION.into();
    line["agentId"] = "a0000000000900001".into();
    let outcome = Parser.parse_line(&source(), 0, line.to_string().as_bytes());
    assert!(matches!(outcome, Outcome::Gap(_)), "{outcome:?}");
}
