use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use super::*;
use crate::item::{Next, Park};
use crate::publish::find_key;
use crate::publish::sanitise::tests::{CORPUS, inert};

fn attempt(number: u32, state: AttemptState, tail: &str) -> AttemptFacts {
    AttemptFacts {
        number,
        state,
        model: Some("model-a".into()),
        effort: None,
        sessions: vec![format!("session-{number}")],
        crash_retries: 0,
        verdict: Some(VerdictFacts {
            first: RunResult::Fail,
            rerun: Some(RunResult::Fail),
            duration_ms: 12_345,
            line: "test result: FAILED. 1 failed".into(),
            tail: tail.into(),
        }),
        unready: 0,
        refusal: Vec::new(),
        denials: Vec::new(),
    }
}

fn facts() -> ReportFacts {
    ReportFacts {
        item: ItemId::new("owner/repo#12"),
        title: "Add the widget".into(),
        state: State::NeedsYou(Reason::AttemptsExhausted),
        detail: None,
        verdict: "cargo test -p widget".into(),
        model: Some("opus".into()),
        attempts: vec![attempt(1, AttemptState::Fail, "error: one\nerror: two")],
        flaky_tests: vec![],
        parked: vec![ParkedFacts {
            reason: ParkReason::Limit,
            ms: 3_723_000,
        }],
        landing: None,
        diff: Some(DiffStat {
            files: 3,
            insertions: 1_204,
            deletions: 7,
            protected: vec![],
        }),
        summary: None,
        bill: Bill {
            eqv: 1_234_567.4,
            input_tokens: 1_000,
            output_tokens: 2_000,
            cache_read_tokens: 3_000_000,
            cache_write_tokens: 4_000,
            wall_clock_ms: 754_000,
            commits: 2,
        },
    }
}

/// The text of every fenced or indented code block in `body`.
fn code_blocks(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for event in Parser::new(body) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => current = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => blocks.extend(current.take()),
            Event::Text(t) => {
                if let Some(c) = &mut current {
                    c.push_str(&t);
                }
            }
            _ => {}
        }
    }
    blocks
}

/// The text of `body` outside code blocks and code spans.
fn prose(body: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for event in Parser::new(body) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code = true,
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Text(t) if !in_code => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak | Event::End(_) => out.push('\n'),
            _ => {}
        }
    }
    out
}

/// The HTML in `body` that is not the daemon's own: the marker and the tails' `<details>`.
fn foreign_html(body: &str) -> Vec<String> {
    let own = |h: &str| {
        let h = h.trim();
        h.starts_with("<!-- bosun:key=report:")
            || h == "</details>"
            || (h.starts_with("<details><summary>Attempt ") && h.ends_with("</summary>"))
    };
    Parser::new(body)
        .filter_map(|e| match e {
            Event::Html(h) | Event::InlineHtml(h) if !own(&h) => Some(h.into_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_body_starts_with_the_hidden_key() {
    let body = render_report(&facts(), ReportOptions::for_repository(false));
    assert_eq!(find_key(&body), Some("report:owner/repo#12"), "{body}");
    assert!(
        body.contains("Needs you: the failed attempts reached the limit"),
        "{body}"
    );
    assert!(body.contains("eqv 1,234,567"), "{body}");
    assert!(body.contains("- Model asked: `opus`"), "{body}");
    assert!(body.contains("eqv per commit 617,284"), "{body}");
    assert!(body.contains("1h 02m 03s"), "{body}");
    assert!(body.contains("verdict fail, then fail in 12.3 s"), "{body}");
    assert!(
        code_blocks(&body).iter().any(|b| b.contains("error: two")),
        "{body}"
    );
}

#[test]
fn the_attempt_line_names_its_effort_only_when_one_was_passed() {
    let mut facts = facts();
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(!body.contains("effort"), "{body}");
    facts.attempts = vec![AttemptFacts {
        effort: Some("high".into()),
        ..attempt(1, AttemptState::Fail, "")
    }];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(
        body.contains("; model `model-a`; effort `high`; sessions `session-1`"),
        "{body}"
    );
}

#[test]
fn denied_calls_name_their_targets_only_off_a_public_repository() {
    // Joined at run time, so the source holds no token-shaped literal.
    let token = format!("{}{}", "ghp", "_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
    let denied = |tool: &str, target: &str| DeniedCall {
        tool: tool.into(),
        target: Some(target.into()),
    };
    let mut facts = facts();
    facts.attempts = vec![AttemptFacts {
        denials: vec![
            denied("Bash", "git push origin HEAD"),
            denied("Edit", "/fixture/repo-a/.github/workflows/ci.yml"),
            denied(
                "Bash",
                &format!("curl -H 'Authorization: token {token}' example.invalid"),
            ),
        ],
        ..attempt(1, AttemptState::Fail, "")
    }];

    // A public repository gets the count per tool, never a target.
    let body = render_report(&facts, ReportOptions::for_repository(true));
    assert!(
        body.contains("; 3 tool calls denied (Bash 2, Edit 1); model `model-a`"),
        "{body}"
    );
    for target in ["git push", "ci.yml", "curl", &token] {
        assert!(!body.contains(target), "{target}: {body}");
    }

    // A private one names each call and its target as code spans, the secret redacted.
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(
        body.contains(
            "; 3 tool calls denied: `Bash` `git push origin HEAD`, \
             `Edit` `/fixture/repo-a/.github/workflows/ci.yml`, `Bash` `curl"
        ),
        "{body}"
    );
    assert!(!body.contains(&token), "{body}");
    assert!(body.contains("[redacted]"), "{body}");

    // One call is a call, and an attempt without any says nothing.
    facts.attempts[0].denials.truncate(1);
    let body = render_report(&facts, ReportOptions::for_repository(true));
    assert!(body.contains("; 1 tool call denied (Bash 1)"), "{body}");
    facts.attempts[0].denials.clear();
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(!body.contains("denied"), "{body}");

    // Past the budget the rest are counted.
    facts.attempts[0].denials = vec![denied("Bash", &"y".repeat(200)); 30];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(
        body.contains("; 30 tool calls denied: `Bash` `yyy"),
        "{body}"
    );
    assert!(body.contains(" more; model `model-a`"), "{body}");
}

#[test]
fn an_attempt_refused_before_its_verdict_lists_the_reasons() {
    let mut facts = facts();
    facts.attempts = vec![AttemptFacts {
        verdict: None,
        refusal: vec![
            "the working tree is not clean: ?? loose.txt".into(),
            "commit 0123abc carries a closing keyword: \"Fixes #3\"".into(),
        ],
        ..attempt(1, AttemptState::Fail, "")
    }];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(
        body.contains(
            "- Attempt 1: fail, refused before its verdict: the working tree is not clean"
        ),
        "{body}"
    );
    assert!(body.contains("loose.txt"), "{body}");
    // The quoted commit message is inert: the closing verb and its reference are a code span.
    assert!(body.contains("\"`Fixes #3`\""), "{body}");

    // A public repository gets that there was a refusal, not what it names.
    let body = render_report(&facts, ReportOptions::for_repository(true));
    assert!(
        body.contains("- Attempt 1: fail, refused before its verdict (the reasons are in"),
        "{body}"
    );
    assert!(!body.contains("loose.txt"), "{body}");
}

#[test]
fn a_public_repository_gets_no_verdict_tails() {
    let body = render_report(&facts(), ReportOptions::for_repository(true));
    assert!(!body.contains("error: two"), "{body}");
    assert!(code_blocks(&body).is_empty(), "{body}");
    assert!(
        body.contains("not published on public repositories"),
        "{body}"
    );
}

#[test]
fn a_token_in_a_verdict_tail_is_redacted() {
    // Joined at run time, so the source holds no token-shaped literal.
    let token = format!("{}{}", "ghp", "_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
    let mut facts = facts();
    facts.attempts = vec![attempt(
        1,
        AttemptState::Fail,
        &format!("running\nAuthorization: token {token}\nGITHUB_TOKEN=abcdef123456\nfailed"),
    )];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(!body.contains(&token), "{body}");
    assert!(!body.contains("abcdef123456"), "{body}");
    assert!(
        code_blocks(&body)
            .iter()
            .any(|b| b.contains("token [redacted]") && b.contains("failed")),
        "{body}"
    );
}

#[test]
fn a_secret_split_by_an_invisible_character_is_redacted() {
    // Joined at run time, so the source holds no token-shaped literal. Each split character
    // breaks the token's shape until cleaning removes it (#138).
    let (head, rest) = ("ghp_ABCDEFGHIJKLMNO", "PQRSTUVWXYZabcdefghij");
    for split in ['\u{200B}', '\u{202E}'] {
        let secret = format!("{head}{split}{rest}");
        let mut facts = facts();
        facts.detail = Some(format!("leaked {secret}"));
        facts.attempts = vec![attempt(
            1,
            AttemptState::Fail,
            &format!("running\nGITHUB_TOKEN {secret}\nfailed"),
        )];
        facts.attempts[0].verdict.as_mut().unwrap().line = format!("token {secret}");
        facts.summary = Some(AgentSummary {
            attempt: 1,
            head: "abc1234".into(),
            done: format!("Used {secret}."),
            decisions: vec![format!("Kept {secret}.")],
            deviations: vec![],
            questions: vec![],
            followups: vec![],
        });
        let body = render_report(&facts, ReportOptions::for_repository(false));
        assert!(!body.contains(head), "{body}");
        assert!(
            code_blocks(&body)
                .iter()
                .any(|b| b.contains("GITHUB_TOKEN [redacted]\nfailed")),
            "{body}"
        );
        assert_eq!(body.matches("redacted").count(), 5, "{body}");
    }
}

#[test]
fn a_200_kb_tail_renders_under_the_cap_and_keeps_its_last_lines() {
    let mut tail = String::new();
    let mut n = 0;
    while tail.len() < 200_000 {
        n += 1;
        let _ = writeln!(tail, "line {n:06}: some ordinary verdict output");
    }
    tail.push_str("FINAL: assertion failed at the very end");
    let mut facts = facts();
    facts.attempts = vec![attempt(1, AttemptState::Fail, &tail)];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(chars(&body) <= BODY_CAP, "{}", chars(&body));
    let blocks = code_blocks(&body);
    assert_eq!(blocks.len(), 1);
    let kept = &blocks[0];
    assert!(
        kept.ends_with("FINAL: assertion failed at the very end\n"),
        "{kept:.200}"
    );
    assert!(kept.contains(&format!("line {n:06}:")));
    assert!(!kept.contains("line 000001:"));
    assert!(
        kept.starts_with("line "),
        "the cut is at a line boundary: {kept:.80}"
    );
    assert!(body.contains("earlier lines cut"));
}

#[test]
fn maximal_facts_stay_under_the_cap() {
    let big = "x".repeat(300_000);
    let lines = "a\n".repeat(150_000);
    let many = |s: &str| vec![s.to_owned(); 500];
    let facts = ReportFacts {
        item: ItemId::new("owner/repo#1"),
        title: big.clone(),
        state: State::Parked(Park {
            reason: ParkReason::Limit,
            until_ms: None,
            next: Next::Resume,
        }),
        detail: Some(lines.clone()),
        verdict: big.clone(),
        model: Some(big.clone()),
        attempts: (1..=30)
            .map(|i| AttemptFacts {
                number: i,
                state: AttemptState::Fail,
                model: Some(big.clone()),
                effort: Some(big.clone()),
                sessions: many(&big),
                crash_retries: 3,
                unready: u32::MAX,
                verdict: Some(VerdictFacts {
                    first: RunResult::Timeout,
                    rerun: Some(RunResult::Fail),
                    duration_ms: u64::MAX,
                    line: lines.clone(),
                    tail: lines.clone(),
                }),
                refusal: many(&lines),
                denials: many(&big)
                    .into_iter()
                    .map(|target| DeniedCall {
                        tool: big.clone(),
                        target: Some(target),
                    })
                    .collect(),
            })
            .collect(),
        flaky_tests: many(&big),
        parked: vec![
            ParkedFacts {
                reason: ParkReason::DailyCap,
                ms: 1,
            };
            500
        ],
        landing: Some(Landing {
            mode: LandingMode::Pr,
            branch: big.clone(),
            commits: many(&big),
            pull_request: Some(u64::MAX),
        }),
        diff: Some(DiffStat {
            files: u32::MAX,
            insertions: u64::MAX,
            deletions: u64::MAX,
            protected: many(&big),
        }),
        summary: Some(AgentSummary {
            attempt: 1,
            head: big.clone(),
            done: lines.clone(),
            decisions: many(&lines),
            deviations: many(&big),
            questions: many(&lines),
            followups: many(&big),
        }),
        bill: Bill {
            eqv: f64::MAX,
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: u64::MAX,
            wall_clock_ms: u64::MAX,
            commits: u32::MAX,
        },
    };
    for options in [true, false].map(ReportOptions::for_repository) {
        let body = render_report(&facts, options);
        assert!(chars(&body) <= BODY_CAP, "{}", chars(&body));
    }
}

#[test]
fn ten_backticks_cannot_close_the_fence() {
    let run = "`".repeat(10);
    let escape = format!("{run}\n@evil Fixes #1 <img src=x>\n{run}");
    let mut facts = facts();
    facts.attempts = vec![attempt(
        1,
        AttemptState::Fail,
        &format!("before\n{escape}\nafter"),
    )];
    facts.flaky_tests = vec![format!("{run} @evil {run}")];
    facts.summary = Some(AgentSummary {
        attempt: 1,
        head: "0123abc".into(),
        done: format!("````\n{escape}\n````"),
        ..AgentSummary::default()
    });
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(!prose(&body).contains("evil"), "{body}");
    assert!(foreign_html(&body).is_empty(), "{body}");
    let blocks = code_blocks(&body);
    assert!(
        blocks
            .iter()
            .any(|b| b.contains(&run) && b.contains("@evil") && b.ends_with("after\n")),
        "{body}"
    );
}

#[test]
fn the_agents_summary_is_quoted_marked_and_inert() {
    let corpus: Vec<String> = CORPUS.iter().map(|s| (*s).to_owned()).collect();
    let mut facts = facts();
    facts.title = "Fixes #1 for @someone".into();
    facts.detail = Some("/merge <b>x</b> owner/repo#2".into());
    facts.flaky_tests = corpus.clone();
    facts.attempts = vec![AttemptFacts {
        verdict: Some(VerdictFacts {
            first: RunResult::Fail,
            rerun: None,
            duration_ms: 1,
            line: CORPUS.join(" "),
            tail: CORPUS.join("\n"),
        }),
        refusal: corpus.clone(),
        denials: corpus
            .iter()
            .map(|target| DeniedCall {
                tool: target.clone(),
                target: Some(target.clone()),
            })
            .collect(),
        ..attempt(1, AttemptState::Fail, "")
    }];
    facts.summary = Some(AgentSummary {
        attempt: 1,
        head: "0123abc".into(),
        done: CORPUS.join("\n\n"),
        decisions: corpus.clone(),
        deviations: corpus.clone(),
        questions: corpus.clone(),
        followups: corpus,
    });
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(foreign_html(&body).is_empty(), "{:?}", foreign_html(&body));
    // The daemon's own section headings and the marker line aside, the prose is inert.
    let section = body
        .split("#### Agent's summary")
        .nth(1)
        .and_then(|s| s.split("#### Verdict output").next())
        .expect("an agent section");
    assert!(
        section.trim_start().starts_with("> *Written by the agent"),
        "{section}"
    );
    let unquoted: String = section
        .trim_end()
        .lines()
        .skip_while(|l| l.is_empty())
        .map(|l| {
            assert!(
                l.starts_with('>'),
                "an unquoted line in the agent's section: {l:?}"
            );
            l.strip_prefix("> ").unwrap_or(l.trim_start_matches('>'))
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(inert(&unquoted), None, "{unquoted}");
    let head = body
        .split("#### Agent's summary")
        .next()
        .unwrap_or_default();
    let head = head.split_once('\n').map_or("", |(_, rest)| rest);
    assert_eq!(inert(head), None, "{head}");
}

#[test]
fn a_tail_is_cut_from_the_front_at_a_line_boundary() {
    assert_eq!(truncate_front("abc", 10), ("abc", 0));
    assert_eq!(truncate_front("one\ntwo\nthree", 8), ("three", 2));
    assert_eq!(truncate_front("one\ntwo\nthree", 9), ("two\nthree", 1));
    assert_eq!(truncate_front("abcdef", 3), ("def", 0));
}

#[test]
fn numbers_and_durations_read_well() {
    assert_eq!(group(0), "0");
    assert_eq!(group(1_234_567), "1,234,567");
    assert_eq!(duration(450), "450 ms");
    assert_eq!(duration(12_345), "12.3 s");
    assert_eq!(duration(65_000), "1m 05s");
    assert_eq!(eqv(f64::NAN), "n/a");
}

#[test]
fn not_ready_runs_are_counted_apart_from_the_verdict() {
    let passed = |unready| AttemptFacts {
        verdict: Some(VerdictFacts {
            first: RunResult::Pass,
            rerun: None,
            duration_ms: 1_000,
            line: String::new(),
            tail: String::new(),
        }),
        unready,
        ..attempt(1, AttemptState::Pass, "")
    };
    let mut facts = facts();
    facts.attempts = vec![passed(2)];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(
        body.contains(", verdict pass (after 2 not-ready runs) in 1.0 s"),
        "{body}"
    );
    facts.attempts = vec![passed(1)];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(body.contains("pass (after 1 not-ready run) in"), "{body}");
    facts.attempts = vec![passed(0)];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(body.contains(", verdict pass in 1.0 s"), "{body}");
    assert!(!body.contains("not ready"), "{body}");

    // Every run so far was not ready: no verdict judged.
    facts.attempts = vec![AttemptFacts {
        verdict: None,
        unready: 3,
        ..attempt(1, AttemptState::VerdictRunning, "")
    }];
    facts.parked = vec![ParkedFacts {
        reason: ParkReason::Unready,
        ms: 300_000,
    }];
    let body = render_report(&facts, ReportOptions::for_repository(false));
    assert!(body.contains(", verdict not ready (3 runs)"), "{body}");
    assert!(body.contains("verdict not ready 5m 00s"), "{body}");
}
