//! The methods of `bosun-api` over the daemon's state, and the JSON-RPC handling of one line.

use std::collections::BTreeMap;

use bosun_api::rpc::{self, ErrorObject, Request, Response, code};
use bosun_api::{
    API_VERSION, CommitScan, DaemonStatus, Estimates, FailedFile, HelloResult, IngestStatus,
    LastPass, Method, ModelRow, PassReport, Picodollars, ReingestResult, RepoRef, RepoRow, Report,
    ReportParams, SessionRow, TokenSums, TokensByModel, TokensByRepo, TokensBySession,
    TokensParams, TokensSummary, UnpricedModel, Wide, method,
};
use bosun_core::{Cost, PriceTable};
use bosun_store::{Bill, By, Group};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::state::{Served, State};

/// Answers one line: `None` for a notification, which gets no response.
pub(crate) async fn handle_line(state: &State, line: &str) -> Option<Response> {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            return Some(Response::error(
                Value::Null,
                ErrorObject::new(code::PARSE_ERROR, format!("not JSON: {e}")),
            ));
        }
    };
    if value.is_array() {
        return Some(Response::error(
            Value::Null,
            ErrorObject::new(code::INVALID_REQUEST, "batches are not supported"),
        ));
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let request = match serde_json::from_value::<Request>(value) {
        Ok(request) if request.jsonrpc == rpc::JSONRPC => request,
        Ok(_) => {
            return Some(Response::error(
                id,
                ErrorObject::new(code::INVALID_REQUEST, "`jsonrpc` must be \"2.0\""),
            ));
        }
        Err(e) => {
            return Some(Response::error(
                id,
                ErrorObject::new(code::INVALID_REQUEST, format!("not a request: {e}")),
            ));
        }
    };
    let outcome = dispatch(state, &request.method, request.params).await;
    let id = request.id?;
    Some(match outcome {
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(id, error),
    })
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

fn result<T: Serialize>(value: &T) -> Result<Value, ErrorObject> {
    serde_json::to_value(value)
        .map_err(|e| ErrorObject::new(code::INTERNAL_ERROR, format!("result: {e}")))
}

fn failed(e: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new(code::FAILED, e.to_string())
}

/// Parses the params of `M` and runs `f` on them.
async fn call<M, F, Fut>(params_value: Value, f: F) -> Result<Value, ErrorObject>
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
    })
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
    let report = served.ingest.reingest().await.map_err(failed)?;
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
    fn repo(&self, key: &str) -> RepoRef {
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
        handle_line(&state, line)
            .await
            .map(|r| serde_json::to_value(r).unwrap())
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
