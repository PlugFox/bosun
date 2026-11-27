//! The generic metrics of heat (#11, part D): figures per repository, provider and session kind
//! that a detector can test, computed at query time from the store's rows (#3). Pure: no I/O
//! but [`heat_metrics`], which reads the rows and calls [`metrics`]. See `bosun_api::HeatMetrics`
//! for what each metric means.
//!
//! - Every group key holds the provider: two providers' rows are never folded into one figure,
//!   and only some figures compare across providers ([`Metric::runner_comparable`]).
//! - Result bytes, counts, durations and usage tokens are exact; tokens from bytes are an
//!   estimate ([`Exactness::Estimate`], with [`BYTES_PER_TOKEN`]) and never added to an exact
//!   figure.
//! - Only messages in a repository count. A root session (a provider's session id with its
//!   sub-agents) belongs to every repository it has a message in, and its figures there count
//!   that repository's messages and calls.
//!
//! `eqv_per_commit` is deliberately not the report's eqv per commit: the report folds every
//! message and every commit of a repository whatever the provider and the session kind, while
//! this one is the eqv of one provider's `run` sessions over the commits their commit ranges
//! list. The two are not meant to agree.

use std::collections::{BTreeMap, BTreeSet};

use bosun_api::{HeatMetric, HeatMetrics, RepoRef};
use bosun_core::{Cost, Outcome, PriceTable, State, TouchKind};
use bosun_store::{ItemHistory, Reader, RunCommitRow, ToolRow, UsageRow};

use crate::estimate::BYTES_PER_TOKEN;
use crate::exact::{Scope, rates};

/// Whether a figure is measured or guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exactness {
    /// Counts, bytes, durations and usage tokens as stored.
    Exact,
    /// Tokens guessed from bytes with [`BYTES_PER_TOKEN`].
    Estimate,
}

/// One metric of one group: a repository, a provider and, for the session metrics, a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metric {
    pub id: &'static str,
    /// The repository's path key.
    pub repo: String,
    pub provider: String,
    /// `run` or `interactive` for the session metrics; `run` for the item and commit metrics.
    pub kind: Option<&'static str>,
    /// The normalized figure in `unit`, rounded down; a share is in parts per million.
    pub value: u64,
    /// The numerator, in its base unit.
    pub total: u64,
    /// The denominator, in `sample_unit`; never zero (a group without one has no row).
    pub sample: u64,
    pub unit: &'static str,
    pub sample_unit: &'static str,
    pub exactness: Exactness,
    /// The ratio of an estimate; `None` for an exact figure.
    pub bytes_per_token: Option<u64>,
    /// Whether the figure can be compared between providers.
    pub runner_comparable: bool,
}

/// The rows the metrics are computed from, as the store reads them over a range.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub table: &'a PriceTable,
    pub usage: &'a [UsageRow],
    pub calls: &'a [ToolRow],
    pub histories: &'a [ItemHistory],
    pub run_commits: &'a [RunCommitRow],
}

/// A tool call with the message that made it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Joined<'a> {
    pub(crate) call: &'a ToolRow,
    pub(crate) message: &'a UsageRow,
}

impl<'a> Joined<'a> {
    fn touches(&self, kind: TouchKind) -> bool {
        self.call.touches.iter().any(|(_, k)| *k == kind)
    }

    /// A read or a search that writes nothing (as the estimates' cost of finding counts it).
    fn finds(&self) -> bool {
        !self.touches(TouchKind::Write)
            && (self.touches(TouchKind::Read) || self.touches(TouchKind::Search))
    }

    /// The distinct paths this call read.
    fn reads(&self) -> BTreeSet<&'a str> {
        self.call
            .touches
            .iter()
            .filter(|(_, kind)| *kind == TouchKind::Read)
            .map(|(path, _)| path.as_str())
            .collect()
    }
}

/// Each call of `inputs` with its message, in the order of the calls; a call whose message is
/// not in the rows is dropped, as the estimates drop it.
pub(crate) fn join<'a>(inputs: &Inputs<'a>) -> Vec<Joined<'a>> {
    let messages: BTreeMap<(&str, &str), &UsageRow> = inputs
        .usage
        .iter()
        .map(|row| ((row.provider.as_str(), row.message.as_str()), row))
        .collect();
    inputs
        .calls
        .iter()
        .filter_map(|call| {
            let message = messages.get(&(call.provider.as_str(), call.message.as_str()))?;
            Some(Joined { call, message })
        })
        .collect()
}

/// An item whose verdict passed in the range.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)] // `closed_ms` is the name the detectors (#225) use.
pub(crate) struct Closed<'a> {
    pub(crate) history: &'a ItemHistory,
    /// The repository's path key.
    pub(crate) repo: &'a str,
    /// The provider of its last session.
    pub(crate) provider: &'a str,
    /// `Pass` or `Flaky`.
    pub(crate) outcome: Outcome,
    /// Its last arrow to `done`.
    #[allow(dead_code)] // For the detectors (#225); the metrics need only the range.
    pub(crate) closed_ms: i64,
}

/// The items of `inputs` closed in `scope`: state `Done(Pass)` or `Done(Flaky)` (a card the
/// user closed has no verdict), its last arrow to `done` in the range, in the scope's
/// repository, with a session (an item without one is skipped).
pub(crate) fn closed_items<'a>(inputs: &Inputs<'a>, scope: Scope<'_>) -> Vec<Closed<'a>> {
    inputs
        .histories
        .iter()
        .filter_map(|history| {
            let State::Done(outcome @ (Outcome::Pass | Outcome::Flaky)) = history.item.item.state
            else {
                return None;
            };
            let repo = history.item.repo.as_str();
            if !scope.holds_repo(Some(repo)) {
                return None;
            }
            let closed_ms = history
                .transitions
                .iter()
                .rev()
                .find(|t| t.to == State::Done(outcome).as_str())?
                .ts_ms;
            if closed_ms < scope.from_ms || closed_ms >= scope.to_ms {
                return None;
            }
            let provider = history.runs.last()?.provider.as_str();
            Some(Closed {
                history,
                repo,
                provider,
                outcome,
                closed_ms,
            })
        })
        .collect()
}

/// What a metric is, apart from its numbers.
struct Def {
    id: &'static str,
    unit: &'static str,
    sample_unit: &'static str,
    comparable: bool,
}

const EQV_PER_COMMIT: Def = Def {
    id: "eqv_per_commit",
    unit: "eqv_tokens_per_commit",
    sample_unit: "commit",
    comparable: true,
};
const CONTEXT_AT_START: Def = Def {
    id: "context_at_start",
    unit: "tokens",
    sample_unit: "session",
    comparable: false,
};
const REREADS: Def = Def {
    id: "rereads_per_session",
    unit: "reads_per_session",
    sample_unit: "session",
    comparable: true,
};
const FINDING_BYTES: Def = Def {
    id: "finding_bytes_per_session",
    unit: "bytes_per_session",
    sample_unit: "session",
    comparable: true,
};
const FINDING_TOKENS: Def = Def {
    id: "finding_tokens_per_session",
    unit: "estimated_tokens_per_session",
    sample_unit: "session",
    comparable: false,
};
const CACHE_READ_SHARE: Def = Def {
    id: "cache_read_share",
    unit: "ppm",
    sample_unit: "prompt_token",
    comparable: false,
};
const TOOL_ERROR_RATE: Def = Def {
    id: "tool_error_rate",
    unit: "ppm",
    sample_unit: "call",
    comparable: true,
};
const PARKED: Def = Def {
    id: "parked_ms_per_item",
    unit: "ms_per_closed_item",
    sample_unit: "closed_item",
    comparable: true,
};
const FLAKY: Def = Def {
    id: "flaky_verdict_rate",
    unit: "ppm",
    sample_unit: "closed_item",
    comparable: true,
};

/// A group: repository path key, provider, session kind.
type Key<'a> = (&'a str, &'a str, &'static str);

/// The provider a row is filed under: its own.
#[derive(Debug, Default)]
struct Providers<'a> {
    first: Option<&'a str>,
}

impl<'a> Providers<'a> {
    fn key(&mut self, provider: &'a str) -> &'a str {
        let first = *self.first.get_or_insert(provider);
        // Mutant (`mutant_heat_metrics_mix_providers`): every row is filed under the first
        // provider seen, so two providers' rows are folded together.
        if cfg!(mutant_heat_metrics_mix_providers) {
            first
        } else {
            provider
        }
    }
}

/// The session kinds the store allows.
fn kind(name: &str) -> Option<&'static str> {
    match name {
        "run" => Some("run"),
        "interactive" => Some("interactive"),
        _ => None,
    }
}

/// One root session in one repository.
#[derive(Debug, Default)]
struct Session<'a> {
    /// The time and prompt size of the first message of the main transcript.
    start: Option<(i64, u64)>,
    /// Reads per path.
    reads: BTreeMap<&'a str, u64>,
    wrote: bool,
    finding_bytes: u64,
    finding_tokens: u64,
}

/// The messages and calls of one group.
#[derive(Debug, Default)]
struct Group<'a> {
    sessions: BTreeMap<&'a str, Session<'a>>,
    cache_read: u64,
    prompt: u64,
    calls: u64,
    errors: u64,
    cost: Cost,
}

/// The closed items of one repository and provider.
#[derive(Debug, Default)]
struct Items {
    closed: u64,
    flaky: u64,
    parked_ms: u64,
}

/// Everything the metrics are computed from, folded per group.
#[derive(Debug, Default)]
struct Folds<'a> {
    providers: Providers<'a>,
    groups: BTreeMap<Key<'a>, Group<'a>>,
    /// The distinct commits per repository and provider.
    commits: BTreeMap<(&'a str, &'a str), BTreeSet<&'a str>>,
    items: BTreeMap<(&'a str, &'a str), Items>,
}

/// The repository and kind of a message that counts in `scope`: in the range, in a
/// repository the scope holds, of a known kind.
fn counted<'a>(row: &'a UsageRow, scope: Scope<'_>) -> Option<(&'a str, &'static str)> {
    let repo = row.repo.as_deref()?;
    let in_range = row.ts_ms >= scope.from_ms && row.ts_ms < scope.to_ms;
    if !in_range || !scope.holds_repo(Some(repo)) {
        return None;
    }
    Some((repo, kind(&row.kind)?))
}

impl<'a> Folds<'a> {
    fn group(&mut self, repo: &'a str, provider: &'a str, kind: &'static str) -> &mut Group<'a> {
        let provider = self.providers.key(provider);
        self.groups.entry((repo, provider, kind)).or_default()
    }

    fn add_messages(&mut self, inputs: &Inputs<'a>, scope: Scope<'_>) {
        for row in inputs.usage {
            let Some((repo, kind)) = counted(row, scope) else {
                continue;
            };
            let group = self.group(repo, &row.provider, kind);
            let prompt = row.tokens.prompt_size();
            group.cache_read += row.tokens.cache_read;
            group.prompt += prompt;
            if let Some(rates) = rates(inputs.table, row) {
                group.cost = group.cost + rates.cost(&row.tokens);
            }
            let session = group.sessions.entry(&row.session).or_default();
            if row.agent.is_none() && session.start.is_none_or(|(ts, _)| row.ts_ms < ts) {
                session.start = Some((row.ts_ms, prompt));
            }
        }
    }

    fn add_calls(&mut self, inputs: &Inputs<'a>, scope: Scope<'_>) {
        let mut joined = join(inputs);
        // In time order; the calls of one message keep their order.
        joined.sort_by_key(|j| j.message.ts_ms);
        for j in &joined {
            let Some((repo, kind)) = counted(j.message, scope) else {
                continue;
            };
            let group = self.group(repo, &j.message.provider, kind);
            group.calls += 1;
            // Mutant (`mutant_heat_error_rate_counts_all_calls`): every call counts as an error.
            group.errors +=
                u64::from(j.call.is_error || cfg!(mutant_heat_error_rate_counts_all_calls));
            let session = group.sessions.entry(&j.message.session).or_default();
            for path in j.reads() {
                *session.reads.entry(path).or_default() += 1;
            }
            if session.wrote {
                continue;
            }
            if j.touches(TouchKind::Write) {
                session.wrote = true;
            } else if j.finds() {
                session.finding_bytes += j.call.result_bytes;
                session.finding_tokens += j.call.result_bytes.div_ceil(BYTES_PER_TOKEN);
            }
        }
    }

    fn add_commits(&mut self, inputs: &Inputs<'a>, scope: Scope<'_>) {
        for row in inputs.run_commits {
            if scope.holds_repo(Some(&row.repo)) {
                let provider = self.providers.key(&row.provider);
                self.commits
                    .entry((&row.repo, provider))
                    .or_default()
                    .insert(&row.commit.sha);
            }
        }
    }

    fn add_items(&mut self, inputs: &Inputs<'a>, scope: Scope<'_>) {
        for closed in closed_items(inputs, scope) {
            let provider = self.providers.key(closed.provider);
            let found = self.items.entry((closed.repo, provider)).or_default();
            found.closed += 1;
            found.flaky += u64::from(closed.outcome == Outcome::Flaky);
            found.parked_ms += crate::items::parked(closed.history, scope.to_ms)
                .values()
                .map(|p| p.ms)
                .sum::<u64>();
        }
    }

    /// The metrics of every group.
    fn metrics(&self, table: &PriceTable) -> Vec<Metric> {
        let mut keys: BTreeSet<Key<'_>> = self.groups.keys().copied().collect();
        keys.extend(
            self.commits
                .keys()
                .chain(self.items.keys())
                .map(|(repo, provider)| (*repo, *provider, "run")),
        );
        let empty = Group::default();
        let mut out = Vec::new();
        for key in keys {
            let group = self.groups.get(&key).unwrap_or(&empty);
            if key.2 == "run" {
                let shas = self.commits.get(&(key.0, key.1)).map_or(0, BTreeSet::len) as u64;
                let eqv = whole_tokens(table.eqv(group.cost).tokens());
                push(&mut out, key, &EQV_PER_COMMIT, (eqv, shas), per);
                if let Some(found) = self.items.get(&(key.0, key.1)) {
                    push(&mut out, key, &PARKED, (found.parked_ms, found.closed), per);
                    push(&mut out, key, &FLAKY, (found.flaky, found.closed), ppm);
                }
            }
            session_metrics(&mut out, key, group);
            let shares = [
                (&CACHE_READ_SHARE, (group.cache_read, group.prompt)),
                (&TOOL_ERROR_RATE, (group.errors, group.calls)),
            ];
            for (def, numbers) in shares {
                push(&mut out, key, def, numbers, ppm);
            }
        }
        out
    }
}

/// The metrics of the sessions of one group.
fn session_metrics(out: &mut Vec<Metric>, key: Key<'_>, group: &Group<'_>) {
    let sessions = group.sessions.len() as u64;
    let mut starts: Vec<u64> = group
        .sessions
        .values()
        .filter_map(|s| s.start.map(|(_, size)| size))
        .collect();
    starts.sort_unstable();
    // The lower median.
    if let Some(median) = starts.get(starts.len().saturating_sub(1) / 2) {
        let total = starts.iter().sum();
        out.push(metric(
            key,
            &CONTEXT_AT_START,
            total,
            starts.len() as u64,
            *median,
        ));
    }
    let rereads = group
        .sessions
        .values()
        .flat_map(|s| s.reads.values())
        .map(|reads| reads.saturating_sub(1))
        .sum();
    push(out, key, &REREADS, (rereads, sessions), per);
    let bytes = group.sessions.values().map(|s| s.finding_bytes).sum();
    push(out, key, &FINDING_BYTES, (bytes, sessions), per);
    let tokens = group.sessions.values().map(|s| s.finding_tokens).sum();
    if sessions > 0 {
        let mut row = metric(key, &FINDING_TOKENS, tokens, sessions, tokens / sessions);
        // Mutant (`mutant_heat_metric_estimate_as_exact`): the tokens guessed from bytes are
        // labelled exact.
        if !cfg!(mutant_heat_metric_estimate_as_exact) {
            row.exactness = Exactness::Estimate;
            row.bytes_per_token = Some(BYTES_PER_TOKEN);
        }
        out.push(row);
    }
}

/// The metrics of `inputs` in `scope`, sorted by repository, provider, kind and id.
#[must_use]
pub fn metrics(inputs: &Inputs<'_>, scope: Scope<'_>) -> Vec<Metric> {
    let mut folds = Folds::default();
    folds.add_messages(inputs, scope);
    folds.add_calls(inputs, scope);
    folds.add_commits(inputs, scope);
    folds.add_items(inputs, scope);
    let mut out = folds.metrics(inputs.table);
    out.sort_by(|a, b| {
        (&a.repo, &a.provider, a.kind, a.id).cmp(&(&b.repo, &b.provider, b.kind, b.id))
    });
    out
}

/// The metric of `key` by `def` over `(total, sample)`, unless its sample is empty.
fn push(
    out: &mut Vec<Metric>,
    key: Key<'_>,
    def: &Def,
    (total, sample): (u64, u64),
    value: fn(u64, u64) -> u64,
) {
    if sample > 0 {
        out.push(metric(key, def, total, sample, value(total, sample)));
    }
}

/// An exact metric of `key`.
fn metric(key: Key<'_>, def: &Def, total: u64, sample: u64, value: u64) -> Metric {
    Metric {
        id: def.id,
        repo: key.0.to_owned(),
        provider: key.1.to_owned(),
        kind: Some(key.2),
        value,
        total,
        sample,
        unit: def.unit,
        sample_unit: def.sample_unit,
        exactness: Exactness::Exact,
        bytes_per_token: None,
        runner_comparable: def.comparable,
    }
}

/// `total / sample`, rounded down.
fn per(total: u64, sample: u64) -> u64 {
    total / sample
}

/// `total / sample` in parts per million, rounded down.
fn ppm(total: u64, sample: u64) -> u64 {
    let share = u128::from(total) * 1_000_000 / u128::from(sample);
    u64::try_from(share).unwrap_or(u64::MAX)
}

/// An eqv rounded down to a whole token.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // An eqv is never negative,
// and a cost in picodollars over a positive rate is far below 2^64 tokens; `as` saturates.
fn whole_tokens(eqv: f64) -> u64 {
    eqv.floor() as u64
}

/// The wire form of `metrics` over `scope`.
pub(crate) fn to_api(
    identity: &str,
    scope: Scope<'_>,
    metrics: Vec<Metric>,
    repo_ref: impl Fn(&str) -> RepoRef,
) -> HeatMetrics {
    HeatMetrics {
        identity: identity.to_owned(),
        from_ms: scope.from_ms,
        to_ms: scope.to_ms,
        repo: scope.repo.map(&repo_ref),
        bytes_per_token: BYTES_PER_TOKEN,
        metrics: metrics
            .into_iter()
            .map(|m| HeatMetric {
                id: m.id.to_owned(),
                repo: repo_ref(&m.repo),
                provider: m.provider,
                kind: m.kind.map(str::to_owned),
                value: m.value,
                total: m.total,
                sample: m.sample,
                unit: m.unit.to_owned(),
                sample_unit: m.sample_unit.to_owned(),
                estimate: m.exactness == Exactness::Estimate,
                bytes_per_token: m.bytes_per_token,
                runner_comparable: m.runner_comparable,
            })
            .collect(),
    }
}

/// The metrics of `identity` over `scope` (`heat.metrics`).
///
/// `repo_ref` names a repository path key as the configuration does.
///
/// # Errors
///
/// A database error, or a stored value that does not read back.
pub async fn heat_metrics(
    reader: &Reader,
    identity: &str,
    scope: Scope<'_>,
    repo_ref: impl Fn(&str) -> RepoRef,
) -> Result<HeatMetrics, bosun_store::Error> {
    let (table, usage) = reader.usage(scope.from_ms, scope.to_ms).await?;
    let calls = reader.tool_calls(scope.from_ms, scope.to_ms).await?;
    let histories = reader.item_histories(scope.from_ms, scope.to_ms).await?;
    let run_commits = reader.run_commits(scope.from_ms, scope.to_ms).await?;
    let inputs = Inputs {
        table: &table,
        usage: &usage,
        calls: &calls,
        histories: &histories,
        run_commits: &run_commits,
    };
    Ok(to_api(identity, scope, metrics(&inputs, scope), repo_ref))
}

#[cfg(test)]
pub(crate) mod tests;
