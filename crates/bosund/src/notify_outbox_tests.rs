//! The notification drainer (#9, "Delivery through the outbox") over the served state of
//! `notify_tests`: identities `a` (repository `alpha`) and `b` (`beta`), each with its own
//! store, credential store and drainer, and `fake` channels with their own sinks.

use std::time::{Duration, Instant};

use bosun_core::{Ctx, Event, Item, ItemId, State as ItemState, Stepped, step};
use bosun_store::{NewItem, NewNotice, Write};
use serde_json::json;

use crate::notify_tests::{TOKEN, World};

/// Long enough for many polls of the drainer (50 ms under `cfg(test)`).
const SETTLE: Duration = Duration::from_millis(1_000);

/// Adds the `fake` channel `ops` of `identity` for `needs_you`, writing to `sink`, with no
/// minimum gap; `fail` failed sends first.
async fn channel(world: &World, identity: &str, sink: &str, fail: u32) {
    world
        .ok(
            "notify.channel.add",
            json!({
                "operator": TOKEN,
                "identity": identity,
                "name": "ops",
                "kind": "fake",
                "params": {"sink": sink, "fail": fail.to_string()},
                "events": ["needs_you"],
                "min_gap_s": 0,
            }),
        )
        .await;
}

/// Registers card 1 of `repo` in `identity`'s store and sends it to Needs you (lint) at its
/// claim; returns the lint transition.
async fn needs_you(world: &World, identity: &str, repo: &str) -> Stepped {
    let served = &world.state.identities[identity];
    let (_, configured) = served.repos.iter().find(|(name, _)| name == repo).unwrap();
    let id = ItemId::new(format!("{repo}#1"));
    let ctx = Ctx::at(crate::executor::now_ms());
    let claimed = step(&Item::new(id.clone()), &Event::Claim, &ctx).unwrap();
    let linted = step(&claimed.item, &Event::LintFailed, &ctx).unwrap();
    assert_eq!(
        linted.item.state,
        ItemState::NeedsYou(bosun_core::Reason::Lint)
    );
    let at_ms = crate::executor::now_ms();
    served
        .store
        .writer()
        .write(vec![
            Write::Item(NewItem {
                item: id,
                name: repo.to_owned(),
                repo: configured.key.clone(),
                card_key: Some("card-1".to_owned()),
                at_ms,
            }),
            Write::Transition {
                stepped: claimed,
                at_ms,
            },
            Write::Transition {
                stepped: linted.clone(),
                at_ms,
            },
        ])
        .await
        .unwrap();
    linted
}

/// The notice of `linted`, queued again by hand under the same key.
async fn queue_again(world: &World, identity: &str, repo: &str, linted: &Stepped) {
    let notice = NewNotice {
        key: format!("needs_you:{}", linted.transition.key),
        event: "needs_you".to_owned(),
        repo: repo.to_owned(),
        item: Some(linted.transition.key.item.to_string()),
        facts: [
            ("reason".to_owned(), "lint".to_owned()),
            ("attempt".to_owned(), linted.transition.attempt.to_string()),
        ]
        .into(),
        at_ms: crate::executor::now_ms(),
    };
    world.state.identities[identity]
        .store
        .writer()
        .write(vec![Write::NoticeQueued(notice)])
        .await
        .unwrap();
}

async fn wait_for(what: &str, limit: Duration, mut done: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !done().await {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// #9: a needs-you item is one message, also when the daemon restarts between the event and
/// the delivery, and when the same notice is queued again under its key before and after it
/// (`mutant_notify_no_dedup_key`: each queue is a row of its own, and two go out).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_needs_you_notification_is_delivered_once_across_a_daemon_restart() {
    const SINK: &str = "notify-outbox-tests-restart";
    let mut world = World::new().await;
    channel(&world, "a", SINK, 1).await;
    let linted = needs_you(&world, "a", "alpha").await;
    queue_again(&world, "a", "alpha", &linted).await;

    // The first send fails: the rows wait for their retry.
    wait_for(
        "the first send to fail",
        Duration::from_secs(10),
        async || {
            world.state.identities["a"]
                .store
                .reader()
                .pending_notices("ops")
                .await
                .unwrap()
                .iter()
                .any(|r| r.tries >= 1)
        },
    )
    .await;
    assert_eq!(bosun_notify::fake::sent(SINK), Vec::<String>::new());

    world.restart().await;
    wait_for("the delivery", Duration::from_secs(10), async || {
        !bosun_notify::fake::sent(SINK).is_empty()
    })
    .await;
    queue_again(&world, "a", "alpha", &linted).await;
    tokio::time::sleep(SETTLE).await;

    let sent = bosun_notify::fake::sent(SINK);
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert!(
        sent[0].starts_with("bosun a: alpha#1 needs you (lint)"),
        "{sent:#?}"
    );
    assert!(!sent[0].contains("notices"), "one notice: {sent:#?}");
    assert_eq!(
        world.state.identities["a"]
            .store
            .reader()
            .pending_notices("ops")
            .await
            .unwrap(),
        []
    );
    world.close().await;
}

/// #9, #2: a needs-you item of `b` reaches `b`'s channel and never `a`'s, though both are
/// subscribed (`mutant_notify_shared_channel` sends through every identity's channels).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_needs_you_item_of_one_identity_never_reaches_another_identitys_channel() {
    const SINK_A: &str = "notify-outbox-tests-isolation-a";
    const SINK_B: &str = "notify-outbox-tests-isolation-b";
    let world = World::new().await;
    channel(&world, "a", SINK_A, 0).await;
    channel(&world, "b", SINK_B, 0).await;
    needs_you(&world, "b", "beta").await;

    wait_for("b's message", Duration::from_secs(10), async || {
        !bosun_notify::fake::sent(SINK_B).is_empty()
    })
    .await;
    tokio::time::sleep(SETTLE).await;
    let b = bosun_notify::fake::sent(SINK_B);
    assert_eq!(b.len(), 1, "{b:#?}");
    assert!(
        b[0].starts_with("bosun b: beta#1 needs you (lint)"),
        "{b:#?}"
    );
    assert_eq!(bosun_notify::fake::sent(SINK_A), Vec::<String>::new());
    world.close().await;
}
