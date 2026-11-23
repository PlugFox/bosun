//! The commit reader of one identity (#23, #26): the commits of its configured repositories,
//! read into its store at start, every [`INTERVAL`], and by every reingest.
//!
//! Only the report reads commits (eqv per commit), so a scan never runs on a report request:
//! a request only reads, and says per repository how the last scan went.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_api::ScanState;
use bosun_config::Repo;
use bosun_store::{Reader, Writer};
use tokio::task::JoinHandle;

/// How often every repository is scanned.
const INTERVAL: Duration = Duration::from_secs(300);

/// The repositories of one identity and the outcome of each one's last scan.
#[derive(Debug, Clone)]
pub(crate) struct Commits {
    reader: Reader,
    writer: Writer,
    /// The repositories, by name.
    repos: Arc<Vec<(String, Repo)>>,
    /// The last scan of each repository, by name.
    states: Arc<Mutex<BTreeMap<String, ScanState>>>,
}

impl Commits {
    pub(crate) fn new(reader: Reader, writer: Writer, repos: Vec<(String, Repo)>) -> Self {
        let states = repos
            .iter()
            .map(|(name, _)| (name.clone(), ScanState::Pending))
            .collect();
        Self {
            reader,
            writer,
            repos: Arc::new(repos),
            states: Arc::new(Mutex::new(states)),
        }
    }

    /// Scans every repository once, one after the other, and records each outcome. A
    /// repository that fails does not stop the others.
    pub(crate) async fn refresh(&self) {
        for (name, repo) in self.repos.iter() {
            let state =
                match bosun_ingest::git::refresh(&self.reader, &self.writer, &repo.path).await {
                    Ok(_) => ScanState::Done,
                    Err(e) => ScanState::Failed {
                        error: e.to_string(),
                    },
                };
            self.states
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name.clone(), state);
        }
    }

    /// Scans now and then every [`INTERVAL`] until the task is aborted.
    pub(crate) fn spawn(&self) -> JoinHandle<()> {
        let commits = self.clone();
        tokio::spawn(async move {
            loop {
                commits.refresh().await;
                tokio::time::sleep(INTERVAL).await;
            }
        })
    }

    /// The last scan of the repository named `name`.
    pub(crate) fn state(&self, name: &str) -> ScanState {
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
            .unwrap_or(ScanState::Pending)
    }
}
