//! The event stream over the real endpoint (#69): a daemon's state served on its own pipe or
//! socket, with the test as the publisher the loop (#62) will be.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bosun_api::{EventBody, EventFilter, OutputStream, ResyncReason, Tail};
use bosun_client::{Cursor, Delivery, Endpoint, Subscription};
use bosun_config::SystemProbe;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::events::{self, Events};
use crate::state::{self, State};

const IDENTITY: &str = "personal";

/// How long one delivery may take before the test gives up on it.
const PATIENCE: Duration = Duration::from_secs(10);

/// A served state with one identity and no transcripts, on an endpoint of its own.
struct Served {
    _dir: TempDir,
    endpoint: Endpoint,
    state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Served {
    async fn start(options: events::Options) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir_all(claude.join("projects")).unwrap();
        let config = format!(
            "[identity.{IDENTITY}]\nclaude_config_dir = '{}'\ndatabase = '{}'\n",
            claude.display(),
            dir.path().join("bosun.db").display()
        );
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config).unwrap();
        let config = bosun_config::load(&path, &SystemProbe::new()).unwrap();
        let identities = state::open(&config, "test-epoch", options)
            .await
            .map_err(|(_, e)| e)
            .unwrap();
        let state = Arc::new(State {
            started_ms: 0,
            identities,
            credentials: crate::agent::Credentials::default(),
            control: crate::control::Control::default(),
            leases: std::sync::Arc::new(crate::leases::Leases::none()),
            gh: std::path::PathBuf::from("gh"),
            blockers: std::sync::Arc::default(),
        });
        let endpoint = if cfg!(windows) {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            // A bare name: the client adds the pipe prefix.
            Endpoint::new(format!("bosun-test-events-{}-{n}", std::process::id()))
        } else {
            Endpoint::new(dir.path().join("bosund.sock"))
        };
        let listener = crate::listen::claim(&endpoint).await.unwrap();
        let (stop, stopped) = oneshot::channel();
        let served = state.clone();
        let task = tokio::spawn(async move {
            state::serve(listener, &served, async {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            _dir: dir,
            endpoint,
            state,
            stop: Some(stop),
            task: Some(task),
        }
    }

    fn events(&self) -> Events {
        self.state.identities[IDENTITY].events.clone()
    }

    async fn subscribe(&self, filter: EventFilter) -> Subscription {
        Subscription::open(self.endpoint.clone(), IDENTITY, filter)
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        if let Ok(state) = Arc::try_unwrap(self.state) {
            state::shut_down(state.identities).await;
        }
    }
}

fn options(capacity: usize, stall: Duration) -> events::Options {
    events::Options { capacity, stall }
}

fn resumed(attempt: u32) -> EventBody {
    EventBody::LimitResumed {
        item: "a".to_owned(),
        attempt,
    }
}

/// An event of about a kilobyte on the wire.
fn heavy(n: u32) -> EventBody {
    EventBody::BoardSyncError {
        repo: format!("repo-{n}"),
        error: "x".repeat(1000),
    }
}

async fn next(sub: &mut Subscription) -> Delivery {
    tokio::time::timeout(PATIENCE, sub.next())
        .await
        .expect("no delivery in time")
        .unwrap()
}

async fn next_seq(sub: &mut Subscription) -> u64 {
    match next(sub).await {
        Delivery::Event(event) => event.seq,
        Delivery::Resync(resync) => panic!("a resync: {resync:?}"),
    }
}

/// Reads until a resync, asserting that the events before it are contiguous from 1.
async fn contiguous_then_resync(sub: &mut Subscription) -> ResyncReason {
    let mut expected = 1;
    loop {
        match next(sub).await {
            Delivery::Event(event) => {
                assert_eq!(event.seq, expected, "a silent gap");
                expected += 1;
            }
            Delivery::Resync(resync) => return resync.reason,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_gets_every_missed_event_in_order() {
    let served = Served::start(options(64, Duration::from_secs(5))).await;
    let events = served.events();
    let mut sub = served.subscribe(EventFilter::default()).await;
    for n in 1..=3 {
        events.publish(resumed(n));
    }
    for seq in 1..=3 {
        assert_eq!(next_seq(&mut sub).await, seq);
    }

    // Away while five events are published: all five come back, in order.
    sub.disconnect();
    for n in 4..=8 {
        events.publish(resumed(n));
    }
    for seq in 4..=8 {
        assert_eq!(next_seq(&mut sub).await, seq);
    }

    // Away while the ring moves past the cursor: a resync, never a silent gap.
    sub.disconnect();
    for n in 9..=108 {
        events.publish(resumed(n));
    }
    let Delivery::Resync(resync) = next(&mut sub).await else {
        panic!("the missed events were not held, yet no resync");
    };
    assert_eq!(resync.reason, ResyncReason::Gone);
    assert_eq!(resync.head, 108);
    events.publish(resumed(109));
    assert_eq!(next_seq(&mut sub).await, 109);
    served.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cursor_of_another_epoch_is_a_resync() {
    let served = Served::start(options(64, Duration::from_secs(5))).await;
    let events = served.events();
    events.publish(resumed(1));
    let cursor = Cursor {
        epoch: "an-earlier-run".to_owned(),
        seq: 0,
    };
    let mut sub = Subscription::resume(
        served.endpoint.clone(),
        IDENTITY,
        EventFilter::default(),
        cursor,
    )
    .await
    .unwrap();
    let Delivery::Resync(resync) = next(&mut sub).await else {
        panic!("an old epoch's sequence was taken for this one");
    };
    assert_eq!(resync.reason, ResyncReason::Restarted);
    assert_eq!(sub.cursor().unwrap().epoch, "test-epoch");
    events.publish(resumed(2));
    assert_eq!(next_seq(&mut sub).await, 2);
    served.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_stops_reading_is_dropped_and_never_slows_the_publisher() {
    let stall = Duration::from_secs(1);
    // The ring holds everything: the subscriber cannot lag, so only the stall can drop it.
    let served = Served::start(options(100_000, stall)).await;
    let events = served.events();

    let mut baseline = Duration::ZERO;
    for n in 0..200 {
        let start = Instant::now();
        events.publish(heavy(n));
        baseline = baseline.max(start.elapsed());
    }

    // Subscribed from the next event, then never read: the pipe fills and the write stalls.
    let mut stalled = Subscription::resume(
        served.endpoint.clone(),
        IDENTITY,
        EventFilter::default(),
        Cursor {
            epoch: "test-epoch".to_owned(),
            seq: events.head(),
        },
    )
    .await
    .unwrap();
    let first = events.head() + 1;
    assert_eq!(events.subscribers(), 1);
    let mut slowest = Duration::ZERO;
    for n in 0..3000 {
        let start = Instant::now();
        events.publish(heavy(n));
        slowest = slowest.max(start.elapsed());
        if n % 100 == 0 {
            tokio::task::yield_now().await;
        }
    }
    assert!(
        slowest < baseline + Duration::from_millis(100),
        "a publish took {slowest:?} with a stalled subscriber, {baseline:?} without"
    );

    let deadline = Instant::now() + stall * 5;
    while events.subscribers() > 0 {
        assert!(
            Instant::now() < deadline,
            "the stalled subscriber was never dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Reading again: what the pipe held, then the rest from the ring after the reconnect, in
    // order and without a gap.
    for seq in first..=events.head() {
        assert_eq!(next_seq(&mut stalled).await, seq, "a silent gap");
    }
    served.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_falls_behind_gets_a_resync_marker() {
    // A long stall limit: the subscriber is slow, not gone, and is told so.
    let served = Served::start(options(16, Duration::from_secs(60))).await;
    let events = served.events();
    let mut slow = served.subscribe(EventFilter::default()).await;
    for n in 0..3000 {
        events.publish(heavy(n));
    }
    assert_eq!(
        contiguous_then_resync(&mut slow).await,
        ResyncReason::Lagged
    );
    // The daemon closed the connection after the marker. The reconnect starts after the
    // marker's head; when the ring has moved past that too, it is one more resync.
    // The event published before that resync is covered by it, so publish again after it.
    loop {
        let seq = events.publish(resumed(1));
        match next(&mut slow).await {
            Delivery::Event(event) => {
                assert_eq!(event.seq, seq);
                break;
            }
            Delivery::Resync(resync) => assert_eq!(resync.reason, ResyncReason::Gone),
        }
    }
    served.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn output_lines_reach_only_subscriptions_that_opted_in() {
    let served = Served::start(options(64, Duration::from_secs(5))).await;
    let events = served.events();
    let mut plain = served.subscribe(EventFilter::default()).await;
    let mut tail = served
        .subscribe(EventFilter {
            kinds: None,
            tail: Some(Tail::default()),
        })
        .await;
    events.publish(EventBody::SessionOutput {
        item: "a".to_owned(),
        attempt: 1,
        spawn: 1,
        stream: OutputStream::Stdout,
        line: "working".to_owned(),
        tool: None,
    });
    events.publish(resumed(1));
    let Delivery::Event(first) = next(&mut plain).await else {
        panic!("a resync");
    };
    assert_eq!(
        (first.seq, first.body.kind()),
        (2, "limit_resumed"),
        "an output line reached a subscription without the tail"
    );
    let Delivery::Event(line) = next(&mut tail).await else {
        panic!("a resync");
    };
    assert_eq!((line.seq, line.body.kind()), (1, "session_output"));
    assert_eq!(next_seq(&mut tail).await, 2);
    served.stop().await;
}
