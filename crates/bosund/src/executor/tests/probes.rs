//! The resource probe before a claim (#8, "Leases": a lease says who holds a resource, not
//! whether it answers): a card that needs a resource whose probe fails waits in Ready with no
//! session spent, the next card that does not need it runs meanwhile, the failure is told once,
//! the probe runs again only after its backoff, and the card is claimed once the probe passes.

use std::collections::BTreeMap;

use bosun_api::EventFilter;
use bosun_store::{NewChannel, NoticeRow};

use super::leases::card_needing;
use super::*;
use crate::events::{Batch, Subscriber};

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(180);

/// Writes the channel `ops` for `resource_unhealthy` to the scene's store.
async fn channel(scene: &Scene) {
    scene
        .store
        .writer()
        .write(vec![Write::ChannelAdded(NewChannel {
            name: "ops".to_owned(),
            kind: "fake".to_owned(),
            params: BTreeMap::from([("sink".to_owned(), "executor-tests-probes".to_owned())]),
            events: vec!["resource_unhealthy".to_owned()],
            min_gap_ms: 0,
            secret_entry: "notify/test/ops".to_owned(),
            at_ms: 1,
        })])
        .await
        .unwrap();
}

async fn unhealthy(scene: &Scene) -> Vec<NoticeRow> {
    let rows = scene.store.reader().pending_notices("ops").await.unwrap();
    rows.into_iter()
        .filter(|r| r.event == "resource_unhealthy")
        .collect()
}

fn subscribe(scene: &Scene) -> Subscriber {
    let filter = EventFilter {
        kinds: Some(vec!["resource_probe".to_owned()]),
        tail: None,
    };
    scene.events.subscribe("test", None, None, filter).1
}

/// The `resource_probe` events `sub` received since the last call, as their bodies.
fn probe_events(sub: &mut Subscriber) -> Vec<serde_json::Value> {
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

/// How many times the probe ran: the lines of `<scene>/probes`.
fn probes(scene: &Scene) -> usize {
    std::fs::read_to_string(scene.path("probes")).map_or(0, |t| t.lines().count())
}

/// `[resource.rig]`'s probe counts its runs in `probes` and passes once `rig-up` exists. Card 1
/// needs `rig`, card 2 needs nothing. While `rig-up` is missing card 1 stays in Ready (no claim
/// marker, no attempt, no run) and card 2 runs to Done; the probe ran once, and its failure is
/// told once (one event, one notice) over some twenty polls. After the 30 s backoff it runs
/// once more, untold. Once `rig-up` exists and the next backoff ran out, card 1 is claimed and
/// ends Done, and the streak's end is published. `mutant_loop_skip_probe` runs card 1 against
/// the dead rig; `mutant_loop_probe_holds_the_repository` never claims card 2;
/// `mutant_leases_probe_no_backoff` probes at every poll; `mutant_loop_probe_told_every_time`
/// tells every poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_probe_keeps_its_card_ready_and_lets_the_next_card_run() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = format!(
        "[resource.rig]\nkind = \"device\"\nprobe = '''echo probe >> \"{}\"; test -f \"{}\"'''\n",
        sh_path(&scene.path("probes")),
        sh_path(&scene.path("rig-up")),
    );
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
    channel(&scene).await;
    let mut sub = subscribe(&scene);
    scene.write_board(&[
        card_needing(1, "test -f one.txt", &["rig"]),
        card_needing(2, "test -f two.txt", &[]),
    ]);
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);

    let two = ended(&scene, 2, LIMIT).await;
    assert!(matches!(two, State::Done(_)), "{two:?}");
    let one = scene.item(1).await.expect("card 1 has its row");
    assert_eq!(one.state, State::Ready, "{one:?}");
    assert_eq!(one.attempts, 0, "{one:?}");
    assert_eq!(scene.runs(1).await.len(), 0);
    let on_board = scene.cards();
    let card_one = on_board.iter().find(|c| c.id.0 == 1).unwrap();
    assert_eq!(card_one.column, Column::Ready);
    assert!(
        !card_one
            .comments
            .iter()
            .any(|c| c.key.as_deref().is_some_and(|k| k.starts_with("claim:"))),
        "{card_one:?}"
    );
    let waits = scene.blockers.probing_of("demo");
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0].item, ItemId::new("demo#1"));
    assert_eq!(waits[0].resource, "rig");
    assert!(waits[0].streak.detail.starts_with("exit 1"), "{waits:?}");

    // Some twenty polls more, the clock where it is: no probe, nothing told again.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(probes(&scene), 1, "the probe ran again within its backoff");
    let events = probe_events(&mut sub);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["healthy"], false);
    assert_eq!(events[0]["resource"], "rig");
    assert_eq!(events[0]["item"], "demo#1");
    assert!(
        events[0]["detail"]
            .as_str()
            .is_some_and(|d| d.starts_with("exit 1")),
        "{events:?}"
    );
    let rows = unhealthy(&scene).await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0].item.as_deref(), Some("demo#1"));
    assert_eq!(rows[0].facts["resource"], "rig");
    assert_eq!(rows[0].facts["failure"], "exit 1");
    assert!(
        rows[0].key.starts_with("resource_unhealthy:rig/"),
        "{rows:#?}"
    );

    // The backoff runs out (nothing is in flight): the probe runs once more, untold.
    scene.clock.advance(Duration::from_secs(31));
    wait_for("the second probe", LIMIT, async || probes(&scene) >= 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(probes(&scene), 2);
    assert_eq!(probe_events(&mut sub), Vec::<serde_json::Value>::new());
    assert_eq!(unhealthy(&scene).await.len(), 1);

    // The rig comes up; the next backoff (60 s) runs out: card 1 is claimed and ends Done.
    std::fs::write(scene.path("rig-up"), "").unwrap();
    scene.clock.advance(Duration::from_secs(61));
    let one = ended(&scene, 1, LIMIT).await;
    assert!(matches!(one, State::Done(_)), "{one:?}");
    let events = probe_events(&mut sub);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["healthy"], true);
    assert_eq!(events[0]["resource"], "rig");
    assert!(events[0].get("detail").is_none(), "{events:?}");
    assert_eq!(scene.blockers.probing_of("demo"), []);
    assert_eq!(unhealthy(&scene).await.len(), 1);

    task.abort();
    watcher.abort();
    let _ = task.await;
    let _ = watcher.await;
}
