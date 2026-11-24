//! The feed against a fake daemon on a real pipe (Windows) or socket (Unix): what a key sends,
//! a daemon restart under a running view, and a resync of the Session tab's tail.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bosun_api::rpc::{ErrorObject, Request, Response, code};
use bosun_api::{
    ControlStatus, CurrentItem, Event, HelloResult, Overview, QueueView, RepoControl, Resync,
    ResyncReason, SessionTail, SubscribeParams, SubscribeResult, TailParams,
};
use bosun_client::Endpoint;
use bosun_client::framing;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use super::*;
use crate::app::Key;
use crate::app::tests::{overview, queue};

/// The token the fake daemon takes.
pub(crate) const TOKEN: &str = "bosun-operator-test";

#[expect(clippy::unnecessary_wraps, reason = "the type of `Feed::operator`")]
pub(crate) fn token(_: &Endpoint) -> Result<String, String> {
    Ok(TOKEN.to_owned())
}

/// What the fake daemon answers, and what it was asked.
#[derive(Debug)]
struct State {
    epoch: String,
    overview: Overview,
    queue: QueueView,
    /// The methods called, in order, `hello` and `events.subscribe` excluded.
    calls: Vec<String>,
    /// The `release` of each `control.kill`, in order.
    kills: Vec<bool>,
    /// The output lines of the fake's ring, oldest first, and the head.
    lines: Vec<Event>,
    head: u64,
    /// What each tail subscription is sent after its answer, one batch per subscription.
    pushes: VecDeque<Vec<Push>>,
}

/// One step of what the fake daemon does on a tail subscription.
#[derive(Debug, Clone)]
enum Push {
    /// Waits until the test says so.
    Wait(Arc<Notify>),
    /// Publishes a line into the ring; sent to the subscriber when `deliver`.
    Publish { event: Event, deliver: bool },
    /// The ring drops its lines up to this sequence.
    Drop(u64),
    /// Sends a `lagged` resync and closes the connection, as the daemon does.
    Resync,
}

/// A fake daemon that can be stopped and started again under the same endpoint.
pub(crate) struct Fake {
    pub(crate) endpoint: Endpoint,
    state: Arc<Mutex<State>>,
    server: Option<JoinHandle<()>>,
    /// The socket's directory (Unix).
    _dir: tempfile::TempDir,
}

impl Fake {
    pub(crate) fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = if cfg!(windows) {
            Endpoint::new(format!("bosun-tui-test-{}-{name}", std::process::id()))
        } else {
            Endpoint::new(dir.path().join("sock"))
        };
        let now = 1_790_000_000_000;
        let mut fake = Self {
            endpoint,
            state: Arc::new(Mutex::new(State {
                epoch: "1-1".to_owned(),
                overview: overview("personal", now),
                queue: queue("personal"),
                calls: Vec::new(),
                kills: Vec::new(),
                lines: Vec::new(),
                head: 0,
                pushes: VecDeque::new(),
            })),
            server: None,
            _dir: dir,
        };
        fake.start();
        fake
    }

    fn start(&mut self) {
        let listening = listen(&self.endpoint);
        self.server = Some(tokio::spawn(serve(listening, Arc::clone(&self.state))));
    }

    /// Stops serving and closes every connection, as a daemon that exits.
    pub(crate) async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    /// The `release` of each `control.kill` the fake was sent.
    pub(crate) fn kills(&self) -> Vec<bool> {
        self.state.lock().unwrap().kills.clone()
    }
}

#[cfg(windows)]
type Listening = (
    std::path::PathBuf,
    tokio::net::windows::named_pipe::NamedPipeServer,
);
#[cfg(unix)]
type Listening = tokio::net::UnixListener;

#[cfg(windows)]
fn listen(endpoint: &Endpoint) -> Listening {
    use tokio::net::windows::named_pipe::ServerOptions;
    let path = endpoint.path().to_owned();
    let started = Instant::now();
    loop {
        // The previous daemon's last instance may take a moment to close.
        match ServerOptions::new().first_pipe_instance(true).create(&path) {
            Ok(server) => return (path, server),
            Err(e) if started.elapsed() < Duration::from_secs(5) => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

#[cfg(unix)]
fn listen(endpoint: &Endpoint) -> Listening {
    let _ = std::fs::remove_file(endpoint.path());
    tokio::net::UnixListener::bind(endpoint.path()).unwrap()
}

#[cfg(windows)]
async fn serve(listening: Listening, state: Arc<Mutex<State>>) {
    use tokio::net::windows::named_pipe::ServerOptions;
    let (path, mut server) = listening;
    let mut connections = JoinSet::new();
    loop {
        if server.connect().await.is_err() {
            return;
        }
        let next = ServerOptions::new().create(&path).unwrap();
        let connected = std::mem::replace(&mut server, next);
        connections.spawn(connection(connected, Arc::clone(&state)));
    }
}

#[cfg(unix)]
async fn serve(listener: Listening, state: Arc<Mutex<State>>) {
    let mut connections = JoinSet::new();
    while let Ok((stream, _)) = listener.accept().await {
        connections.spawn(connection(stream, Arc::clone(&state)));
    }
}

/// Answers one connection's requests; a subscription is answered and then held open.
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(stream: S, state: Arc<Mutex<State>>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    while let Ok(Some(line)) = framing::read_line(&mut read).await {
        let request: Request = serde_json::from_str(&line).unwrap();
        let id = request.id.clone().unwrap_or(Value::Null);
        let answer = answer(&request, &state);
        let response = match answer {
            Ok(result) => Response::result(id, result),
            Err(message) => Response::error(
                id,
                ErrorObject {
                    code: code::UNAUTHORIZED,
                    message,
                    data: None,
                },
            ),
        };
        if framing::write_message(&mut write, &response).await.is_err() {
            return;
        }
        let tail =
            request.method == "events.subscribe" && !request.params["filter"]["tail"].is_null();
        if tail {
            let batch = state.lock().unwrap().pushes.pop_front().unwrap_or_default();
            if !push(batch, &state, &mut write).await {
                return;
            }
        }
    }
}

/// A notification line of `method`.
fn note(method: &str, params: &Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Plays one batch on a tail subscription; false when the connection is closed after it.
async fn push<W: AsyncWrite + Unpin>(
    batch: Vec<Push>,
    state: &Mutex<State>,
    write: &mut W,
) -> bool {
    for step in batch {
        let note = match step {
            Push::Wait(go) => {
                go.notified().await;
                None
            }
            Push::Publish { event, deliver } => {
                let mut state = state.lock().unwrap();
                state.head = event.seq;
                state.lines.push(event.clone());
                deliver.then(|| {
                    note(
                        bosun_api::event::EVENT,
                        &serde_json::to_value(&event).unwrap(),
                    )
                })
            }
            Push::Drop(upto) => {
                state.lock().unwrap().lines.retain(|e| e.seq > upto);
                None
            }
            Push::Resync => {
                let (epoch, head) = {
                    let state = state.lock().unwrap();
                    (state.epoch.clone(), state.head)
                };
                let resync = Resync {
                    epoch,
                    head,
                    reason: ResyncReason::Lagged,
                };
                let note = note(
                    bosun_api::event::RESYNC,
                    &serde_json::to_value(&resync).unwrap(),
                );
                let _ = framing::write_message(write, &note).await;
                return false;
            }
        };
        if let Some(note) = note
            && framing::write_message(write, &note).await.is_err()
        {
            return false;
        }
    }
    true
}

fn answer(request: &Request, state: &Mutex<State>) -> Result<Value, String> {
    let mut state = state.lock().unwrap();
    let params = &request.params;
    let operator = || {
        if params["operator"] == TOKEN {
            Ok(())
        } else {
            Err("the operator token is wrong".to_owned())
        }
    };
    let value = match request.method.as_str() {
        "hello" => serde_json::to_value(HelloResult {
            api_version: bosun_api::API_VERSION,
            daemon_version: "0.0.0".to_owned(),
            pid: 1,
        }),
        "events.subscribe" => {
            let ask: SubscribeParams = serde_json::from_value(params.clone()).unwrap();
            let restarted = ask.epoch.is_some_and(|e| e != state.epoch);
            serde_json::to_value(SubscribeResult {
                identity: ask.identity,
                epoch: state.epoch.clone(),
                head: state.head,
                resync: restarted.then_some(bosun_api::ResyncReason::Restarted),
            })
        }
        method => {
            state.calls.push(method.to_owned());
            match method {
                "status.overview" => serde_json::to_value(&state.overview),
                "queue.get" => serde_json::to_value(&state.queue),
                "session.tail" => {
                    let ask: TailParams = serde_json::from_value(params.clone()).unwrap();
                    let first = state.lines.first().map_or(state.head + 1, |e| e.seq);
                    let from = ask.from_seq.unwrap_or(0);
                    serde_json::to_value(SessionTail {
                        identity: ask.identity,
                        item: ask.item,
                        epoch: state.epoch.clone(),
                        head: state.head,
                        lines: state
                            .lines
                            .iter()
                            .filter(|e| e.seq >= from)
                            .cloned()
                            .collect(),
                        gap: ask.from_seq.is_some_and(|f| f < first),
                        truncated: first > 1,
                        session: None,
                        context: None,
                    })
                }
                "control.pause" | "control.resume" => {
                    operator()?;
                    let paused = method == "control.pause";
                    serde_json::to_value(ControlStatus {
                        identity: params["identity"].as_str().unwrap_or_default().to_owned(),
                        killed: false,
                        paused: false,
                        daily_cap_until_ms: None,
                        repos: vec![RepoControl {
                            repo: params["repo"].as_str().unwrap_or_default().to_owned(),
                            paused,
                            auto_paused_until_ms: None,
                            hold: paused.then(|| "pause".to_owned()),
                        }],
                    })
                }
                "control.kill" => {
                    operator()?;
                    let release = params["release"].as_bool().unwrap_or_default();
                    state.kills.push(release);
                    Ok(json!({"killed": !release}))
                }
                "item.restart" => {
                    operator()?;
                    Ok(json!({"item": params["item"], "label": "restart"}))
                }
                other => return Err(format!("the fake daemon does not know {other}")),
            }
        }
    };
    Ok(value.unwrap())
}

/// The calls that change something.
pub(crate) fn actions_of(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .map(String::as_str)
        .filter(|m| m.starts_with("control.") || *m == "item.restart")
        .collect()
}

/// A key without its confirmation sends nothing (#73); with the mutant `mutant_tui_no_confirm`
/// the pause is sent on the key alone.
#[tokio::test]
async fn a_key_without_its_confirmation_sends_nothing() {
    let mut fake = Fake::new("confirm");
    let mut app = App::new(["personal".to_owned()]);
    let mut feed = Feed::new(fake.endpoint.clone()).with_operator(token);
    feed.refresh(&mut app, 0).await;
    assert_eq!(app.link, Link::Up);

    for spec in bosun_client::actions::ACTIONS {
        for answer in [Key::Char('n'), Key::Esc, Key::Enter] {
            assert!(!crate::on_key(&mut app, &mut feed, Key::Char(spec.key)).await);
            assert!(!crate::on_key(&mut app, &mut feed, answer).await);
        }
    }
    assert_eq!(actions_of(&fake.calls()), Vec::<&str>::new());

    // The same key, answered yes, is sent once.
    assert!(!crate::on_key(&mut app, &mut feed, Key::Char('p')).await);
    assert_eq!(actions_of(&fake.calls()), Vec::<&str>::new());
    assert!(!crate::on_key(&mut app, &mut feed, Key::Char('y')).await);
    assert_eq!(actions_of(&fake.calls()), vec!["control.pause"]);
    assert_eq!(app.message.as_deref(), Some("alpha (personal) is paused"),);
    fake.stop().await;
}

/// The run loop's reaction to a signal or a tick, as `drive` does it.
async fn settle(
    app: &mut App,
    feed: &mut Feed,
    changes: &mut mpsc::UnboundedReceiver<Changed>,
    tick: Duration,
) {
    tokio::select! {
        Some(_) = changes.recv() => {
            while changes.try_recv().is_ok() {}
            feed.refresh(app, 0).await;
        }
        () = tokio::time::sleep(tick) => {
            if !matches!(app.link, Link::Up) {
                feed.refresh(app, 0).await;
            }
        }
    }
}

/// The view survives a daemon restart without a relaunch (#73): the new daemon's answers
/// reach the screen; with `mutant_tui_no_reconnect` the request connection is never opened
/// again and the screen stays on the old daemon.
#[tokio::test]
async fn the_view_survives_a_daemon_restart() {
    let mut fake = Fake::new("restart");
    let mut app = App::new(["personal".to_owned()]);
    let mut feed = Feed::new(fake.endpoint.clone()).with_operator(token);
    let (tx, mut changes) = mpsc::unbounded_channel();
    let watcher = watch(fake.endpoint.clone(), "personal".to_owned(), tx);
    // The watcher's first subscription is a signal.
    settle(&mut app, &mut feed, &mut changes, Duration::from_secs(10)).await;
    let first = crate::view::tests::screen(&app, 0);
    assert!(first.contains("alpha#3"), "{first}");
    assert!(first.contains("bosund 0.0.0"), "{first}");

    fake.stop().await;
    {
        let mut state = fake.state.lock().unwrap();
        state.epoch = "2-2".to_owned();
        state.overview.repos[1].current = Some(CurrentItem {
            item: "beta#8".to_owned(),
            state: "verifying".to_owned(),
            detail: None,
            attempt: 1,
            since_ms: state.overview.now_ms,
        });
    }
    // While the daemon is away the view keeps what it had.
    feed.refresh(&mut app, 0).await;
    assert!(matches!(app.link, Link::Down(_)), "{:?}", app.link);
    let away = crate::view::tests::screen(&app, 0);
    assert!(away.contains("daemon unreachable: reconnecting"), "{away}");
    assert!(away.contains("alpha#3"), "{away}");

    fake.start();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        settle(
            &mut app,
            &mut feed,
            &mut changes,
            Duration::from_millis(200),
        )
        .await;
        let now = crate::view::tests::screen(&app, 0);
        if now.contains("beta#8") && app.link == Link::Up {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the view never reached the restarted daemon:\n{now}"
        );
    }
    watcher.abort();
    fake.stop().await;
}

/// The `line-N` lines of the screen, in order.
fn lines_on(screen: &str) -> Vec<u64> {
    screen
        .split_whitespace()
        .filter_map(|w| w.strip_prefix("line-"))
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// #74: after a resync marker the Session tab refetches its tail and shows no gap.
///
/// The view has lines 1-3 and is delivered line 4. The fake daemon then publishes lines 5 and 6
/// without delivering them, its ring drops everything up to line 5, and it sends a `lagged`
/// resync. After the resync the view must show a contiguous run of lines up to line 7, which
/// comes live after the reconnect; with `mutant_tui_ignore_resync` it shows 1-4 and 7.
#[tokio::test]
async fn after_a_resync_the_tail_is_refetched_with_no_gap() {
    let line = crate::session::tests::line;
    let go = Arc::new(Notify::new());
    let mut fake = Fake::new("resync");
    {
        let mut state = fake.state.lock().unwrap();
        state.lines = (1..=3).map(|n| line(n, &format!("line-{n}"))).collect();
        state.head = 3;
        let publish = |n: u64, deliver: bool| Push::Publish {
            event: line(n, &format!("line-{n}")),
            deliver,
        };
        state.pushes = VecDeque::from([
            vec![
                Push::Wait(Arc::clone(&go)),
                publish(4, true),
                publish(5, false),
                publish(6, false),
                Push::Drop(5),
                Push::Resync,
            ],
            vec![publish(7, true)],
        ]);
    }
    let mut app = App::new(["personal".to_owned()]);
    let mut feed = Feed::new(fake.endpoint.clone()).with_operator(token);
    feed.refresh(&mut app, 0).await;
    let _ = app.on_key(Key::Char('3'));
    let (tx, mut tails) = mpsc::unbounded_channel();
    let mut tailing = Tailing::new(fake.endpoint.clone(), tx);
    tailing.follow(&mut app, &mut feed).await;
    assert_eq!(
        lines_on(&crate::view::tests::screen(&app, 0)),
        [1, 2, 3],
        "the first fetch"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut opened = false;
    loop {
        let changed = tokio::time::timeout_at(deadline.into(), tails.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the tail never reached line 7:\n{}",
                    crate::view::tests::screen(&app, 0)
                )
            })
            .expect("the watcher runs");
        let open = changed.signal == TailSignal::Opened;
        on_tail(&mut app, &mut feed, changed).await;
        if open && !opened {
            // Subscribed and caught up: now the daemon publishes past the subscriber.
            opened = true;
            go.notify_one();
        }
        if app
            .session
            .as_ref()
            .and_then(|s| s.lines.back())
            .is_some_and(|e| e.seq == 7)
        {
            break;
        }
    }
    let screen = crate::view::tests::screen(&app, 0);
    let shown = lines_on(&screen);
    assert_eq!(shown.last(), Some(&7), "{screen}");
    assert!(
        shown.windows(2).all(|w| w[1] == w[0] + 1),
        "a gap in the tail {shown:?}:\n{screen}"
    );
    assert!(shown.contains(&6), "{screen}");
    assert!(
        screen.contains("earlier lines are no longer held"),
        "{screen}"
    );
    drop(tailing);
    fake.stop().await;
}
