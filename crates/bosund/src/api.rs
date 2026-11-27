//! The methods of `bosun-api` over the daemon's state, and the JSON-RPC handling of one line.

use std::collections::BTreeMap;

use bosun_api::rpc::{self, ErrorObject, Request, Response, code};
use bosun_api::{
    AGENT_GROUP, API_VERSION, CommitScan, DaemonStatus, DriftedFile, Estimates, EventBody,
    FailedFile, HelloResult, IngestDrift, IngestStatus, LastPass, Method, ModelRow, PassReport,
    Picodollars, ReingestResult, RepoRef, RepoRow, Report, ReportParams, Resources, SessionRow,
    TokenSums, TokensByModel, TokensByRepo, TokensBySession, TokensParams, TokensSummary,
    UnpricedModel, Wide, WorktreeView, method,
};
use bosun_core::{Cost, PriceTable};
use bosun_store::{Bill, By, Group};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agent;
use crate::events::Subscriber;
use crate::state::{Served, State};

/// The answer to one line.
#[derive(Debug)]
pub(crate) enum Answer {
    /// A response; `None` for a notification, which gets none.
    Reply(Option<Response>),
    /// The response to `events.subscribe`: the connection carries the subscriber's events
    /// from now on.
    Subscribe(Response, Subscriber),
}

/// What one connection has shown of itself.
#[derive(Debug, Default)]
pub(crate) struct Connection {
    /// It called `agent.*`: an agent session's server, which may call nothing else (#63).
    agent: bool,
}

/// Answers one line of `conn`.
pub(crate) async fn handle_line(state: &State, conn: &mut Connection, line: &str) -> Answer {
    let request = match request(line) {
        Ok(request) => request,
        Err(response) => return Answer::Reply(Some(*response)),
    };
    let outcome = if request.method.starts_with(AGENT_GROUP) {
        // `agent.*` admits only a live credential naming its item (#63).
        conn.agent = true;
        agent::dispatch(&state.credentials, &request.method, request.params).await
    } else if conn.agent || request.params.get("credential").is_some() {
        // An agent credential admits `agent.*` only, on its connection too.
        Err(ErrorObject::new(
            code::UNAUTHORIZED,
            format!(
                "an agent credential admits agent.* only, not {}",
                request.method
            ),
        ))
    } else if request.method == method::EventsSubscribe::NAME {
        return subscribe(state, request);
    } else {
        dispatch(state, &request.method, request.params).await
    };
    let Some(id) = request.id else {
        return Answer::Reply(None);
    };
    Answer::Reply(Some(match outcome {
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(id, error),
    }))
}

/// Parses one line into a request, or the error response it gets.
fn request(line: &str) -> Result<Request, Box<Response>> {
    let refuse =
        |id, code, message: String| Box::new(Response::error(id, ErrorObject::new(code, message)));
    let value: Value = serde_json::from_str(line)
        .map_err(|e| refuse(Value::Null, code::PARSE_ERROR, format!("not JSON: {e}")))?;
    if value.is_array() {
        return Err(refuse(
            Value::Null,
            code::INVALID_REQUEST,
            "batches are not supported".to_owned(),
        ));
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    match serde_json::from_value::<Request>(value) {
        Ok(request) if request.jsonrpc == rpc::JSONRPC => Ok(request),
        Ok(_) => Err(refuse(
            id,
            code::INVALID_REQUEST,
            "`jsonrpc` must be \"2.0\"".to_owned(),
        )),
        Err(e) => Err(refuse(
            id,
            code::INVALID_REQUEST,
            format!("not a request: {e}"),
        )),
    }
}

/// Subscribes the connection to the identity's events. A subscription asked for in a
/// notification is not made: it could not say where the stream starts.
fn subscribe(state: &State, request: Request) -> Answer {
    let Some(id) = request.id else {
        return Answer::Reply(None);
    };
    let made = params::<method::EventsSubscribe>(request.params).and_then(|p| {
        let served = state.identity(&p.identity)?;
        let (answer, subscriber) =
            served
                .events
                .subscribe(&served.name, p.epoch.as_deref(), p.from_seq, p.filter);
        Ok((result(&answer)?, subscriber))
    });
    match made {
        Ok((answer, subscriber)) => Answer::Subscribe(Response::result(id, answer), subscriber),
        Err(error) => Answer::Reply(Some(Response::error(id, error))),
    }
}

/// The event of a finished ingest pass.
pub(crate) fn pass_event(
    reingest: bool,
    outcome: &Result<bosun_ingest::Report, String>,
) -> EventBody {
    match outcome {
        Ok(report) => EventBody::IngestPass {
            reingest,
            pass: pass_report(report),
        },
        Err(error) => EventBody::IngestFailed {
            reingest,
            error: error.clone(),
        },
    }
}

fn params<M: Method>(params: Value) -> Result<M::Params, ErrorObject> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params)
        .map_err(|e| ErrorObject::new(code::INVALID_PARAMS, format!("params of {}: {e}", M::NAME)))
}

pub(crate) fn result<T: Serialize>(value: &T) -> Result<Value, ErrorObject> {
    serde_json::to_value(value)
        .map_err(|e| ErrorObject::new(code::INTERNAL_ERROR, format!("result: {e}")))
}

pub(crate) fn failed(e: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new(code::FAILED, e.to_string())
}

/// Parses the params of `M` and runs `f` on them.
pub(crate) async fn call<M, F, Fut>(params_value: Value, f: F) -> Result<Value, ErrorObject>
where
    M: Method,
    F: FnOnce(M::Params) -> Fut,
    Fut: Future<Output = Result<M::Result, ErrorObject>>,
    M::Params: DeserializeOwned,
{
    let parsed = params::<M>(params_value)?;
    result(&f(parsed).await?)
}

async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::Hello::NAME => call::<method::Hello, _, _>(p, |_| async { Ok(hello()) }).await,
        method::StatusDaemon::NAME => {
            call::<method::StatusDaemon, _, _>(p, |p| async move {
                let served = state.identity(&p.identity)?;
                Ok(DaemonStatus {
                    identity: served.name.clone(),
                    api_version: API_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    pid: std::process::id(),
                    started_ms: state.started_ms,
                })
            })
            .await
        }
        method::IngestStatus::NAME => {
            call::<method::IngestStatus, _, _>(p, |p| async move {
                ingest_status(state.identity(&p.identity)?).await
            })
            .await
        }
        method::IngestReingest::NAME => {
            call::<method::IngestReingest, _, _>(p, |p| async move {
                reingest(state.identity(&p.identity)?).await
            })
            .await
        }
        method::TokensBySession::NAME => {
            call::<method::TokensBySession, _, _>(p, |p| async move {
                let (summary, bills) = tokens(state, &p, By::Session).await?;
                let rows = bills
                    .into_iter()
                    .filter_map(|(group, bill)| match group {
                        Group::Session { session, agent } => Some(SessionRow {
                            session,
                            agent,
                            sums: sums(&bill),
                        }),
                        _ => None,
                    })
                    .collect();
                Ok(TokensBySession { summary, rows })
            })
            .await
        }
        method::TokensByRepo::NAME => {
            call::<method::TokensByRepo, _, _>(
                p,
                |p| async move { tokens_by_repo(state, &p).await },
            )
            .await
        }
        method::TokensByModel::NAME => {
            call::<method::TokensByModel, _, _>(p, |p| async move {
                let (summary, bills) = tokens(state, &p, By::Model).await?;
                let rows = bills
                    .into_iter()
                    .filter_map(|(group, bill)| match group {
                        Group::Model(model) => Some(ModelRow {
                            model,
                            sums: sums(&bill),
                        }),
                        _ => None,
                    })
                    .collect();
                Ok(TokensByModel { summary, rows })
            })
            .await
        }
        method::ReportExact::NAME => {
            call::<method::ReportExact, _, _>(p, |p| async move {
                report(state.identity(&p.identity)?, &p).await
            })
            .await
        }
        method::ReportEstimates::NAME => {
            call::<method::ReportEstimates, _, _>(p, |p| async move {
                estimates(state.identity(&p.identity)?, &p).await
            })
            .await
        }
        method::TokensPace::NAME => crate::pace::dispatch(state, name, p).await,
        method::LeaseList::NAME => lease_list(state, p).await,
        name if name.starts_with("control.") => crate::control::dispatch(state, name, p).await,
        name if name.starts_with("doctor.") => crate::doctor::dispatch(state, name, p).await,
        name if name.starts_with("lease.") => crate::leases::dispatch(state, name, p).await,
        name if name.starts_with("notify.") => crate::notify::dispatch(state, name, p).await,
        name if name.starts_with("status.") => crate::overview::dispatch(state, name, p).await,
        name if name.starts_with("queue.") => crate::board::dispatch(state, name, p).await,
        name if name.starts_with("item.") || name.starts_with("session.") => {
            crate::item::dispatch(state, name, p).await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

async fn tokens_by_repo(state: &State, p: &TokensParams) -> Result<TokensByRepo, ErrorObject> {
    let served = state.identity(&p.identity)?;
    let (summary, bills) = tokens(state, p, By::Repo).await?;
    let mut rows: Vec<RepoRow> = bills
        .into_iter()
        .filter_map(|(group, bill)| match group {
            Group::Repo(key) => Some(RepoRow {
                repo: key.map(|key| served.repo(&key)),
                sums: sums(&bill),
            }),
            _ => None,
        })
        .collect();
    // By path, with messages in no repository last.
    rows.sort_by(|a, b| match (&a.repo, &b.repo) {
        (Some(a), Some(b)) => a.path.cmp(&b.path),
        (a, b) => a.is_none().cmp(&b.is_none()),
    });
    Ok(TokensByRepo { summary, rows })
}

/// `lease.list` (#8): the resource instances the identity can lease as it may see them, and
/// its worktrees on disk or quarantined, from its own store. Reads only.
async fn lease_list(state: &State, p: Value) -> Result<Value, ErrorObject> {
    call::<method::LeaseList, _, _>(p, |p| async move {
        resources(state, state.identity(&p.identity)?).await
    })
    .await
}

async fn resources(state: &State, served: &Served) -> Result<Resources, ErrorObject> {
    let repos: Vec<String> = served.repos.iter().map(|(n, _)| n.clone()).collect();
    let seen = state.leases.seen_by(&served.name, &repos);
    let reader = served.store.reader();
    let mut worktrees = reader.open_worktrees().await.map_err(failed)?;
    worktrees.extend(reader.quarantined_worktrees().await.map_err(failed)?);
    worktrees.sort_by_key(|w| w.created_ms);
    Ok(Resources {
        identity: served.name.clone(),
        resources: seen.resources,
        worktrees: worktrees
            .into_iter()
            .map(|w| WorktreeView {
                item: w.item.0,
                path: w.path,
                kind: w.kind.as_str().to_owned(),
                state: w.state.as_str().to_owned(),
                bytes: w.bytes,
                blocking: w.blocking,
            })
            .collect(),
    })
}

pub(crate) fn hello() -> HelloResult {
    HelloResult {
        api_version: API_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
    }
}

async fn ingest_status(served: &Served) -> Result<IngestStatus, ErrorObject> {
    let files = served
        .store
        .reader()
        .source_files(bosun_ingest::claude::PROVIDER)
        .await
        .map_err(failed)?;
    Ok(IngestStatus {
        identity: served.name.clone(),
        database: served.database.display().to_string(),
        transcripts: served.transcripts.display().to_string(),
        files: files.len() as u64,
        size: Wide(files.iter().map(|f| f.size).sum()),
        parsed: Wide(files.iter().map(|f| f.parsed_offset).sum()),
        parsed_since_start: Wide(served.ingest.parsed_bytes()),
        last_pass: match served.ingest.last_pass() {
            None => LastPass::Pending,
            Some(Ok(report)) => LastPass::Done(pass_report(&report)),
            Some(Err(error)) => LastPass::Failed { error },
        },
        drift: if cfg!(mutant_api_ingest_drift_unsummed) {
            // Mutant (`mutant_api_ingest_drift_unsummed`): the drift counts are not summed.
            IngestDrift::default()
        } else {
            ingest_drift(&files)
        },
    })
}

/// The drift counts of `files`, summed over those whose lines and drift are all counted.
fn ingest_drift(files: &[bosun_store::SourceFile]) -> IngestDrift {
    let mut drift = IngestDrift::default();
    let mut drifted = Vec::new();
    for file in files {
        let (Some(lines), Some(unknown), Some(usage), Some(unbilled)) = (
            file.parsed_lines,
            file.unknown_lines,
            file.usage_lines,
            file.unbilled_lines,
        ) else {
            drift.uncounted_files += 1;
            continue;
        };
        drift.counted_files += 1;
        drift.lines += lines;
        drift.unknown += unknown;
        drift.usage += usage;
        drift.unbilled += unbilled;
        if unknown > 0 || unbilled > 0 {
            drifted.push(DriftedFile {
                path: file.path.clone(),
                unknown,
                usage,
                unbilled,
            });
        }
    }
    drifted.sort_by(|a, b| {
        (b.unbilled, b.unknown)
            .cmp(&(a.unbilled, a.unknown))
            .then_with(|| a.path.cmp(&b.path))
    });
    drift.more_files = drifted.len().saturating_sub(IngestDrift::MAX_FILES) as u64;
    drifted.truncate(IngestDrift::MAX_FILES);
    drift.files = drifted;
    drift
}

fn pass_report(report: &bosun_ingest::Report) -> PassReport {
    PassReport {
        files: report.files,
        parsed_bytes: Wide(report.parsed_bytes),
        restarted: report.restarted,
        failed: report
            .failed
            .iter()
            .map(|(path, error)| FailedFile {
                path: path.display().to_string(),
                error: error.clone(),
            })
            .collect(),
    }
}

/// Re-reads every transcript, then the new commits, so that a report after a reingest sees
/// both.
async fn reingest(served: &Served) -> Result<ReingestResult, ErrorObject> {
    let outcome = served.ingest.reingest().await.map_err(|e| e.to_string());
    served.events.publish(pass_event(true, &outcome));
    let report = outcome.map_err(failed)?;
    served.commits.refresh().await;
    Ok(ReingestResult {
        identity: served.name.clone(),
        pass: pass_report(&report),
    })
}

/// A configured repository: its name and configuration.
type NamedRepo = (String, bosun_config::Repo);

/// The configured repositories a report covers (the one `p` names, else every one) and its
/// scope.
fn report_scope<'a>(
    served: &'a Served,
    p: &ReportParams,
) -> Result<(Vec<&'a NamedRepo>, bosun_heat::Scope<'a>), ErrorObject> {
    if p.from_ms >= p.to_ms {
        return Err(ErrorObject::new(
            code::INVALID_PARAMS,
            "from_ms must be before to_ms: the range is empty",
        ));
    }
    let repos: Vec<&NamedRepo> = match &p.repo {
        None => served.repos.iter().collect(),
        Some(name) => {
            let found = served
                .repos
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| {
                    ErrorObject::new(
                        code::INVALID_PARAMS,
                        format!("identity {} has no repository named {name:?}", served.name),
                    )
                })?;
            vec![found]
        }
    };
    let scope = bosun_heat::Scope {
        from_ms: p.from_ms,
        to_ms: p.to_ms,
        repo: p.repo.as_ref().map(|_| repos[0].1.key.as_str()),
    };
    Ok((repos, scope))
}

/// The exact sections of the report of one identity. Reads only: the commits are those the
/// commit reader has stored.
async fn report(served: &Served, p: &ReportParams) -> Result<Report, ErrorObject> {
    let (repos, scope) = report_scope(served, p)?;
    let scans = repos
        .iter()
        .map(|(name, repo)| CommitScan {
            repo: served.repo(&repo.key),
            state: served.commits.state(name),
        })
        .collect();
    bosun_heat::exact(
        served.store.reader(),
        &served.name,
        scope,
        |key| served.repo(key),
        scans,
    )
    .await
    .map_err(failed)
}

/// The estimate sections of the report of one identity. Reads only.
async fn estimates(served: &Served, p: &ReportParams) -> Result<Estimates, ErrorObject> {
    let (_, scope) = report_scope(served, p)?;
    bosun_heat::estimates(served.store.reader(), &served.name, scope, |key| {
        served.repo(key)
    })
    .await
    .map_err(failed)
}

/// The bills of one identity in the range, and their summary.
async fn tokens(
    state: &State,
    p: &TokensParams,
    by: By,
) -> Result<(TokensSummary, BTreeMap<Group, Bill>), ErrorObject> {
    let served = state.identity(&p.identity)?;
    let (table, bills) = served
        .store
        .reader()
        .bills(
            p.from_ms.unwrap_or(i64::MIN),
            p.to_ms.unwrap_or(i64::MAX),
            by,
        )
        .await
        .map_err(failed)?;
    Ok((summary(served, p, &table, &bills), bills))
}

fn summary(
    served: &Served,
    p: &TokensParams,
    table: &PriceTable,
    bills: &BTreeMap<Group, Bill>,
) -> TokensSummary {
    let mut total = TokenSums::default();
    let mut cost = Cost::ZERO;
    let mut unpriced: BTreeMap<&str, u64> = BTreeMap::new();
    for bill in bills.values() {
        total.messages += bill.messages;
        total.input += bill.tokens.input;
        total.cache_write_5m += bill.tokens.cache_write_5m;
        total.cache_write_1h += bill.tokens.cache_write_1h;
        total.cache_read += bill.tokens.cache_read;
        total.output += bill.tokens.output;
        cost = cost + bill.cost;
        for (model, n) in &bill.unpriced {
            *unpriced.entry(model).or_insert(0) += n;
            total.unpriced_messages += n;
        }
    }
    total.cost = Picodollars(cost.picodollars);
    total.eqv = table.eqv(cost).tokens();
    TokensSummary {
        identity: served.name.clone(),
        from_ms: p.from_ms,
        to_ms: p.to_ms,
        reference_model: table.reference_model().to_owned(),
        total,
        unpriced: unpriced
            .into_iter()
            .map(|(model, messages)| UnpricedModel {
                model: model.to_owned(),
                messages,
            })
            .collect(),
    }
}

fn sums(bill: &Bill) -> TokenSums {
    TokenSums {
        messages: bill.messages,
        input: bill.tokens.input,
        cache_write_5m: bill.tokens.cache_write_5m,
        cache_write_1h: bill.tokens.cache_write_1h,
        cache_read: bill.tokens.cache_read,
        output: bill.tokens.output,
        cost: Picodollars(bill.cost.picodollars),
        eqv: bill.eqv.tokens(),
        unpriced_messages: bill.unpriced.values().sum(),
    }
}

impl Served {
    /// The configured repository with store key `key`, or the key itself when the
    /// configuration no longer names it.
    pub(crate) fn repo(&self, key: &str) -> RepoRef {
        self.repos
            .iter()
            .find(|(_, repo)| repo.key == key)
            .map_or_else(
                || RepoRef {
                    name: None,
                    path: key.to_owned(),
                },
                |(name, repo)| RepoRef {
                    name: Some(name.clone()),
                    path: repo.path.display().to_string(),
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn answer(line: &str) -> Option<Value> {
        let state = State::empty();
        match handle_line(&state, &mut Connection::default(), line).await {
            Answer::Reply(response) => response.map(|r| serde_json::to_value(r).unwrap()),
            Answer::Subscribe(..) => panic!("subscribed with no identity"),
        }
    }

    #[tokio::test]
    async fn hello_answers_the_api_version() {
        let response = answer(r#"{"jsonrpc":"2.0","id":7,"method":"hello"}"#)
            .await
            .unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["api_version"], API_VERSION);
    }

    #[tokio::test]
    async fn errors_have_their_codes() {
        let cases = [
            ("not json", code::PARSE_ERROR),
            ("[]", code::INVALID_REQUEST),
            (
                r#"{"jsonrpc":"1.0","id":1,"method":"hello"}"#,
                code::INVALID_REQUEST,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#,
                code::METHOD_NOT_FOUND,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"tokens.by_model","params":{}}"#,
                code::INVALID_PARAMS,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"ingest.status","params":{"identity":"x"}}"#,
                code::UNKNOWN_IDENTITY,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe","params":{"identity":"x"}}"#,
                code::UNKNOWN_IDENTITY,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe","params":{}}"#,
                code::INVALID_PARAMS,
            ),
        ];
        for (line, expected) in cases {
            let response = answer(line).await.unwrap();
            assert_eq!(response["error"]["code"], expected, "{line}: {response}");
        }
    }

    #[tokio::test]
    async fn a_notification_gets_no_response() {
        assert_eq!(answer(r#"{"jsonrpc":"2.0","method":"hello"}"#).await, None);
    }
}
