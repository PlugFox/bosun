//! What the daemon serves: one store and one ingest per identity (#2, rule 4), and the
//! start-up and shutdown around them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bosun_api::rpc::{ErrorObject, code};
use bosun_client::framing;
use bosun_client::{Endpoint, Stream};
use bosun_config::{Config, Repo};
use bosun_ingest::{Handle, Ingest, Options as IngestOptions, claude};
use bosun_store::{Options as StoreOptions, Store};
use tokio::io::BufReader;
use tokio::task::{JoinHandle, JoinSet};

use crate::api;
use crate::commits::Commits;
use crate::listen::{self, ClaimError, Listener};

/// One identity: its own store and its own ingest.
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
}

/// Everything a request can reach.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) started_ms: i64,
    pub(crate) identities: BTreeMap<String, Served>,
}

impl State {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            started_ms: 0,
            identities: BTreeMap::new(),
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

    let identities = match open(&config).await {
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
    });

    serve(listener, &state, stop).await;
    match Arc::try_unwrap(state) {
        Ok(state) => shut_down(state.identities).await,
        Err(_) => eprintln!("bosund: a connection outlived shutdown; stores left to the OS"),
    }
    eprintln!("bosund: stopped");
    Ok(())
}

/// Opens each identity's store and starts its ingest. On failure, what was opened is returned
/// with the error so it can be closed.
async fn open(
    config: &Config,
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
            },
        );
    }
    Ok(identities)
}

async fn shut_down(identities: BTreeMap<String, Served>) {
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
async fn serve(mut listener: Listener, state: &Arc<State>, stop: impl Future<Output = ()>) {
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

/// Answers the requests of one connection, one at a time, until it closes.
async fn connection(state: Arc<State>, stream: Box<dyn Stream>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    while let Ok(Some(line)) = framing::read_line(&mut read).await {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = api::handle_line(&state, &line).await
            && framing::write_message(&mut write, &response).await.is_err()
        {
            return;
        }
    }
}
