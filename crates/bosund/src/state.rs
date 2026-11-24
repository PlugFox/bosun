//! What the daemon serves: one store, one ingest and one event stream per identity (#2, rule
//! 4), and the start-up and shutdown around them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{EventBody, Wide};
use bosun_client::framing;
use bosun_client::{Endpoint, Stream};
use bosun_config::{Config, Queue, Repo, SystemProbe};
use bosun_ingest::{Handle, Ingest, Options as IngestOptions, Report, claude};
use bosun_proc::{NativeSpawner, SystemPower};
use bosun_queue::LocalFile;
use bosun_store::{Options as StoreOptions, Store};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::agent::Credentials;
use crate::api::{self, Answer};
use crate::commits::Commits;
use crate::config_watch;
use crate::events::{self, Batch, Events, Subscriber};
use crate::executor;
use crate::listen::{self, ClaimError, Listener};

/// How often the ingest's counters are compared for `ingest_progress` and `ingest_pass`.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// The most events written to a subscriber in one batch.
const BATCH: usize = 256;

/// How often a repository's loop polls its board and wakes a parked item.
const LOOP_POLL: Duration = Duration::from_secs(2);

/// One identity: its own store, its own ingest and its own event stream.
#[derive(Debug)]
pub(crate) struct Served {
    pub(crate) name: String,
    pub(crate) database: PathBuf,
    pub(crate) transcripts: PathBuf,
    /// The repositories bound to this identity, by name.
    pub(crate) repos: Vec<(String, Repo)>,
    pub(crate) store: Store,
    pub(crate) ingest: Handle,
    /// The commit reader of the repositories.
    pub(crate) commits: Commits,
    /// Its timer task; aborted at shutdown.
    pub(crate) commit_task: JoinHandle<()>,
    /// The identity's events; the loop (#62) publishes here too.
    pub(crate) events: Events,
}

/// Everything a request can reach.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) started_ms: i64,
    pub(crate) identities: BTreeMap<String, Served>,
    /// The live agent credentials of every identity's sessions (#63).
    pub(crate) credentials: Credentials,
}

impl State {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            started_ms: 0,
            identities: BTreeMap::new(),
            credentials: Credentials::default(),
        }
    }

    /// The identity a request names. Every method but `hello` goes through here, so no
    /// response holds rows of another identity than the one asked for.
    pub(crate) fn identity(&self, name: &str) -> Result<&Served, ErrorObject> {
        let found = if cfg!(mutant_api_first_identity) {
            // Mutant: the identity argument is ignored.
            self.identities.values().next()
        } else {
            self.identities.get(name)
        };
        found.ok_or_else(|| {
            ErrorObject::new(
                code::UNKNOWN_IDENTITY,
                format!("this daemon serves no identity named {name:?}"),
            )
        })
    }
}

/// Why the daemon stopped other than by a signal.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Another instance listens at the endpoint.
    Running {
        endpoint: Endpoint,
        pid: Option<u32>,
    },
    /// Anything else: the endpoint, a store, an ingest.
    Other(String),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Runs the daemon until `stop` completes, then shuts down: stops listening (removing a Unix
/// socket), stops every ingest after its pass in progress, and closes every store, which
/// checkpoints and truncates its WAL.
pub(crate) async fn run(
    config: Config,
    config_path: PathBuf,
    endpoint: Endpoint,
    stop: impl Future<Output = ()>,
) -> Result<(), Failure> {
    let listener = listen::claim(&endpoint).await.map_err(|e| match e {
        ClaimError::Running(pid) => Failure::Running {
            endpoint: endpoint.clone(),
            pid,
        },
        ClaimError::Io(e) => Failure::Other(format!("cannot listen: {e}")),
    })?;

    // A restart starts a new epoch, so a client's old sequence is never taken for a new one.
    let epoch = format!("{}-{}", now_ms(), std::process::id());
    let identities = match open(&config, &epoch, events::Options::default()).await {
        Ok(identities) => identities,
        Err((opened, e)) => {
            listener.close();
            shut_down(opened).await;
            return Err(Failure::Other(e));
        }
    };
    eprintln!(
        "bosund: listening at {endpoint}; {} identities",
        identities.len()
    );
    let state = Arc::new(State {
        started_ms: now_ms(),
        identities,
        credentials: Credentials::default(),
    });

    let progress = tokio::spawn(watch_ingest(state.clone()));
    // The configuration in force, replaced by each valid edit of the file (#55, #62).
    let (config_tx, config_rx) = watch::channel(Arc::new(config.clone()));
    let watcher = config_watch::spawn(
        config_path,
        config_tx,
        Arc::new(SystemProbe::new()),
        config_watch::INTERVAL,
        config_watch::Reported::default(),
    );
    let loops = start_loops(&config, &state, &config_rx);
    drop(config_rx);
    serve(listener, &state, stop).await;
    // The watcher holds the sender: without it every loop ends, killing its child first.
    watcher.abort();
    let _ = watcher.await;
    for task in loops {
        let _ = task.await;
    }
    progress.abort();
    let _ = progress.await;
    match Arc::try_unwrap(state) {
        Ok(state) => shut_down(state.identities).await,
        Err(_) => eprintln!("bosund: a connection outlived shutdown; stores left to the OS"),
    }
    eprintln!("bosund: stopped");
    Ok(())
}

/// Opens each identity's store and starts its ingest. On failure, what was opened is returned
/// with the error so it can be closed.
pub(crate) async fn open(
    config: &Config,
    epoch: &str,
    options: events::Options,
) -> Result<BTreeMap<String, Served>, (BTreeMap<String, Served>, String)> {
    let mut identities = BTreeMap::new();
    for plan in bosun_ingest::plans(config) {
        let store = match Store::open(&plan.database, StoreOptions::default()).await {
            Ok(store) => store,
            Err(e) => {
                let e = format!(
                    "identity {}: {}: {e}",
                    plan.identity,
                    plan.database.display()
                );
                return Err((identities, e));
            }
        };
        let ingest = Ingest::new(
            claude::Parser,
            plan.claude_projects.clone(),
            &store,
            IngestOptions::default(),
        );
        if let Err(e) = ingest.register_repos(&plan.repo_roots).await {
            let _ = store.close().await;
            return Err((identities, format!("identity {}: {e}", plan.identity)));
        }
        let repos = config
            .repos
            .iter()
            .filter(|(_, repo)| repo.identity == plan.identity)
            .map(|(name, repo)| (name.clone(), repo.clone()))
            .collect::<Vec<_>>();
        let commits = Commits::new(
            store.reader().clone(),
            store.writer().clone(),
            repos.clone(),
        );
        let commit_task = commits.spawn();
        identities.insert(
            plan.identity.clone(),
            Served {
                name: plan.identity,
                database: plan.database,
                transcripts: plan.claude_projects,
                repos,
                store,
                ingest: bosun_ingest::spawn(ingest),
                commits,
                commit_task,
                events: Events::new(epoch, options),
            },
        );
    }
    Ok(identities)
}

pub(crate) async fn shut_down(identities: BTreeMap<String, Served>) {
    for (name, served) in identities {
        served.commit_task.abort();
        let _ = served.commit_task.await;
        if let Err(e) = served.ingest.shutdown().await {
            eprintln!("bosund: identity {name}: ingest: {e}");
        }
        if let Err(e) = served.store.close().await {
            eprintln!("bosund: identity {name}: closing the store: {e}");
        }
    }
}

/// Accepts connections until `stop` completes, then ends every connection.
pub(crate) async fn serve(
    mut listener: Listener,
    state: &Arc<State>,
    stop: impl Future<Output = ()>,
) {
    let mut connections = JoinSet::new();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    connections.spawn(connection(state.clone(), stream));
                }
                Err(e) => {
                    eprintln!("bosund: accept: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    listener.close();
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

/// Answers the requests of one connection, one at a time, until it closes or subscribes; a
/// subscribed connection carries its events until either side ends it.
async fn connection(state: Arc<State>, stream: Box<dyn Stream>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut conn = api::Connection::default();
    while let Ok(Some(line)) = framing::read_line(&mut read).await {
        if line.trim().is_empty() {
            continue;
        }
        match api::handle_line(&state, &mut conn, &line).await {
            Answer::Reply(None) => {}
            Answer::Reply(Some(response)) => {
                if framing::write_message(&mut write, &response).await.is_err() {
                    return;
                }
            }
            Answer::Subscribe(response, subscriber) => {
                // The stream needs nothing else of the state; a shutdown need not wait for it.
                drop(state);
                if framing::write_message(&mut write, &response).await.is_ok() {
                    stream_events(read, write, subscriber).await;
                }
                return;
            }
        }
    }
}

/// Writes a subscriber's events until the client goes away, falls behind the ring (it is sent
/// a resync), or stalls a write beyond the stream's limit. Nothing here holds the ring while it
/// writes, so a slow client slows nobody else.
async fn stream_events(
    mut read: impl AsyncRead + Unpin,
    mut write: impl AsyncWrite + Unpin,
    mut subscriber: Subscriber,
) {
    let stall = subscriber.stall();
    // What the client sends after subscribing is read only to see the stream end.
    let mut ignored = [0u8; 512];
    loop {
        match subscriber.take(BATCH) {
            Batch::Lagged(resync) => {
                let _ = tokio::time::timeout(stall, async {
                    write.write_all(&resync).await?;
                    write.flush().await
                })
                .await;
                return;
            }
            Batch::Events(batch) if batch.is_empty() => {
                tokio::select! {
                    () = subscriber.changed() => {}
                    read = read.read(&mut ignored) => {
                        if matches!(read, Ok(0) | Err(_)) {
                            return;
                        }
                    }
                }
            }
            Batch::Events(batch) => {
                let written = tokio::time::timeout(stall, async {
                    for event in &batch {
                        write.write_all(&event.line).await?;
                    }
                    write.flush().await
                })
                .await;
                match (written, batch.last()) {
                    (Ok(Ok(())), Some(last)) => subscriber.delivered(last.seq),
                    // A broken or stalled client is dropped.
                    _ => return,
                }
            }
        }
    }
}

/// Publishes each identity's ingest progress: `ingest_progress` when it parsed more bytes,
/// `ingest_pass` or `ingest_failed` when the outcome of its last timed pass changed.
async fn watch_ingest(state: Arc<State>) {
    /// What was last published of an identity: bytes parsed, and the last pass's outcome.
    type Seen = (u64, Option<Result<Report, String>>);
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    loop {
        tick.tick().await;
        for (name, served) in &state.identities {
            let (parsed, last) = seen.entry(name.as_str()).or_default();
            let now = served.ingest.parsed_bytes();
            if now != *parsed {
                *parsed = now;
                served.events.publish(EventBody::IngestProgress {
                    parsed_since_start: Wide(now),
                });
            }
            let pass = served.ingest.last_pass();
            if pass != *last {
                if let Some(outcome) = &pass {
                    served.events.publish(api::pass_event(false, outcome));
                }
                *last = pass;
            }
        }
    }
}

/// This daemon's id on boards: host and OS user, as claim markers need it stable (#61).
fn daemon_id() -> String {
    let var = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok())
            .unwrap_or_else(|| "unknown".to_owned())
    };
    let raw = format!(
        "{}/{}",
        var(&["COMPUTERNAME", "HOSTNAME"]),
        var(&["USERNAME", "USER"])
    );
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_./".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(100)
        .collect()
}

/// Starts the loop of every repository whose queue is a local file (#62). A Projects v2 board
/// is not run by the loop yet.
fn start_loops(
    config: &Config,
    state: &State,
    config_rx: &watch::Receiver<Arc<Config>>,
) -> Vec<JoinHandle<()>> {
    let agent = bosun_config::tools::find_program("claude", std::env::var_os("PATH").as_deref());
    let daemon_id = daemon_id();
    let mut loops = Vec::new();
    for (name, repo) in &config.repos {
        let file = match &repo.queue {
            Some(Queue::LocalFile { file }) => file.clone(),
            Some(_) => {
                eprintln!(
                    "bosund: repository {name}: only a local-file queue is run in this version"
                );
                continue;
            }
            None => continue,
        };
        let Some(served) = state.identities.get(&repo.identity) else {
            continue;
        };
        let work_dir = served
            .database
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("bosun-work")
            .join(&served.name);
        let options = executor::Options {
            repo: name.clone(),
            daemon_id: daemon_id.clone(),
            agent: agent.clone(),
            work_dir,
            poll: LOOP_POLL,
            recheck: Duration::from_mins(1),
            question_grace: executor::QUESTION_GRACE,
            #[cfg(test)]
            failpoint: None,
        };
        let deps = executor::Deps {
            reader: served.store.reader().clone(),
            writer: served.store.writer().clone(),
            events: served.events.clone(),
            queue: Arc::new(LocalFile::new(file)),
            spawner: Arc::new(NativeSpawner),
            power: Arc::new(SystemPower),
            logins: Arc::new(executor::GhLogins),
            credentials: state.credentials.clone(),
            clock: Arc::new(executor::SystemClock::new()),
            alive: Arc::new(bosun_proc::is_running),
        };
        eprintln!("bosund: repository {name}: the loop runs");
        loops.push(tokio::spawn(
            executor::Loop::new(options, deps, config_rx.clone()).run(),
        ));
    }
    loops
}
