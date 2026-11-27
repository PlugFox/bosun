//! The fast-death breaker in the loop (#7): sessions that keep dying at once, across items,
//! pause the repository until `bosun resume` instead of draining the queue to Needs you; the
//! session that trips it charges its item nothing.

use bosun_api::EventFilter;
use bosun_core::ParkReason;

use super::*;
use crate::events::{Batch, Subscriber};

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(120);

fn subscribe_fast_deaths(scene: &Scene) -> Subscriber {
    let filter = EventFilter {
        kinds: Some(vec!["fast_deaths".to_owned()]),
        tail: None,
    };
    scene.events.subscribe("test", None, None, filter).1
}

/// The `fast_deaths` events `sub` has received so far, as their bodies.
fn fast_deaths(sub: &mut Subscriber) -> Vec<serde_json::Value> {
    match sub.take(1_000) {
        Batch::Events(events) => events
            .iter()
            .map(|e| {
                let v: serde_json::Value = serde_json::from_slice(&e.line).unwrap();
                v["params"]["body"].clone()
            })
            .collect(),
        Batch::Lagged(_) => panic!("lagged"),
    }
}

pub(super) fn tripped(scene: &Scene) -> Option<String> {
    let now = scene.clock.wall_ms();
    let status = scene.control.status("test", &["demo".into()], now);
    status.repos.into_iter().find(|r| r.repo == "demo")?.tripped
}

/// An agent that dies at once in every session: its first crash takes the crash retry, the
/// failed resume costs an uncounted spawn, and the third quick death in a row trips the
/// breaker: card 1 waits parked, uncharged, and card 2 is never claimed, until the resume
/// (`mutant_loop_no_fast_death_breaker`: card 1 drains to Needs you).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_sessions_dying_at_once_pause_the_repository_until_resume() {
    let scene = Scene::new().await;
    let mut sub = subscribe_fast_deaths(&scene);
    scene.write_board(&[
        card(1, "Dies at once", "test -f one.txt", "Item one."),
        card(2, "Waits", "test -f two.txt", "Item two."),
    ]);
    scene.script(1, &["exit 1".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    let mut state = None;
    wait_for("card 1 to park paused", LIMIT, async || {
        state = scene.state(1).await;
        matches!(state, Some(State::Parked(_) | State::NeedsYou(_)))
    })
    .await;
    let Some(State::Parked(park)) = state else {
        panic!("card 1 is {state:?}, not parked");
    };
    assert_eq!(park.reason, ParkReason::Paused);
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0), "charged nothing more");
    assert_eq!(scene.runs(1).await.len(), 3);
    let why = tripped(&scene).expect("the repository is tripped");
    assert!(why.contains("3 sessions"), "{why}");
    let events = fast_deaths(&mut sub);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["repo"], "demo");
    assert_eq!(events[0]["sessions"], 3);
    assert_eq!(events[0]["within_ms"], 60_000);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        matches!(scene.state(2).await, None | Some(State::Ready)),
        "card 2 was claimed: {:?}",
        scene.state(2).await
    );
    assert_eq!(scene.runs(1).await.len(), 3, "no new session of card 1");
    assert!(matches!(scene.state(1).await, Some(State::Parked(_))));

    scene.script(1, &["commit one.txt one".to_owned()]);
    let now = scene.clock.wall_ms();
    scene.control.resume("test", Some("demo"), now);
    assert_eq!(tripped(&scene), None, "the resume lifts the trip");
    let one = ended(&scene, 1, LIMIT).await;
    let two = ended(&scene, 2, LIMIT).await;
    task.abort();
    watcher.abort();
    assert_eq!(one, State::Done(Outcome::Pass));
    assert_eq!(two, State::Done(Outcome::Pass));
    let runs = scene.runs(1).await;
    assert_eq!(
        modes(&runs).last(),
        Some(&(1, SpawnMode::Reset)),
        "a fresh session of the same attempt: {:?}",
        modes(&runs)
    );
    let item = scene.item(1).await.unwrap();
    assert_eq!((item.attempts, item.failed), (1, 0));
}

/// An agent CLI that cannot be spawned (its path does not exist): every session dies before it
/// starts, and those are fast deaths too. After three in a row the breaker trips: the
/// repository is paused, card 1 waits parked and card 2 is never claimed
/// (`mutant_breaker_blind_to_spawn_failure`: nothing trips, card 1 drains to Needs you).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_whose_agent_cannot_be_spawned_trips_the_fast_death_breaker() {
    let mut scene = Scene::new().await;
    scene.agent_program = Some(scene.path("bin").join("no-such-agent"));
    let mut sub = subscribe_fast_deaths(&scene);
    scene.write_board(&[
        card(1, "Cannot start", "test -f one.txt", "Item one."),
        card(2, "Waits", "test -f two.txt", "Item two."),
    ]);
    let (task, watcher) = scene.start(None);
    let mut state = None;
    wait_for("card 1 to park or end", LIMIT, async || {
        state = scene.state(1).await;
        matches!(state, Some(State::Parked(_) | State::NeedsYou(_)))
    })
    .await;
    let Some(State::Parked(park)) = state else {
        panic!("card 1 is {state:?}, not parked");
    };
    assert_eq!(park.reason, ParkReason::Paused);
    let why = tripped(&scene).expect("the repository is tripped");
    assert!(why.contains("3 sessions"), "{why}");
    let events = fast_deaths(&mut sub);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["repo"], "demo");
    assert_eq!(events[0]["sessions"], 3);

    tokio::time::sleep(Duration::from_secs(2)).await;
    task.abort();
    watcher.abort();
    assert!(
        matches!(scene.state(2).await, None | Some(State::Ready)),
        "card 2 was claimed: {:?}",
        scene.state(2).await
    );
    assert!(matches!(scene.state(1).await, Some(State::Parked(_))));
}

/// Three quick crashes in all, never three in a row: a passing session between them resets the
/// streak, and both cards end Done (`mutant_loop_fast_death_streak_never_resets`: card 2's
/// second crash trips the breaker).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quick_passing_session_between_crashes_resets_the_streak() {
    let scene = Scene::new().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "Crashes once", "test -f one.txt", "Item one."),
        card(2, "Crashes twice", "test -f two.txt", "Item two."),
    ]);
    scene.script(
        1,
        &[
            format!("once {} exit 1", m("m1")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(
        2,
        &[
            format!("once {} exit 1", m("m2")),
            format!("once {} exit 1", m("m3")),
            "commit two.txt two".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let one = ended(&scene, 1, LIMIT).await;
    let mut two = None;
    wait_for("card 2 to end or park", LIMIT, async || {
        two = scene.state(2).await;
        terminal(two) || matches!(two, Some(State::Parked(_)))
    })
    .await;
    task.abort();
    watcher.abort();
    assert_eq!(one, State::Done(Outcome::Pass));
    assert_eq!(two, Some(State::Done(Outcome::Pass)));
    assert_eq!(tripped(&scene), None);
    assert_eq!(scene.runs(1).await.len(), 2);
    assert_eq!(scene.runs(2).await.len(), 3);
}
