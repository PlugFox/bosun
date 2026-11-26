//! The tool calls the agent CLI refused (#7: refusals per attempt): read from the session's
//! tail at its end, stored on its attempt with tool, target and reason, and counted per tool on
//! the attempt's line of the sticky report (a local-file scene's repository counts as public,
//! so no target is shown).

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
