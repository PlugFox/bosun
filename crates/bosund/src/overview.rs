//! `status.overview` (#4, #70): per repository of an identity, its state, the item in flight,
//! the last verdict and the control state, read from the identity's own store.

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{CurrentItem, LastVerdict, Method, Overview, RepoOverview, StreamStatus, method};
use bosun_config::Queue;
use bosun_store::StoredItem;
use serde_json::Value;

use crate::api::{call, failed};
use crate::state::{Served, State};

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The name of a repository's queue kind.
pub(crate) fn queue_kind(queue: Option<&Queue>) -> &'static str {
    match queue {
        Some(Queue::LocalFile { .. }) => "local_file",
        Some(Queue::Projects { .. }) => "projects_v2",
        Some(_) => "other",
        None => "none",
    }
}

/// Where a repository's queue is: the local file's path as configured, or `<owner>/<number>`
/// of a Projects v2 board; `None` for no queue or another kind.
pub(crate) fn queue_source(queue: Option<&Queue>) -> Option<String> {
    match queue {
        Some(Queue::LocalFile { file }) => Some(file.display().to_string()),
        Some(Queue::Projects { owner, number }) => Some(format!("{owner}/{number}")),
        Some(_) | None => None,
    }
}

/// The `status.*` methods.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::StatusOverview::NAME => {
            call::<method::StatusOverview, _, _>(p, |p| async move {
                overview(state, state.identity(&p.identity)?).await
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

async fn overview(state: &State, served: &Served) -> Result<Overview, ErrorObject> {
    let now = now_ms();
    let names: Vec<String> = served.repos.iter().map(|(name, _)| name.clone()).collect();
    let control = state.control.status(&served.name, &names, now);
    let items = served.store.reader().items(false).await.map_err(failed)?;
    // The lease manager, read once for every repository of the identity.
    let waits = crate::leases::waits_of(state, served);
    let mut repos = Vec::new();
    for ((name, repo), held) in served.repos.iter().zip(&control.repos) {
        let mine: Vec<&StoredItem> = items
            .iter()
            .filter(|i| crate::executor::of_repo(i, name, repo))
            .collect();
        let current = mine.iter().find(|i| i.item.state.is_active()).copied();
        // The item in flight, else the one that moved last.
        let latest = current.or_else(|| mine.iter().max_by_key(|i| i.updated_ms).copied());
        let last_verdict = match latest {
            Some(item) => last_verdict(served, item).await?,
            None => None,
        };
        repos.push(RepoOverview {
            repo: name.clone(),
            path: repo.path.display().to_string(),
            queue: queue_kind(repo.queue.as_ref()).to_owned(),
            queue_source: queue_source(repo.queue.as_ref()),
            state: if current.is_some() { "working" } else { "idle" }.to_owned(),
            hold: held.hold.clone(),
            current: current.map(|i| CurrentItem {
                item: i.item.id.to_string(),
                state: i.item.state.as_str().to_owned(),
                detail: crate::events::reason_of(i.item.state),
                attempt: i.item.attempt.as_ref().map_or(0, |a| a.number),
                since_ms: i.updated_ms,
                waiting: crate::leases::waiting_in(&waits, &i.item.id),
            }),
            last_verdict,
        });
    }
    Ok(Overview {
        identity: served.name.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        started_ms: state.started_ms,
        now_ms: now,
        killed: control.killed,
        paused: control.paused,
        daily_cap_until_ms: control.daily_cap_until_ms,
        events: StreamStatus {
            epoch: served.events.epoch().to_owned(),
            head: served.events.head(),
            subscribers: served.events.subscribers() as u64,
        },
        repos,
    })
}

/// The last verdict run of `item` that finished.
async fn last_verdict(
    served: &Served,
    item: &StoredItem,
) -> Result<Option<LastVerdict>, ErrorObject> {
    let runs = served
        .store
        .reader()
        .verdict_runs(&item.item.id)
        .await
        .map_err(failed)?;
    Ok(runs.into_iter().rev().find_map(|run| {
        Some(LastVerdict {
            item: item.item.id.to_string(),
            attempt: run.attempt,
            run: run.run,
            result: crate::events::run_result_name(run.result?).to_owned(),
            ended_ms: run.ended_ms?,
        })
    }))
}
