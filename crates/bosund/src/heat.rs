//! Heat's proposals in the daemon (#11, part D): `heat.evaluate` runs the detectors over a
//! repository's window and writes the reconciliation's batch (the landings the repository's done
//! items made first, [`bosun_heat::link`]), `heat.list` reads the proposals, `heat.dismiss`
//! dismisses one and `heat.land` records by hand that one's remedy landed (both with the
//! operator token). An evaluation also measures each landed proposal whose follow-up is due
//! ([`bosun_heat::measure`]); `heat.list` shows the before, the after and the outcome, and the
//! standing of each detector ([`bosun_heat::standings`]).
//!
//! Each identity has its own [`Heat`] handle over its own store and repositories, so an
//! evaluation asked for one identity reads and writes that identity's rows only. One evaluation
//! runs at a time per identity: two calls never interleave their reads and writes.
//!
//! Besides `heat.evaluate`, a daily timer per identity ([`Heat::spawn`]) evaluates each of its
//! repositories through the same handle and mutex: the first time [`FIRST_DELAY_MS`] after the
//! daemon's start (a daemon restarted often must not evaluate at every start), then every
//! [`DAY_MS`] ([`due`]). The time of the last evaluation is not stored: a restarted daemon waits
//! its first delay again.

use std::sync::Arc;
use std::time::Duration;

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{
    DetectorStanding, HeatDismissParams, HeatEvaluateParams, HeatEvaluation, HeatInsufficient,
    HeatLandParams, HeatListParams, HeatProposals, HeatRepoEvaluation, Method, ProposalView,
    method,
};
use bosun_config::Repo;
use bosun_core::proposal::{LandedHow, ProposalState, is_active};
use bosun_store::{ProposalRow, ProposalStep, Reader, Write, Writer};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::api::{call, failed};
use crate::overview::now_ms;
use crate::state::{Served, State};

/// The wait after the daemon's start before the timer's first evaluation.
pub(crate) const FIRST_DELAY_MS: i64 = 60 * 60 * 1000;
/// The time between two evaluations of the timer.
pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How often the timer asks [`due`].
const TICK: Duration = Duration::from_secs(600);

/// Whether the timer evaluates at `now_ms`, for a daemon started at `started_ms` whose timer
/// last evaluated at `last_ms` (`None` before its first evaluation in this process). Never when
/// the clock went back behind the last evaluation or the start.
pub(crate) fn due(started_ms: i64, last_ms: Option<i64>, now_ms: i64) -> bool {
    match last_ms {
        Some(last) => now_ms >= last && now_ms - last >= DAY_MS,
        // Mutant (`mutant_heat_timer_at_start`): the first evaluation runs at the start.
        None if cfg!(mutant_heat_timer_at_start) => now_ms >= started_ms,
        None => now_ms >= started_ms && now_ms - started_ms >= FIRST_DELAY_MS,
    }
}

/// The evaluations of one identity: its reader, its writer (the daemon is the store's single
/// writer) and its repositories, by name. Clones share one mutex.
#[derive(Debug, Clone)]
pub(crate) struct Heat {
    reader: Reader,
    writer: Writer,
    repos: Arc<Vec<(String, Repo)>>,
    /// Held for the whole of an evaluation, the timer's and `heat.evaluate`'s alike.
    running: Arc<tokio::sync::Mutex<()>>,
}

impl Heat {
    /// The evaluations of the identity whose store is `store` and repositories `repos`.
    pub(crate) fn of(store: &bosun_store::Store, repos: Vec<(String, Repo)>) -> Self {
        Self {
            reader: store.reader().clone(),
            writer: store.writer().clone(),
            repos: Arc::new(repos),
            running: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The daily timer of the identity `identity`, for a daemon started at `started_ms`: every
    /// [`TICK`] it asks [`due`] and, when due, evaluates every repository through
    /// [`Heat::evaluate`] (which logs a repository that fails and goes on with the others),
    /// until the task is aborted.
    pub(crate) fn spawn(&self, identity: String, started_ms: i64) -> JoinHandle<()> {
        let heat = self.clone();
        tokio::spawn(async move {
            let mut last_ms = None;
            loop {
                tokio::time::sleep(TICK).await;
                let now = now_ms();
                if !due(started_ms, last_ms, now) {
                    continue;
                }
                if let Err(e) = heat.evaluate(&identity, None, now).await {
                    crate::log::log!("bosund: identity {identity}: heat: {e}");
                }
                last_ms = Some(now);
            }
        })
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
        landed: s.landed,
        measured: s.measured,
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

/// `heat.metrics`, `heat.evaluate`, `heat.list`, `heat.dismiss` and `heat.land`.
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
        method::HeatDismiss::NAME => {
            call::<method::HeatDismiss, _, _>(p, |p| async move {
                // Mutant (`mutant_heat_dismiss_without_token`): any caller may dismiss.
                if !cfg!(mutant_heat_dismiss_without_token) {
                    state.control.authorize(&p.operator)?;
                }
                dismiss(state.identity(&p.identity)?, &p).await
            })
            .await
        }
        method::HeatLand::NAME => {
            call::<method::HeatLand, _, _>(p, |p| async move {
                state.control.authorize(&p.operator)?;
                land(state.identity(&p.identity)?, &p).await
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
    let reader = served.store.reader();
    let every = reader.proposals(None).await.map_err(failed)?;
    let detectors = bosun_heat::standings(&every)
        .into_iter()
        .map(|s| DetectorStanding {
            detector: s.detector,
            measured: s.measured,
            improved: s.improved,
            dismissed: s.dismissed,
            flagged: s.flagged.map(str::to_owned),
        })
        .collect();
    let rows = match repo {
        None => every,
        Some(_) => reader.proposals(repo).await.map_err(failed)?,
    };
    let proposals = rows
        .into_iter()
        .filter(|row| row.seen_count >= 2 && (p.all || is_active(row.state)))
        .map(|row| view(served, row))
        .collect();
    Ok(HeatProposals {
        identity: served.name.clone(),
        proposals,
        detectors,
    })
}

/// Dismisses the identity's proposal `p.key` when it is open or published. A key of no row of
/// the identity's store and a sighting (seen once, not a proposal yet) are refused alike.
async fn dismiss(served: &Served, p: &HeatDismissParams) -> Result<ProposalView, ErrorObject> {
    let reader = served.store.reader();
    let row = reader
        .proposal(&p.key)
        .await
        .map_err(failed)?
        .filter(|row| row.seen_count >= 2)
        .ok_or_else(|| {
            ErrorObject::new(
                code::INVALID_PARAMS,
                format!("identity {} has no proposal {:?}", served.name, p.key),
            )
        })?;
    if !matches!(row.state, ProposalState::Open | ProposalState::Published) {
        return Err(ErrorObject::new(
            code::INVALID_PARAMS,
            format!(
                "proposal {} is {}: only an open or published proposal can be dismissed",
                row.key,
                row.state.as_str()
            ),
        ));
    }
    served
        .store
        .writer()
        .write(vec![Write::ProposalStep {
            key: row.key.clone(),
            step: ProposalStep::Dismissed {
                reason: p.reason.clone().unwrap_or_default(),
            },
            at_ms: now_ms(),
        }])
        .await
        .map_err(|e| ErrorObject::new(code::INVALID_PARAMS, e.to_string()))?;
    let row = reader
        .proposal(&row.key)
        .await
        .map_err(failed)?
        .ok_or_else(|| ErrorObject::new(code::INTERNAL_ERROR, "the dismissed proposal vanished"))?;
    Ok(view(served, row))
}

/// Whether `sha` is a commit id: 7 to 40 hex digits.
fn is_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Records that the remedy of the identity's proposal `p.key` landed now, by hand, when it is
/// open or published: its `Landed` step and its repository's annotation, in one batch. A key
/// of no row of the identity's store and a sighting are refused alike.
async fn land(served: &Served, p: &HeatLandParams) -> Result<ProposalView, ErrorObject> {
    if let Some(sha) = p.sha.as_deref().filter(|sha| !is_sha(sha)) {
        return Err(ErrorObject::new(
            code::INVALID_PARAMS,
            format!("{sha:?} is not a commit id: 7 to 40 hex digits"),
        ));
    }
    let reader = served.store.reader();
    let row = reader
        .proposal(&p.key)
        .await
        .map_err(failed)?
        .filter(|row| row.seen_count >= 2)
        .ok_or_else(|| {
            ErrorObject::new(
                code::INVALID_PARAMS,
                format!("identity {} has no proposal {:?}", served.name, p.key),
            )
        })?;
    if !matches!(row.state, ProposalState::Open | ProposalState::Published) {
        return Err(ErrorObject::new(
            code::INVALID_PARAMS,
            format!(
                "proposal {} is {}: only an open or published proposal can land",
                row.key,
                row.state.as_str()
            ),
        ));
    }
    let writes = bosun_heat::landed(
        &row,
        None,
        p.sha.as_deref().map(str::to_ascii_lowercase),
        LandedHow::Manual,
        now_ms(),
    );
    served
        .store
        .writer()
        .write(writes)
        .await
        .map_err(|e| ErrorObject::new(code::INVALID_PARAMS, e.to_string()))?;
    let row = reader
        .proposal(&row.key)
        .await
        .map_err(failed)?
        .ok_or_else(|| ErrorObject::new(code::INTERNAL_ERROR, "the landed proposal vanished"))?;
    Ok(view(served, row))
}

/// The confounder a follow-up recorded in `params`, as one line.
fn confounder(params: &Value) -> Option<String> {
    let c = params.get("confounder")?;
    let models = |name: &str| -> String {
        c.get(name)
            .and_then(Value::as_array)
            .map_or_else(String::new, |models| {
                models
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
    };
    Some(format!(
        "models before: {}; after: {}",
        models("models_before"),
        models("models_after")
    ))
}

fn view(served: &Served, row: ProposalRow) -> ProposalView {
    let evidence = serde_json::from_str(&row.evidence_json).unwrap_or_default();
    let params: Value = serde_json::from_str(&row.params_json).unwrap_or(Value::Null);
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
        dismiss_reason: row.dismiss_reason.filter(|r| !r.is_empty()),
        landed_how: row.landed_how.map(|how| how.as_str().to_owned()),
        landed_sha: row.landed_sha,
        landed_ms: row.landed_ms,
        followup_due_ms: row.followup_due_ms,
        item: row.item.map(|item| item.0),
        after_value: row.after_value,
        after_scaled: params.get("after_scaled").and_then(Value::as_i64),
        after_sample: row.after_sample,
        outcome: row.outcome.map(|o| o.as_str().to_owned()),
        measured_ms: row.measured_ms,
        guard_metric: row.guard_metric,
        guard_baseline: row.guard_baseline,
        guard_after: row.guard_after,
        drift: params.get("drift").and_then(Value::as_i64),
        confounder: confounder(&params),
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::{DAY_MS, due};

    const MINUTE: i64 = 60 * 1000;

    /// The timer's first evaluation waits an hour after the start; after it, one a day; never
    /// when the clock went back (`mutant_heat_timer_at_start` evaluates at the start).
    #[test]
    fn the_first_evaluation_waits_an_hour_and_then_runs_daily() {
        assert!(!due(0, None, 0));
        assert!(!due(0, None, 59 * MINUTE));
        assert!(due(0, None, 60 * MINUTE));
        assert!(!due(10 * DAY_MS, None, 10 * DAY_MS - 1), "clock back");

        let t = 5 * DAY_MS + 17;
        for started in [0, t - MINUTE, t + 3 * DAY_MS] {
            assert!(!due(started, Some(t), t + DAY_MS - 1), "{started}");
            assert!(due(started, Some(t), t + DAY_MS), "{started}");
            assert!(!due(started, Some(t), t - 1), "{started}: clock back");
            assert!(!due(started, Some(t), t), "{started}");
        }
    }
}
