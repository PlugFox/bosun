//! The tool calls the agent CLI refused (#7: refusals per attempt): read from the session's
//! tail at its end, stored on its attempt with tool, target and reason, and counted per tool on
//! the attempt's line of the sticky report (a local-file scene's repository counts as public,
//! so no target is shown); and the deny rules a session gets for its protected paths, less the
//! card's `allow-protected`, so the CLI refuses such an edit as it happens.

use super::*;

/// The result line the CLI writes at the end of a session that had two calls refused.
const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_x1","tool_input":{"command":"git push origin HEAD"}},{"tool_name":"Bash","tool_use_id":"toolu_x2","tool_input":{"command":"curl https://example.invalid"}}]}"#;

/// Card 1's session prints a result line with two denied `Bash` calls, then commits; the item
/// ends Done. The store holds both denials on attempt 1 with their targets, and the sticky
/// report's attempt line counts them per tool without naming either target.
/// `mutant_runner_drop_denials` and `mutant_loop_denials_not_written` leave the store empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_tool_call_is_stored_on_its_attempt_and_shown_in_its_report() {
    let scene = Scene::new().await;
    scene.write_board(&[card(1, "Denied", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[format!("print {RESULT}"), "commit one.txt one".to_owned()],
    );
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, Duration::from_secs(60)).await;
    assert_eq!(state, State::Done(Outcome::Pass));
    drained(&scene).await;
    task.abort();
    watcher.abort();

    let id = ItemId::new("demo#1");
    let denials = scene.store.reader().denials(&id).await.unwrap();
    let stored: Vec<(u32, &str, Option<&str>)> = denials
        .iter()
        .map(|d| (d.attempt, d.tool.as_str(), d.target.as_deref()))
        .collect();
    assert_eq!(
        stored,
        [
            (1, "Bash", Some("git push origin HEAD")),
            (1, "Bash", Some("curl https://example.invalid")),
        ]
    );
    assert!(
        denials
            .iter()
            .all(|d| d.source == bosun_store::DenialSource::Permission),
        "{denials:?}"
    );

    let cards = scene.cards();
    let card = cards.iter().find(|c| c.id.0 == 1).unwrap();
    let report = card
        .comments
        .iter()
        .find(|c| c.key.as_deref() == Some("report:demo#1"))
        .map(|c| c.body.clone())
        .expect("the sticky report");
    let line = report
        .lines()
        .find(|l| l.starts_with("- Attempt 1:"))
        .unwrap_or_else(|| panic!("no attempt line in {report}"));
    assert!(line.contains("; 2 tool calls denied (Bash 2)"), "{line}");
    for target in ["git push", "example.invalid"] {
        assert!(!report.contains(target), "{target} in {report}");
    }
}

/// The values after `--disallowedTools` in the argv the fake agent of item `n` wrote.
fn denied_of(scene: &Scene, n: u64) -> Vec<String> {
    let argv = std::fs::read_to_string(scene.path(&format!("argv{n}"))).unwrap();
    argv.lines()
        .skip_while(|a| *a != "--disallowedTools")
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .map(str::to_owned)
        .collect()
}

/// The policy protects `docs/frozen` and `harness.rs` (and always `.gitignore`); card 2 allows
/// `docs/frozen`. Card 1's session is denied edits of all three, card 2's of the two it was not
/// allowed, after the push rules. `mutant_runner_no_protected_rules` leaves every protected rule
/// out; `mutant_loop_protected_ignores_allowance` denies card 2 `docs/frozen` too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_is_denied_edits_of_its_protected_paths_less_the_cards_allowance() {
    let scene = Scene::new().await;
    std::fs::write(
        &scene.config_path,
        scene.config_text("'docs/frozen', 'harness.rs'"),
    )
    .unwrap();
    scene.write_board(&[
        card(1, "Plain", "test -f one.txt", "Item one."),
        card_allowing(2, "Allowed", "test -f two.txt", "docs/frozen"),
    ]);
    let p = |name: &str| scene.path(name).display().to_string();
    for (n, file) in [(1, "one.txt"), (2, "two.txt")] {
        scene.script(
            n,
            &[
                format!("argv {}", p(&format!("argv{n}"))),
                format!("commit {file} x"),
            ],
        );
    }
    let (task, watcher) = scene.start(None);
    for n in [1, 2] {
        let state = ended(&scene, n, Duration::from_secs(60)).await;
        assert_eq!(state, State::Done(Outcome::Pass), "item {n}");
    }
    drained(&scene).await;
    task.abort();
    watcher.abort();

    let one = denied_of(&scene, 1);
    for rule in [
        "Bash(git push:*)",
        "Edit(/docs/frozen)",
        "Edit(/docs/frozen/**)",
        "Edit(harness.rs)",
        "Edit(.gitignore)",
    ] {
        assert!(
            one.iter().any(|r| r == rule),
            "card 1 lacks {rule}: {one:?}"
        );
    }
    let push = one.iter().position(|r| r == "Bash(git push:*)").unwrap();
    let edit = one.iter().position(|r| r.starts_with("Edit(")).unwrap();
    assert!(push < edit, "the push rules come first: {one:?}");

    let two = denied_of(&scene, 2);
    for rule in ["Edit(harness.rs)", "Edit(.gitignore)"] {
        assert!(
            two.iter().any(|r| r == rule),
            "card 2 lacks {rule}: {two:?}"
        );
    }
    assert!(
        !two.iter().any(|r| r.contains("docs/frozen")),
        "card 2 is allowed docs/frozen: {two:?}"
    );
    for rule in one.iter().chain(&two) {
        assert!(
            !rule.starts_with("Write(") && !rule.starts_with("NotebookEdit("),
            "{rule}"
        );
    }
}
