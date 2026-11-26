use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::*;
use crate::ToolPolicy;

/// `fixtures/claude-stream/<name>.jsonl` (synthetic, see `fixtures/README.md`).
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude-stream")
        .join(format!("{name}.jsonl"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn classify_fixture(name: &str, code: Option<i32>) -> Exit {
    ClaudeRunner::new("claude")
        .classify_exit(ProcessExit { code }, &Tail::from_text(&fixture(name)))
}

const RESET: u64 = 946_706_400; // 2000-01-01T06:00:00Z, the fixtures' reset

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn is_crash(exit: &Exit) -> bool {
    matches!(exit, Exit::Crash { .. })
}

fn ctx() -> SessionContext {
    SessionContext {
        cwd: PathBuf::from("/fixture/repo-a"),
        session_id: SessionId::parse("00000000-0000-4000-8000-052100000000").unwrap(),
        prompt: "the task".to_owned(),
        system_prompt_file: PathBuf::from("/fixture/run/system.md"),
        mcp_config_file: PathBuf::from("/fixture/run/mcp.json"),
        repo_mcp_servers: vec!["docs".to_owned()],
        policy: ToolPolicy::default(),
        model: None,
        effort: None,
    }
}

fn args(spawn: &Spawn) -> Vec<String> {
    spawn
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The values after `flag`, up to the next flag.
fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.iter()
        .skip_while(|a| *a != flag)
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect()
}

#[test]
fn each_fixture_classifies_as_expected() {
    type Expected = fn(&Exit) -> bool;
    let cases: &[(&str, Option<i32>, Expected)] = &[
        ("normal-end", Some(0), |e| *e == Exit::Ok),
        ("permission-denied", Some(0), |e| *e == Exit::Ok),
        ("question-text", Some(0), |e| *e == Exit::Ok),
        ("limit-reset", Some(1), |e| {
            *e == Exit::Limit {
                reset: Some(at(RESET)),
            }
        }),
        ("limit-legacy-reset", Some(1), |e| {
            *e == Exit::Limit {
                reset: Some(at(RESET)),
            }
        }),
        ("limit-no-reset", Some(1), |e| {
            *e == Exit::Limit { reset: None }
        }),
        ("auth-expired", Some(1), |e| *e == Exit::Auth),
        ("crash-torn", Some(1), is_crash),
        ("crash-torn", None, is_crash),
        ("crash-error-result", Some(1), is_crash),
        // A normal stream with a failing exit code, or an exit code 0 without a result.
        ("normal-end", Some(1), is_crash),
        ("crash-torn", Some(0), is_crash),
    ];
    for (name, code, expected) in cases {
        let exit = classify_fixture(name, *code);
        assert!(expected(&exit), "{name} with exit code {code:?}: {exit:?}");
    }
}

#[test]
fn a_limit_yields_its_reset_or_none() {
    assert_eq!(
        classify_fixture("limit-reset", Some(1)),
        Exit::Limit {
            reset: Some(at(RESET))
        }
    );
    assert_eq!(
        classify_fixture("limit-legacy-reset", Some(1)),
        Exit::Limit {
            reset: Some(at(RESET))
        }
    );
    assert_eq!(
        classify_fixture("limit-no-reset", Some(1)),
        Exit::Limit { reset: None }
    );
}

#[test]
fn a_question_in_the_text_is_not_a_question() {
    assert_eq!(classify_fixture("question-text", Some(0)), Exit::Ok);
}

#[test]
fn i_am_done_in_the_final_message_changes_nothing() {
    let done = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_000000000000000052100099","role":"assistant","content":[{"type":"text","text":"I am done. The task is complete."}]},"session_id":"00000000-0000-4000-8000-052100000000"}"#;
    let runner = ClaudeRunner::new("claude");
    for (name, code) in [
        ("normal-end", Some(0)),
        ("crash-torn", Some(1)),
        ("crash-torn", None),
        ("crash-error-result", Some(1)),
        ("limit-no-reset", Some(1)),
        ("auth-expired", Some(1)),
    ] {
        let text = fixture(name);
        let mut tail = Tail::from_text(&text);
        let before = runner.classify_exit(ProcessExit { code }, &tail);
        tail.push(done);
        // Also before the result line, where the agent's last message sits.
        let mut lines: Vec<&str> = text.lines().collect();
        lines.insert(lines.len().saturating_sub(1), done);
        let inserted = Tail::from_text(&lines.join("\n"));
        assert_eq!(
            runner.classify_exit(ProcessExit { code }, &tail),
            before,
            "{name}, appended"
        );
        assert_eq!(
            runner.classify_exit(ProcessExit { code }, &inserted),
            before,
            "{name}, inserted"
        );
    }
}

#[test]
fn the_command_uses_the_pre_assigned_session_id_and_resume_names_it() {
    let runner = ClaudeRunner::new("claude");
    let ctx = ctx();
    let command = runner.command(&ctx);
    let argv = args(&command);
    assert_eq!(values(&argv, "--session-id"), [ctx.session_id.as_str()]);
    assert!(!argv.iter().any(|a| a == "--resume"));
    assert_eq!(command.stdin.as_deref(), Some("the task"));
    assert_eq!(command.cwd, ctx.cwd);
    assert_eq!(values(&argv, "--output-format"), ["stream-json"]);
    assert!(argv.iter().any(|a| a == "-p"));
    assert_eq!(
        values(&argv, "--append-system-prompt-file"),
        ["/fixture/run/system.md"]
    );

    let id = SessionId::parse("00000000-0000-4000-8000-052200000000").unwrap();
    let resume = runner.resume_command(&id, &ctx);
    let argv = args(&resume);
    assert_eq!(values(&argv, "--resume"), [id.as_str()]);
    assert!(!argv.iter().any(|a| a == "--session-id"));
    assert_eq!(resume.stdin.as_deref(), Some(RESUME_PROMPT));
    assert!(argv.iter().any(|a| a == "--strict-mcp-config"));

    // The stream reports the pre-assigned id back.
    let first = fixture("normal-end");
    let line = first.lines().next().unwrap();
    assert_eq!(
        runner.session_id(line).unwrap().as_str(),
        "00000000-0000-4000-8000-052100000000"
    );
    assert_eq!(runner.session_id("not json"), None);
}

#[test]
fn the_command_loads_only_the_session_mcp_servers() {
    let ctx = ctx();
    for spawn in [
        ClaudeRunner::new("claude").command(&ctx),
        ClaudeRunner::new("claude").resume_command(&ctx.session_id, &ctx),
    ] {
        let argv = args(&spawn);
        assert!(
            argv.iter().any(|a| a == "--strict-mcp-config"),
            "the strict MCP flag: {argv:?}"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--mcp-config").count(),
            1,
            "one MCP configuration"
        );
        assert_eq!(values(&argv, "--mcp-config"), ["/fixture/run/mcp.json"]);
        assert_eq!(
            values(&argv, "--setting-sources"),
            ["project"],
            "no user settings or hooks: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|a| a.contains("--settings") || a == "--dangerously-skip-permissions")
        );
    }

    // The file holds bosun and the repository's servers, nothing else.
    let repo = [McpServer {
        name: "docs".to_owned(),
        config: r#"{"command":"docs-mcp"}"#.to_owned(),
    }];
    let text = mcp_config(r#"{"command":"bosun-mcp","args":["--item","7"]}"#, &repo).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    let names: BTreeSet<&str> = value["mcpServers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(names, BTreeSet::from(["bosun", "docs"]));
    assert_eq!(value.as_object().unwrap().len(), 1, "mcpServers only");
    assert_eq!(value["mcpServers"]["docs"]["command"], "docs-mcp");
}

#[test]
fn the_command_turns_auto_memory_off() {
    let ctx = ctx();
    for spawn in [
        ClaudeRunner::new("claude").command(&ctx),
        ClaudeRunner::new("claude").resume_command(&ctx.session_id, &ctx),
    ] {
        let env: Vec<(String, String)> = spawn
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert!(
            env.contains(&("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_owned(), "1".to_owned())),
            "auto memory off: {env:?}"
        );
    }
}

#[test]
fn mcp_config_refuses_a_bad_server() {
    let server = |name: &str, config: &str| McpServer {
        name: name.to_owned(),
        config: config.to_owned(),
    };
    assert_eq!(
        mcp_config("{}", &[server("bosun", "{}")]),
        Err(McpConfigError::Duplicate {
            name: "bosun".to_owned()
        })
    );
    assert_eq!(
        mcp_config("{}", &[server("a", "{}"), server("a", "{}")]),
        Err(McpConfigError::Duplicate {
            name: "a".to_owned()
        })
    );
    assert_eq!(
        mcp_config("{}", &[server("a", "[1]")]),
        Err(McpConfigError::NotAnObject {
            name: "a".to_owned()
        })
    );
    assert!(mcp_config("not json", &[]).is_err());
}

#[test]
fn protected_entries_become_edit_deny_rules() {
    let mut ctx = ctx();
    ctx.policy.protected = ["docs/frozen", "harness.rs", ".gitignore", "!odd"]
        .map(str::to_owned)
        .to_vec();
    ctx.policy.deny.push("WebFetch".to_owned());
    let argv = args(&ClaudeRunner::new("claude").command(&ctx));
    let denied = values(&argv, "--disallowedTools");
    assert_eq!(
        denied,
        [
            "Bash(git push:*)",
            "PowerShell(git push:*)",
            "Edit(/docs/frozen)",
            "Edit(/docs/frozen/**)",
            "Edit(harness.rs)",
            "Edit(**/harness.rs/**)",
            "Edit(.gitignore)",
            "Edit(**/.gitignore/**)",
            r"Edit(\!odd)",
            r"Edit(**/\!odd/**)",
            "WebFetch",
        ]
    );
    for prefix in ["Write(", "NotebookEdit("] {
        assert!(!argv.iter().any(|a| a.starts_with(prefix)), "{argv:?}");
    }
    // The resumed session gets the same rules.
    let session = SessionId::parse("00000000-0000-4000-8000-052100000000").unwrap();
    let resumed = args(&ClaudeRunner::new("claude").resume_command(&session, &ctx));
    assert_eq!(values(&resumed, "--disallowedTools"), denied);
}

#[test]
fn the_tool_lists_come_from_the_policy_and_push_is_denied() {
    let mut ctx = ctx();
    ctx.policy.deny.push("WebFetch".to_owned());
    let argv = args(&ClaudeRunner::new("claude").command(&ctx));
    let allowed = values(&argv, "--allowedTools");
    for tool in [
        "mcp__bosun__ask_question",
        "mcp__bosun__submit_summary",
        "mcp__bosun__note",
        "mcp__docs",
        "Read",
        "Bash",
    ] {
        assert!(allowed.contains(&tool), "{tool} allowed: {allowed:?}");
    }
    let denied = values(&argv, "--disallowedTools");
    assert_eq!(
        denied,
        ["Bash(git push:*)", "PowerShell(git push:*)", "WebFetch"]
    );
    assert_eq!(values(&argv, "--permission-mode"), ["dontAsk"]);

    ctx.policy.allow_push = true;
    let argv = args(&ClaudeRunner::new("claude").command(&ctx));
    assert_eq!(values(&argv, "--disallowedTools"), ["WebFetch"]);

    ctx.policy.deny.clear();
    ctx.model = Some("opus".to_owned());
    let argv = args(&ClaudeRunner::new("claude").command(&ctx));
    assert!(!argv.iter().any(|a| a == "--disallowedTools"));
    assert_eq!(values(&argv, "--model"), ["opus"]);
}

#[test]
fn the_command_passes_the_effort_only_when_given() {
    let runner = ClaudeRunner::new("claude");
    let mut ctx = ctx();
    ctx.effort = Some("xhigh".to_owned());
    for spawn in [
        runner.command(&ctx),
        runner.resume_command(&ctx.session_id, &ctx),
    ] {
        let argv = args(&spawn);
        assert_eq!(
            argv.iter().filter(|a| *a == "--effort").count(),
            1,
            "one effort flag: {argv:?}"
        );
        assert_eq!(values(&argv, "--effort"), ["xhigh"]);
    }
    ctx.effort = None;
    for spawn in [
        runner.command(&ctx),
        runner.resume_command(&ctx.session_id, &ctx),
    ] {
        let argv = args(&spawn);
        assert!(!argv.iter().any(|a| a == "--effort"), "{argv:?}");
    }
}

#[test]
fn the_live_tail_collapses_the_stream() {
    let runner = ClaudeRunner::new("claude");
    let entries: Vec<LiveEntry> = fixture("permission-denied")
        .lines()
        .flat_map(|line| runner.live_entries(line))
        .collect();
    assert_eq!(
        entries,
        [
            LiveEntry::Started {
                model: Some("claude-opus-5-5".to_owned())
            },
            LiveEntry::Tool {
                name: "Bash".to_owned()
            },
            LiveEntry::Denied {
                tool: "Bash".to_owned()
            },
            LiveEntry::Text("Push is not allowed here; the commit stays local.".to_owned()),
            LiveEntry::Ended { is_error: false },
        ]
    );
    let errors: Vec<LiveEntry> = fixture("auth-expired")
        .lines()
        .flat_map(|line| runner.live_entries(line))
        .collect();
    assert!(matches!(&errors[1], LiveEntry::Error(text) if text.contains("expired")));
    assert_eq!(errors[2], LiveEntry::Ended { is_error: true });
    assert_eq!(runner.live_entries("{\"type\":\"assistant\","), []);
}

#[test]
fn a_reset_in_milliseconds_is_read_as_milliseconds() {
    assert_eq!(epoch(RESET * 1000), epoch(RESET));
    assert_eq!(
        legacy_reset("Claude AI usage limit reached|946706400 more"),
        Some(RESET)
    );
    assert_eq!(legacy_reset("usage limit reached"), None);
}

#[test]
fn the_result_lists_the_denied_calls_with_their_targets() {
    let runner = ClaudeRunner::new("claude");
    let denials = |text: &str| runner.denials(&Tail::from_text(text));
    let push = Denial {
        tool: "Bash".to_owned(),
        target: Some("git push origin HEAD".to_owned()),
        reason: Some("rule".to_owned()),
    };

    let stream = fixture("permission-denied");
    assert_eq!(denials(&stream), std::slice::from_ref(&push));

    // Killed before its result: the event still in the tail names the tool and the reason.
    let cut = stream
        .lines()
        .filter(|line| !line.starts_with(r#"{"type":"result""#))
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(cut, stream);
    assert_eq!(
        denials(&cut),
        [Denial {
            target: None,
            ..push
        }]
    );

    assert_eq!(denials(&fixture("normal-end")), []);

    let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","permission_denials":[{"tool_name":"Edit","tool_use_id":"toolu_e1","tool_input":{"file_path":"src/a.rs","old_string":"a","new_string":"b"}},{"tool_name":"mcp__docs__lookup","tool_use_id":"toolu_m1","tool_input":{"depth":2,"topic":"x"}}]}"#;
    assert_eq!(
        denials(result),
        [
            Denial {
                tool: "Edit".to_owned(),
                target: Some("src/a.rs".to_owned()),
                reason: None,
            },
            Denial {
                tool: "mcp__docs__lookup".to_owned(),
                target: Some(r#"{"depth":2,"topic":"x"}"#.to_owned()),
                reason: None,
            },
        ]
    );

    let long = format!(
        r#"{{"type":"result","is_error":false,"permission_denials":[{{"tool_name":"Bash","tool_input":{{"command":"{}"}}}}]}}"#,
        "é".repeat(600)
    );
    let capped = denials(&long);
    assert_eq!(
        capped[0].target.as_deref().map(|t| t.chars().count()),
        Some(Denial::TARGET_CAP)
    );
}
