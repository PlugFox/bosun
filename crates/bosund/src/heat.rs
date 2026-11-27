//! Heat's proposals in the daemon (#11, part D): `heat.evaluate` runs the detectors over a
//! repository's window and writes the reconciliation's batch, `heat.list` reads the proposals.
//!
//! Each identity has its own [`Heat`] handle over its own store and repositories, so an
//! evaluation asked for one identity reads and writes that identity's rows only. One evaluation
//! runs at a time per identity: two calls never interleave their reads and writes. Nothing
//! evaluates on its own yet: only `heat.evaluate` does.

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    HeatEvaluateParams, HeatEvaluation, HeatInsufficient, HeatListParams, HeatProposals,
    HeatRepoEvaluation, Method, ProposalView, method,
};
use bosun_config::Repo;
use bosun_core::proposal::is_active;
use bosun_store::{ProposalRow, Reader, Writer};
use serde_json::Value;

use crate::api::{call, failed};
use crate::overview::now_ms;
use crate::state::{Served, State};

/// The evaluations of one identity: its reader, its writer (the daemon is the store's single
/// writer) and its repositories, by name.
#[derive(Debug)]
pub(crate) struct Heat {
    reader: Reader,
    writer: Writer,
    repos: Vec<(String, Repo)>,
    /// Held for the whole of an evaluation.
    running: tokio::sync::Mutex<()>,
}

impl Heat {
    /// The evaluations of the identity whose store is `store` and repositories `repos`.
    pub(crate) fn of(store: &bosun_store::Store, repos: Vec<(String, Repo)>) -> Self {
        Self {
            reader: store.reader().clone(),
            writer: store.writer().clone(),
            repos,
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Evaluates the repository named `repo`, or each one, at `now_ms`, and writes each
    /// repository's batch in one transaction. A repository whose evaluation fails is logged and
    /// reported with its error; the others go on.
    ///
    /// # Errors
    ///
    /// `repo` names no repository of the identity.
    pub(crate) async fn evaluate(
        &self,
        identity: &str,
        repo: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<(String, HeatRepoEvaluation)>, String> {
        let repos: Vec<&(String, Repo)> = match repo {
            None => self.repos.iter().collect(),
            Some(name) => {
                vec![self.repos.iter().find(|(n, _)| n == name).ok_or_else(|| {
                    format!("identity {identity} has no repository named {name:?}")
                })?]
            }
        };
        let _running = self.running.lock().await;
        let mut out = Vec::new();
        for (name, config) in repos {
            let key = config.key.as_str();
            let outcome = match bosun_heat::evaluate(&self.reader, key, now_ms).await {
                Ok(evaluation) => {
                    let wrote = if evaluation.writes.is_empty() {
                        Ok(())
                    } else {
                        self.writer.write(evaluation.writes.clone()).await
                    };
                    wrote.map(|()| evaluation)
                }
                Err(e) => Err(e),
            };
            let summary = match outcome {
                Ok(evaluation) => summary(key, &evaluation),
                Err(e) => {
                    crate::log::log!("bosund: identity {identity}: heat of {name}: {e}");
                    HeatRepoEvaluation {
                        error: Some(e.to_string()),
                        ..summary(key, &bosun_heat::Evaluation::default())
                    }
                }
            };
            out.push((key.to_owned(), summary));
        }
        Ok(out)
    }
}

fn summary(key: &str, evaluation: &bosun_heat::Evaluation) -> HeatRepoEvaluation {
    let s = evaluation.summary;
    HeatRepoEvaluation {
        repo: bosun_api::RepoRef {
            name: None,
            path: key.to_owned(),
        },
        findings: s.findings,
        sightings: s.sightings,
        promoted: s.promoted,
        held_back: s.held_back,
        seen: s.seen,
        forgotten: s.forgotten,
        expired: s.expired,
        cooled_down: s.cooled_down,
        skipped: s.skipped,
        insufficient: evaluation
            .insufficient
            .iter()
            .map(|i| HeatInsufficient {
                detector: i.detector.to_owned(),
                provider: i.provider.clone(),
                closed_items: i.closed_items,
                run_sessions: i.run_sessions,
            })
            .collect(),
        error: None,
    }
}

/// `heat.metrics`, `heat.evaluate` and `heat.list`.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::HeatMetrics::NAME => crate::api::heat_metrics(state, p).await,
        method::HeatEvaluate::NAME => {
            call::<method::HeatEvaluate, _, _>(p, |p| async move { evaluate(state, &p).await })
                .await
        }
        method::HeatList::NAME => {
            call::<method::HeatList, _, _>(p, |p| async move {
                list(state.identity(&p.identity)?, &p).await
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

/// Evaluates now, with the real clock, the identity's repository `p.repo` or each of them.
async fn evaluate(state: &State, p: &HeatEvaluateParams) -> Result<HeatEvaluation, ErrorObject> {
    let served = state.identity(&p.identity)?;
    let now = now_ms();
    // Mutant (`mutant_heat_evaluate_other_identity`): every other identity of the daemon is
    // evaluated too, each into its own store.
    if cfg!(mutant_heat_evaluate_other_identity) {
        for other in state.identities.values() {
            if other.name != served.name {
                let _ = other.heat.evaluate(&other.name, None, now).await;
            }
        }
    }
    let repos = served
        .heat
        .evaluate(&served.name, p.repo.as_deref(), now)
        .await
        .map_err(|e| ErrorObject::new(code::INVALID_PARAMS, e))?
        .into_iter()
        .map(|(key, summary)| HeatRepoEvaluation {
            repo: served.repo(&key),
            ..summary
        })
        .collect();
    Ok(HeatEvaluation {
        identity: served.name.clone(),
        at_ms: now,
        repos,
    })
}

/// The identity's proposals: rows seen in two evaluations at least, active unless `p.all`.
/// Reads only.
async fn list(served: &Served, p: &HeatListParams) -> Result<HeatProposals, ErrorObject> {
    let repo = match &p.repo {
        None => None,
        Some(name) => Some(
            served
                .repos
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, repo)| repo.key.as_str())
                .ok_or_else(|| {
                    ErrorObject::new(
                        code::INVALID_PARAMS,
                        format!("identity {} has no repository named {name:?}", served.name),
                    )
                })?,
        ),
    };
    let rows = served
        .store
        .reader()
        .proposals(repo)
        .await
        .map_err(failed)?;
    let proposals = rows
        .into_iter()
        .filter(|row| row.seen_count >= 2 && (p.all || is_active(row.state)))
        .map(|row| view(served, row))
        .collect();
    Ok(HeatProposals {
        identity: served.name.clone(),
        proposals,
    })
}

fn view(served: &Served, row: ProposalRow) -> ProposalView {
    let evidence = serde_json::from_str(&row.evidence_json).unwrap_or_default();
    ProposalView {
        repo: served.repo(&row.repo),
        key: row.key,
        provider: row.provider,
        detector: row.detector,
        detector_version: row.detector_version,
        remedy: row.remedy,
        subject_kind: row.subject_kind.as_str().to_owned(),
        subject: row.subject,
        metric: row.metric,
        unit: row.unit,
        estimate: row.estimate,
        baseline_value: row.baseline_value,
        threshold: row.threshold,
        target_value: row.target_value,
        window_from_ms: row.window_from_ms,
        window_to_ms: row.window_to_ms,
        sample: row.sample,
        impact_eqv_estimate: row.impact_eqv_estimate,
        state: row.state.as_str().to_owned(),
        created_ms: row.created_ms,
        last_seen_ms: row.last_seen_ms,
        seen_count: row.seen_count,
        evidence,
    }
}
