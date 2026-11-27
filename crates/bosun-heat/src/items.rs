//! The loop's sections (#72). See `bosun_api::Items` and `bosun_api::InferredItems` for what
//! each number means.

use std::collections::{BTreeMap, BTreeSet};

use bosun_api::{
    AttemptReport, DeniedCall, Evidence, FlakyVerdict, InferredItem, InferredItems,
    InferredSession, ItemRow, Items, LeaseWaits, ModelRow, ParkedTime, RefusedEdit, RepoRef,
    RetriedItem, RunCommit, RunSession, TrailerMissing, Trailers, WorktreeUse,
};
use bosun_core::{Exit, PriceTable, SpawnMode};
use bosun_store::{ItemHistory, LeaseWaitRow, Reader, RunCommitRow, UsageRow};

use crate::exact::{Fold, Scope, rates};

/// The rule of [`InferredItems`].
const RULE: &str = "an interactive session is inferred for an item when one of its windows \
    contains a commit that no run's commit range holds and whose Item: trailer names the item, \
    by the time-and-repository link of #3 (the window's span, from its first to its last \
    message, holds the commit's time, and its messages are in the commit's repository); the \
    whole session counts, main transcript and sub-agents, so the figure is an upper bound";

/// The trailers the daemon writes, in report order.
const TRAILERS: [&str; 3] = ["Item", "Run", "Session"];

/// A session: (provider, session id).
type SessionKey = (String, String);

/// The messages of each session, folded: in total and by model.
#[derive(Debug, Default)]
struct SessionFold {
    total: Fold,
    models: BTreeMap<String, Fold>,
}

/// The messages of `sessions`, folded per session, with the price table they were read with.
async fn fold_sessions(
    reader: &Reader,
    sessions: &BTreeSet<SessionKey>,
) -> Result<(PriceTable, BTreeMap<SessionKey, SessionFold>, Vec<UsageRow>), bosun_store::Error> {
    let keys: Vec<SessionKey> = sessions.iter().cloned().collect();
    let (table, rows) = reader.session_usage(&keys).await?;
    let mut out: BTreeMap<SessionKey, SessionFold> = BTreeMap::new();
    for row in &rows {
        let fold = out
            .entry((row.provider.clone(), row.session.clone()))
            .or_default();
        let rates = rates(&table, row);
        fold.total.add(&row.tokens, rates);
        fold.models
            .entry(row.model.clone())
            .or_default()
            .add(&row.tokens, rates);
    }
    Ok((table, out, rows))
}

fn mode_name(mode: SpawnMode) -> &'static str {
    match mode {
        SpawnMode::Fresh => "fresh",
        SpawnMode::Resume => "resume",
        SpawnMode::Reset => "reset",
    }
}

fn exit_name(exit: Exit) -> &'static str {
    match exit {
        Exit::Ok => "ok",
        Exit::Limit { .. } => "limit",
        Exit::Auth => "auth",
        Exit::Crash => "crash",
    }
}

fn duration(from_ms: i64, to_ms: i64) -> u64 {
    u64::try_from(to_ms.saturating_sub(from_ms)).unwrap_or(0)
}

/// The values of trailer `name` (matched without regard to case).
fn trailer<'a>(trailers: &'a BTreeMap<String, Vec<String>>, name: &str) -> &'a [String] {
    trailers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map_or(&[], |(_, values)| values.as_slice())
}

/// The time parked by reason of one item, from its transitions: each park runs to the next
/// transition, and one still open runs to `to_ms`.
pub(crate) fn parked(history: &ItemHistory, to_ms: i64) -> BTreeMap<String, ParkedTime> {
    let mut out: BTreeMap<String, ParkedTime> = BTreeMap::new();
    let mut open: Option<(String, i64)> = None;
    for t in &history.transitions {
        if let Some((reason, since)) = open.take() {
            park_row(&mut out, &reason).ms += duration(since, t.ts_ms);
        }
        if t.to == "parked" {
            let reason = t.to_detail.clone().unwrap_or_default();
            park_row(&mut out, &reason).parks += 1;
            open = Some((reason, t.ts_ms));
        }
    }
    if let Some((reason, since)) = open {
        let row = park_row(&mut out, &reason);
        row.ms += duration(since, to_ms);
        row.ongoing = true;
    }
    out
}

/// The row of `reason`, added empty when it is not there yet.
fn park_row<'a>(out: &'a mut BTreeMap<String, ParkedTime>, reason: &str) -> &'a mut ParkedTime {
    out.entry(reason.to_owned()).or_insert_with(|| ParkedTime {
        reason: reason.to_owned(),
        parks: 0,
        ms: 0,
        ongoing: false,
    })
}

/// The exact loop sections over `scope`.
pub(crate) async fn items(
    reader: &Reader,
    scope: Scope<'_>,
    repo_ref: &impl Fn(&str) -> RepoRef,
) -> Result<Items, bosun_store::Error> {
    let histories: Vec<ItemHistory> = reader
        .item_histories(scope.from_ms, scope.to_ms)
        .await?
        .into_iter()
        .filter(|h| scope.holds_repo(Some(&h.item.repo)))
        .collect();
    let sessions: BTreeSet<SessionKey> = histories
        .iter()
        .flat_map(|h| &h.runs)
        .map(|r| (r.provider.clone(), r.session_key.clone()))
        .collect();
    let (table, folds, _) = fold_sessions(reader, &sessions).await?;
    #[cfg(mutant_report_merge_inferred)]
    let inferred = inferred(reader, scope, repo_ref).await?;

    let mut out = Items::default();
    let mut parked_total: BTreeMap<String, ParkedTime> = BTreeMap::new();
    for history in &histories {
        let key = &history.item.item.id.0;
        let Folded {
            attempts,
            sessions,
            #[cfg_attr(not(mutant_report_merge_inferred), allow(unused_mut))]
            mut fold,
            wall_clock_ms,
        } = attempts(history, &folds, &table, &mut out.flaky);
        #[cfg(mutant_report_merge_inferred)]
        if let Some(row) = inferred.rows.iter().find(|r| &r.item == key) {
            // Mutant: the inferred sessions' messages folded into the item's runs.
            let ids: BTreeSet<SessionKey> = row
                .sessions
                .iter()
                .map(|s| (s.provider.clone(), s.session.clone()))
                .collect();
            let (_, extra, _) = fold_sessions(reader, &ids).await?;
            for f in extra.values() {
                fold.merge(&f.total);
            }
        }

        for t in &history.transitions {
            if t.to == "needs_you" && t.to_detail.as_deref() == Some("guard_protected_path") {
                out.refused_edits.push(RefusedEdit {
                    item: key.clone(),
                    attempt: t.attempt,
                    ts_ms: t.ts_ms,
                });
            }
        }
        let parked = parked(history, scope.to_ms);
        for row in parked.values() {
            let total = park_row(&mut parked_total, &row.reason);
            total.parks += row.parks;
            total.ms += row.ms;
            total.ongoing |= row.ongoing;
        }
        let failed = history.item.item.failed;
        if history.attempts.len() > 1 {
            out.retried.push(RetriedItem {
                item: key.clone(),
                attempts: u32::try_from(history.attempts.len()).unwrap_or(u32::MAX),
                failed,
            });
        }
        out.rows.push(ItemRow {
            item: key.clone(),
            repo: repo_ref(&history.item.repo),
            state: history.item.item.state.as_str().to_owned(),
            detail: history.detail.clone(),
            failed,
            runs: fold.sums(&table),
            sessions,
            wall_clock_ms,
            parked: parked.into_values().collect(),
            attempts,
        });
    }
    out.refused_edits
        .sort_by(|a, b| (a.ts_ms, &a.item).cmp(&(b.ts_ms, &b.item)));
    out.parked = parked_total.into_values().collect();
    out.denied_calls = denied_calls(&histories);

    let commits = run_commits(reader, scope, &histories).await?;
    add_commits(&mut out, &commits, repo_ref);
    let in_range: u64 = reader
        .commit_counts(scope.from_ms, scope.to_ms)
        .await?
        .iter()
        .filter(|(repo, _)| scope.holds_repo(Some(repo)))
        .map(|(_, n)| n)
        .sum();
    let linked: BTreeSet<(&str, &str)> = commits
        .iter()
        .map(|c| (c.repo.as_str(), c.commit.sha.as_str()))
        .collect();
    out.outside_runs = in_range.saturating_sub(linked.len() as u64);
    leases_and_worktrees(reader, scope, &mut out).await?;
    Ok(out)
}

/// The refused tool calls of `histories`, per tool and redacted target, most frequent first,
/// then by tool and target.
fn denied_calls(histories: &[ItemHistory]) -> Vec<DeniedCall> {
    let mut by: BTreeMap<(String, Option<String>), (u64, BTreeSet<String>)> = BTreeMap::new();
    for history in histories {
        for denial in &history.denials {
            let target = denial.target.as_deref().map(bosun_core::publish::redact);
            let entry = by.entry((denial.tool.clone(), target)).or_default();
            entry.0 += 1;
            entry.1.insert(history.item.item.id.0.clone());
        }
    }
    let mut out: Vec<DeniedCall> = by
        .into_iter()
        .map(|((tool, target), (count, items))| DeniedCall {
            tool,
            target,
            count,
            items: items.into_iter().collect(),
        })
        .collect();
    // A stable sort: equal counts keep the (tool, target) order of the map.
    out.sort_by_key(|a| std::cmp::Reverse(a.count));
    out
}

/// The lease waits of the range and the worktrees on disk or quarantined now, in `scope`'s
/// repository when it names one (#8).
async fn leases_and_worktrees(
    reader: &Reader,
    scope: Scope<'_>,
    out: &mut Items,
) -> Result<(), bosun_store::Error> {
    let repos: BTreeMap<String, String> = reader
        .items(false)
        .await?
        .into_iter()
        .map(|stored| (stored.item.id.0, stored.repo))
        .collect();
    let holds = |item: &str| scope.holds_repo(repos.get(item).map(String::as_str));
    out.lease_waits = lease_waits(
        reader
            .lease_waits(scope.from_ms, scope.to_ms)
            .await?
            .iter()
            .filter(|w| match &w.item {
                Some(item) => holds(&item.0),
                // A command's wait is of no repository.
                None => scope.repo.is_none(),
            }),
    );
    let mut worktrees = reader.open_worktrees().await?;
    worktrees.extend(reader.quarantined_worktrees().await?);
    worktrees.sort_by_key(|w| w.created_ms);
    out.worktrees = worktrees
        .into_iter()
        .filter(|w| holds(&w.item.0))
        .map(|w| WorktreeUse {
            item: w.item.0,
            path: w.path,
            kind: w.kind.as_str().to_owned(),
            state: w.state.as_str().to_owned(),
            bytes: w.bytes,
            blocking: w.blocking,
        })
        .collect();
    Ok(())
}

/// The granted waits of `rows` per resource, longest wait first, then by resource.
fn lease_waits<'a>(rows: impl Iterator<Item = &'a LeaseWaitRow>) -> Vec<LeaseWaits> {
    let mut by: BTreeMap<&str, LeaseWaits> = BTreeMap::new();
    for row in rows {
        let Some(granted) = row.granted_ms else {
            continue;
        };
        let waited = duration(row.filed_ms, granted);
        let entry = by.entry(&row.resource).or_insert_with(|| LeaseWaits {
            resource: row.resource.clone(),
            requests: 0,
            total_ms: 0,
            longest_ms: 0,
        });
        entry.requests += 1;
        entry.total_ms += waited;
        entry.longest_ms = entry.longest_ms.max(waited);
    }
    let mut out: Vec<LeaseWaits> = by.into_values().collect();
    out.sort_by(|a, b| {
        b.longest_ms
            .cmp(&a.longest_ms)
            .then_with(|| a.resource.cmp(&b.resource))
    });
    out
}

/// The row of one item as the loop's section reports it (#72), whatever its time: its runs
/// exact, per attempt and by model. What the item's sticky report shows of its cost and
/// models (#111). `None` when the store has no such item.
///
/// # Errors
///
/// A database error, or a stored value that does not read back.
pub async fn item_row(
    reader: &Reader,
    id: &bosun_core::ItemId,
    repo: RepoRef,
) -> Result<Option<ItemRow>, bosun_store::Error> {
    let Some(item) = reader.item(id).await? else {
        return Ok(None);
    };
    let history = ItemHistory {
        item,
        detail: None,
        attempts: reader.attempts(id).await?,
        runs: reader.runs(id).await?,
        transitions: reader.transitions(id).await?,
        flaky_tests: reader.flaky_tests(id).await?,
        agent_calls: reader.agent_calls(id).await?,
        denials: reader.denials(id).await?,
        verdicts: reader.verdict_runs(id).await?,
    };
    let sessions: BTreeSet<SessionKey> = history
        .runs
        .iter()
        .map(|r| (r.provider.clone(), r.session_key.clone()))
        .collect();
    let (table, folds, _) = fold_sessions(reader, &sessions).await?;
    let Folded {
        attempts,
        sessions,
        fold,
        wall_clock_ms,
    } = attempts(&history, &folds, &table, &mut Vec::new());
    let to_ms = history.transitions.last().map_or(0, |t| t.ts_ms);
    Ok(Some(ItemRow {
        item: id.0.clone(),
        repo,
        state: history.item.item.state.as_str().to_owned(),
        detail: None,
        failed: history.item.item.failed,
        runs: fold.sums(&table),
        sessions,
        wall_clock_ms,
        parked: parked(&history, to_ms).into_values().collect(),
        attempts,
    }))
}

/// The attempts of one item, with the item's runs folded.
struct Folded {
    attempts: Vec<AttemptReport>,
    /// Distinct run sessions.
    sessions: u64,
    /// The item's runs.
    fold: Fold,
    wall_clock_ms: u64,
}

/// The attempts of `history`; its flaky verdicts are added to `flaky`.
fn attempts(
    history: &ItemHistory,
    folds: &BTreeMap<SessionKey, SessionFold>,
    table: &PriceTable,
    flaky: &mut Vec<FlakyVerdict>,
) -> Folded {
    let key = &history.item.item.id.0;
    // A session id counts once, under the first spawn that names it.
    let mut seen: BTreeSet<SessionKey> = BTreeSet::new();
    let mut attempts = Vec::with_capacity(history.attempts.len());
    let mut item_fold = Fold::default();
    let mut wall_clock_ms = 0;
    for (index, attempt) in history.attempts.iter().enumerate() {
        let number = attempt.attempt.number;
        let mut fold = Fold::default();
        let mut models: BTreeMap<&str, Fold> = BTreeMap::new();
        let mut runs = Vec::new();
        for run in history.runs.iter().filter(|r| r.attempt == number) {
            let id = (run.provider.clone(), run.session_key.clone());
            let mut sums = Fold::default();
            if seen.insert(id.clone())
                && let Some(found) = folds.get(&id)
            {
                sums.merge(&found.total);
                fold.merge(&found.total);
                for (model, f) in &found.models {
                    models.entry(model).or_default().merge(f);
                }
            }
            runs.push(RunSession {
                spawn: run.spawn,
                mode: mode_name(run.mode).to_owned(),
                provider: run.provider.clone(),
                session: run.session_key.clone(),
                started_ms: run.started_ms,
                ended_ms: run.ended_ms,
                exit: run.exit.map(|e| exit_name(e).to_owned()),
                base_sha: run.base_sha.clone(),
                head_sha: run.head_sha.clone(),
                sums: sums.sums(table),
            });
        }
        let last = index + 1 == history.attempts.len();
        if !cfg!(mutant_report_item_last_attempt) || last {
            // Mutant `mutant_report_item_last_attempt`: only the last attempt is billed.
            item_fold.merge(&fold);
        }
        let wall_clock = attempt
            .ended_ms
            .map(|end| duration(attempt.started_ms, end));
        wall_clock_ms += wall_clock.unwrap_or(0);
        attempts.push(AttemptReport {
            number,
            state: attempt.attempt.state.as_str().to_owned(),
            started_ms: attempt.started_ms,
            ended_ms: attempt.ended_ms,
            wall_clock_ms: wall_clock,
            crash_retries: attempt.attempt.crash_retries,
            refused_calls: history
                .agent_calls
                .iter()
                .filter(|c| c.attempt == number && c.refused)
                .count() as u64,
            sums: fold.sums(table),
            models: models
                .into_iter()
                .map(|(model, f)| ModelRow {
                    model: model.to_owned(),
                    sums: f.sums(table),
                })
                .collect(),
            sessions: runs,
        });

        let tests: Vec<String> = history
            .flaky_tests
            .iter()
            .filter(|(a, _)| *a == number)
            .map(|(_, name)| name.clone())
            .collect();
        if attempt.attempt.state.as_str() == "flaky" || !tests.is_empty() {
            flaky.push(FlakyVerdict {
                item: key.clone(),
                attempt: number,
                tests,
            });
        }
    }
    Folded {
        attempts,
        sessions: seen.len() as u64,
        fold: item_fold,
        wall_clock_ms,
    }
}

/// Adds `commits` to the commit sections of `out`.
fn add_commits(out: &mut Items, commits: &[RunCommitRow], repo_ref: &impl Fn(&str) -> RepoRef) {
    for row in commits {
        let present = |name| !trailer(&row.commit.trailers, name).is_empty();
        let trailers = Trailers {
            item: present("Item"),
            run: present("Run"),
            session: present("Session"),
        };
        let missing: Vec<String> = TRAILERS
            .iter()
            .filter(|name| !present(name))
            .map(|name| (*name).to_owned())
            .collect();
        if !missing.is_empty() {
            out.trailer_missing.push(TrailerMissing {
                repo: repo_ref(&row.repo),
                sha: row.commit.sha.clone(),
                item: row.item.0.clone(),
                missing,
            });
        }
        out.commits.push(RunCommit {
            repo: repo_ref(&row.repo),
            sha: row.commit.sha.clone(),
            ts_ms: row.commit.ts_ms,
            item: row.item.0.clone(),
            attempt: row.attempt,
            spawn: row.spawn,
            session: row.session_key.clone(),
            trailers,
        });
    }
}

/// The commits of the range in `scope` that a run's commit range holds.
#[cfg(not(mutant_report_commit_by_time))]
async fn run_commits(
    reader: &Reader,
    scope: Scope<'_>,
    _histories: &[ItemHistory],
) -> Result<Vec<RunCommitRow>, bosun_store::Error> {
    Ok(reader
        .run_commits(scope.from_ms, scope.to_ms)
        .await?
        .into_iter()
        .filter(|row| scope.holds_repo(Some(&row.repo)))
        .collect())
}

/// Mutant `mutant_report_commit_by_time`: a commit is linked to every run of the repository
/// whose time span contains it, as the inferred link of stage 0 does.
#[cfg(mutant_report_commit_by_time)]
async fn run_commits(
    reader: &Reader,
    scope: Scope<'_>,
    histories: &[ItemHistory],
) -> Result<Vec<RunCommitRow>, bosun_store::Error> {
    let mut out = Vec::new();
    for link in reader.commit_links(scope.from_ms, scope.to_ms).await? {
        if !scope.holds_repo(Some(&link.repo)) {
            continue;
        }
        for history in histories.iter().filter(|h| h.item.repo == link.repo) {
            for run in &history.runs {
                let end = run.ended_ms.unwrap_or(i64::MAX);
                if run.started_ms <= link.commit.ts_ms && link.commit.ts_ms <= end {
                    out.push(RunCommitRow {
                        repo: link.repo.clone(),
                        commit: link.commit.clone(),
                        item: history.item.item.id.clone(),
                        attempt: run.attempt,
                        spawn: run.spawn,
                        provider: run.provider.clone(),
                        session_key: run.session_key.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// The interactive sessions inferred for items over `scope`, by the rule of [`RULE`].
pub(crate) async fn inferred(
    reader: &Reader,
    scope: Scope<'_>,
    repo_ref: &impl Fn(&str) -> RepoRef,
) -> Result<InferredItems, bosun_store::Error> {
    let exact: BTreeSet<(String, String)> = reader
        .run_commits(scope.from_ms, scope.to_ms)
        .await?
        .into_iter()
        .map(|row| (row.repo, row.commit.sha))
        .collect();
    let items: BTreeMap<String, String> = reader
        .items(false)
        .await?
        .into_iter()
        .map(|stored| (stored.item.id.0, stored.repo))
        .collect();
    // Item -> session -> the shas that link them.
    let mut found: BTreeMap<String, BTreeMap<SessionKey, BTreeSet<String>>> = BTreeMap::new();
    for link in reader.commit_links(scope.from_ms, scope.to_ms).await? {
        if !scope.holds_repo(Some(&link.repo))
            || exact.contains(&(link.repo.clone(), link.commit.sha.clone()))
        {
            continue;
        }
        // The windows the stored spans link the commit to (#23), main transcripts and
        // sub-agents, as sessions; run sessions are dropped below.
        let sessions: BTreeSet<SessionKey> = link
            .sessions
            .iter()
            .map(|s| (s.provider.clone(), s.session_key.clone()))
            .collect();
        for named in trailer(&link.commit.trailers, "Item") {
            let named = named.trim();
            if items.get(named) != Some(&link.repo) {
                continue;
            }
            for session in &sessions {
                found
                    .entry(named.to_owned())
                    .or_default()
                    .entry(session.clone())
                    .or_default()
                    .insert(link.commit.sha.clone());
            }
        }
    }
    let sessions: BTreeSet<SessionKey> = found.values().flat_map(|s| s.keys().cloned()).collect();
    let (table, folds, rows) = fold_sessions(reader, &sessions).await?;
    // A run session is never inferred: its cost is a run's.
    let runs: BTreeSet<SessionKey> = rows
        .iter()
        .filter(|row| row.kind == "run")
        .map(|row| (row.provider.clone(), row.session.clone()))
        .collect();
    let empty = SessionFold::default();
    let mut out = InferredItems {
        estimate: true,
        rule: RULE.to_owned(),
        rows: Vec::new(),
    };
    for (item, sessions) in found {
        let mut total = Fold::default();
        let mut listed = Vec::new();
        for (id, shas) in sessions {
            if runs.contains(&id) {
                continue;
            }
            let fold = folds.get(&id).unwrap_or(&empty);
            total.merge(&fold.total);
            listed.push(InferredSession {
                provider: id.0,
                session: id.1,
                evidence: shas
                    .into_iter()
                    .map(|sha| Evidence::CommitTrailer { sha })
                    .collect(),
                sums: fold.total.sums(&table),
            });
        }
        if listed.is_empty() {
            continue;
        }
        out.rows.push(InferredItem {
            repo: repo_ref(&items[&item]),
            item,
            sessions: listed,
            sums: total.sums(&table),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
