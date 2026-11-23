//! The exact sections of the report. See `bosun_api::Report` for what each one means.

use std::collections::{BTreeMap, BTreeSet};

use bosun_api::{
    Agreement, CategoryRow, CheckTokens, CommitPeriod, CommitRow, CommitScan, Comparison, Health,
    KindRow, ModelRow, OldMethod, PerCommit, Picodollars, RepoRef, Report, SessionAgreement,
    TokenSums, Trend, UnpricedModel, VersionRow,
};
use bosun_core::{Cost, PriceTable, Pricing, Rates, Tokens};
use bosun_store::{CheckRow, Reader, UsageRow, VersionHealth, WindowSums};

/// The tolerance of the agreement with the provider's totals, in percent (#6, "Acceptance").
const TOLERANCE_PERCENT: u64 = 5;

/// The session kinds, in report order. The store allows these two only.
const KINDS: [&str; 2] = ["interactive", "run"];

/// What a report covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// Messages at or after this time.
    pub from_ms: i64,
    /// Messages before this time; after `from_ms`.
    pub to_ms: i64,
    /// A repository's path key: the sections of the range and repository scope count its
    /// messages and commits only.
    pub repo: Option<&'a str>,
}

impl Scope<'_> {
    /// The previous period: the range of the same length that ends at `from_ms`.
    fn previous(&self) -> (i64, i64) {
        let length = self.to_ms.saturating_sub(self.from_ms);
        (self.from_ms.saturating_sub(length), self.from_ms)
    }

    fn holds_repo(&self, repo: Option<&str>) -> bool {
        self.repo.is_none_or(|key| repo == Some(key))
    }
}

/// The exact sections of the report of `identity` over `scope`.
///
/// `repo_ref` names a repository path key as the configuration does; `scans` is the commit
/// reader's state, which the caller owns.
///
/// # Errors
///
/// A database error, or a stored value that does not read back.
pub async fn exact(
    reader: &Reader,
    identity: &str,
    scope: Scope<'_>,
    repo_ref: impl Fn(&str) -> RepoRef,
    scans: Vec<CommitScan>,
) -> Result<Report, bosun_store::Error> {
    let (previous_from, previous_to) = scope.previous();
    let (table, rows) = reader.usage(previous_from, scope.to_ms).await?;
    let (current, previous): (Vec<&UsageRow>, Vec<&UsageRow>) =
        rows.iter().partition(|row| row.ts_ms >= scope.from_ms);
    let in_scope: Vec<&UsageRow> = current
        .iter()
        .copied()
        .filter(|row| scope.holds_repo(row.repo.as_deref()))
        .collect();

    let mut sections = Sections::default();
    for row in &in_scope {
        sections.add(&table, row);
    }

    let commits = (
        reader.commit_counts(scope.from_ms, scope.to_ms).await?,
        reader.commit_counts(previous_from, previous_to).await?,
    );
    let per_commit = per_commit(
        &table,
        scope,
        (&current, &previous),
        &commits,
        &repo_ref,
        scans,
    );

    let ranged: BTreeSet<(&str, &str)> = current
        .iter()
        .map(|row| (row.provider.as_str(), row.session.as_str()))
        .collect();
    let (checks, windows) = reader.checks().await?;
    let agreement = agreement(&ranged, &checks, &windows);
    let health = health(&reader.ingest_health().await?, agreement);

    Ok(Report {
        identity: identity.to_owned(),
        from_ms: scope.from_ms,
        to_ms: scope.to_ms,
        repo: scope.repo.map(&repo_ref),
        reference_model: table.reference_model().to_owned(),
        total: sections.total.sums(&table),
        old_method: sections.old_method(&table),
        by_category: sections.categories(&table),
        by_model: sections.models(&table),
        by_kind: sections.kinds(&table),
        per_commit,
        health,
        unpriced: sections.unpriced(),
    })
}

/// The sections over the messages of the range and repository scope, as they are added.
#[derive(Debug, Default)]
struct Sections<'a> {
    total: Fold,
    categories: [Category; 5],
    models: BTreeMap<&'a str, Fold>,
    kinds: BTreeMap<&'a str, KindFold<'a>>,
    /// The last main-transcript message of each root session: the old method's blocks.
    last_blocks: BTreeMap<(&'a str, &'a str), &'a UsageRow>,
    unpriced: BTreeMap<&'a str, u64>,
}

/// The messages of one session kind and the root sessions they belong to.
#[derive(Debug, Default)]
struct KindFold<'a> {
    fold: Fold,
    sessions: BTreeSet<(&'a str, &'a str)>,
}

impl<'a> Sections<'a> {
    /// Adds one message. Rows come oldest first, then by message id.
    fn add(&mut self, table: &PriceTable, row: &'a UsageRow) {
        let rates = rates(table, row);
        self.total.add(&row.tokens, rates);
        self.models
            .entry(&row.model)
            .or_default()
            .add(&row.tokens, rates);
        let kind = self.kinds.entry(&row.kind).or_default();
        kind.fold.add(&row.tokens, rates);
        kind.sessions.insert((&row.provider, &row.session));
        if rates.is_none() {
            *self.unpriced.entry(&row.model).or_default() += 1;
        }
        // An unpriced message counts its tokens and no cost.
        let priced = rates.unwrap_or_default();
        for (category, (tokens, cost)) in
            self.categories.iter_mut().zip(split(&row.tokens, &priced))
        {
            category.tokens += tokens;
            category.cost = category.cost + cost;
        }
        // The last one added wins.
        if row.agent.is_none() {
            self.last_blocks.insert((&row.provider, &row.session), row);
        }
    }

    fn old_method(&self, table: &PriceTable) -> OldMethod {
        let mut old = Fold::default();
        for row in self.last_blocks.values() {
            old.add(&row.tokens, rates(table, row));
        }
        OldMethod {
            sessions: self.last_blocks.len() as u64,
            sums: old.sums(table),
        }
    }

    fn categories(&self, table: &PriceTable) -> Vec<CategoryRow> {
        CATEGORIES
            .iter()
            .zip(self.categories)
            .map(|(name, c)| CategoryRow {
                category: (*name).to_owned(),
                tokens: c.tokens,
                cost: Picodollars(c.cost.picodollars),
                eqv: table.eqv(c.cost).tokens(),
            })
            .collect()
    }

    fn models(&self, table: &PriceTable) -> Vec<ModelRow> {
        self.models
            .iter()
            .map(|(model, fold)| ModelRow {
                model: (*model).to_owned(),
                sums: fold.sums(table),
            })
            .collect()
    }

    /// Every kind of [`KINDS`] in its order, with or without messages, then any other.
    fn kinds(&self, table: &PriceTable) -> Vec<KindRow> {
        let empty = KindFold::default();
        let others = self.kinds.keys().filter(|kind| !KINDS.contains(kind));
        KINDS
            .iter()
            .chain(others)
            .map(|kind| {
                let found = self.kinds.get(kind).unwrap_or(&empty);
                KindRow {
                    kind: (*kind).to_owned(),
                    sessions: found.sessions.len() as u64,
                    sums: found.fold.sums(table),
                }
            })
            .collect()
    }

    fn unpriced(&self) -> Vec<UnpricedModel> {
        self.unpriced
            .iter()
            .map(|(model, messages)| UnpricedModel {
                model: (*model).to_owned(),
                messages: *messages,
            })
            .collect()
    }
}

/// The rates that price `row`: zero for the synthetic model, `None` when no row prices it.
fn rates(table: &PriceTable, row: &UsageRow) -> Option<Rates> {
    match table.select(&row.model, row.ts_ms, row.tokens.prompt_size()) {
        Pricing::Synthetic => Some(Rates::default()),
        Pricing::Row(price) => Some(price.rates),
        Pricing::Unknown => None,
    }
}

/// The category names, in the order of [`split`].
const CATEGORIES: [&str; 5] = [
    "input",
    "cache_write_5m",
    "cache_write_1h",
    "cache_read",
    "output",
];

/// Each category of `tokens` alone, with its cost at `rates`. The costs sum to
/// `rates.cost(tokens)`: the cost is linear in each category.
fn split(tokens: &Tokens, rates: &Rates) -> [(u64, Cost); 5] {
    let only = |f: fn(&mut Tokens, u64), n: u64| {
        let mut alone = Tokens::default();
        f(&mut alone, n);
        (n, rates.cost(&alone))
    };
    [
        only(|t, n| t.input = n, tokens.input),
        only(|t, n| t.cache_write_5m = n, tokens.cache_write_5m),
        only(|t, n| t.cache_write_1h = n, tokens.cache_write_1h),
        only(|t, n| t.cache_read = n, tokens.cache_read),
        only(|t, n| t.output = n, tokens.output),
    ]
}

#[derive(Debug, Clone, Copy, Default)]
struct Category {
    tokens: u64,
    cost: Cost,
}

/// The sums of a set of messages as they are added.
#[derive(Debug, Clone, Default)]
struct Fold {
    messages: u64,
    tokens: Tokens,
    cost: Cost,
    unpriced: u64,
}

impl Fold {
    fn add(&mut self, tokens: &Tokens, rates: Option<Rates>) {
        self.messages += 1;
        self.tokens.input += tokens.input;
        self.tokens.cache_write_5m += tokens.cache_write_5m;
        self.tokens.cache_write_1h += tokens.cache_write_1h;
        self.tokens.cache_read += tokens.cache_read;
        self.tokens.output += tokens.output;
        match rates {
            Some(rates) => self.cost = self.cost + rates.cost(tokens),
            None => self.unpriced += 1,
        }
    }

    fn merge(&mut self, other: &Self) {
        self.messages += other.messages;
        self.tokens.input += other.tokens.input;
        self.tokens.cache_write_5m += other.tokens.cache_write_5m;
        self.tokens.cache_write_1h += other.tokens.cache_write_1h;
        self.tokens.cache_read += other.tokens.cache_read;
        self.tokens.output += other.tokens.output;
        self.cost = self.cost + other.cost;
        self.unpriced += other.unpriced;
    }

    fn sums(&self, table: &PriceTable) -> TokenSums {
        TokenSums {
            messages: self.messages,
            input: self.tokens.input,
            cache_write_5m: self.tokens.cache_write_5m,
            cache_write_1h: self.tokens.cache_write_1h,
            cache_read: self.tokens.cache_read,
            output: self.tokens.output,
            cost: Picodollars(self.cost.picodollars),
            eqv: table.eqv(self.cost).tokens(),
            unpriced_messages: self.unpriced,
        }
    }
}

/// The eqv-per-commit section.
fn per_commit(
    table: &PriceTable,
    scope: Scope<'_>,
    (current, previous): (&[&UsageRow], &[&UsageRow]),
    (current_commits, previous_commits): &(BTreeMap<String, u64>, BTreeMap<String, u64>),
    repo_ref: &impl Fn(&str) -> RepoRef,
    scans: Vec<CommitScan>,
) -> PerCommit {
    let (now, before) = (
        by_repo(table, scope, current),
        by_repo(table, scope, previous),
    );
    let mut keys: BTreeSet<Option<&str>> = now.keys().chain(before.keys()).copied().collect();
    keys.extend(
        current_commits
            .keys()
            .chain(previous_commits.keys())
            .map(|key| Some(key.as_str()))
            .filter(|key| scope.holds_repo(*key)),
    );
    // Repositories by key, then the messages in no repository.
    let mut keys: Vec<Option<&str>> = keys.into_iter().collect();
    keys.sort_by_key(|key| (key.is_none(), *key));

    let empty = Fold::default();
    let period = |fold: &Fold, commits: u64| {
        let eqv = table.eqv(fold.cost).tokens();
        CommitPeriod {
            messages: fold.messages,
            commits,
            cost: Picodollars(fold.cost.picodollars),
            eqv,
            eqv_per_commit: (commits > 0).then(|| eqv / to_f64(commits)),
        }
    };
    let mut attributed = (Fold::default(), 0, Fold::default(), 0);
    let mut rows = Vec::with_capacity(keys.len());
    for key in keys {
        let (now, before) = (
            now.get(&key).unwrap_or(&empty),
            before.get(&key).unwrap_or(&empty),
        );
        let count = |commits: &BTreeMap<String, u64>| {
            key.and_then(|k| commits.get(k)).copied().unwrap_or(0)
        };
        let (now_commits, before_commits) = (count(current_commits), count(previous_commits));
        if key.is_some() {
            attributed.0.merge(now);
            attributed.1 += now_commits;
            attributed.2.merge(before);
            attributed.3 += before_commits;
        }
        rows.push(CommitRow {
            repo: key.map(repo_ref),
            trend: trend(period(now, now_commits), period(before, before_commits)),
        });
    }
    let (previous_from_ms, previous_to_ms) = scope.previous();
    PerCommit {
        previous_from_ms,
        previous_to_ms,
        rows,
        attributed: trend(
            period(&attributed.0, attributed.1),
            period(&attributed.2, attributed.3),
        ),
        scans,
    }
}

#[allow(clippy::cast_precision_loss)] // A ratio for display; both operands are exact.
fn to_f64(n: u64) -> f64 {
    n as f64
}

/// The messages of `rows` in `scope`, folded per repository.
fn by_repo<'a>(
    table: &PriceTable,
    scope: Scope<'_>,
    rows: &[&'a UsageRow],
) -> BTreeMap<Option<&'a str>, Fold> {
    let mut out: BTreeMap<Option<&str>, Fold> = BTreeMap::new();
    for row in rows {
        if scope.holds_repo(row.repo.as_deref()) {
            out.entry(row.repo.as_deref())
                .or_default()
                .add(&row.tokens, rates(table, row));
        }
    }
    out
}

fn trend(current: CommitPeriod, previous: CommitPeriod) -> Trend {
    let change = match (current.eqv_per_commit, previous.eqv_per_commit) {
        (Some(now), Some(before)) if before > 0.0 => Some(now / before - 1.0),
        _ => None,
    };
    Trend {
        current,
        previous,
        change,
    }
}

/// The agreement of the root sessions in `ranged` that have the provider's totals.
fn agreement(
    ranged: &BTreeSet<(&str, &str)>,
    checks: &[CheckRow],
    windows: &[WindowSums],
) -> Agreement {
    let mut provider: BTreeMap<(&str, &str), CheckTokens> = BTreeMap::new();
    for check in checks {
        let key = (check.provider.as_str(), check.session.as_str());
        if ranged.contains(&key) {
            let sums = provider.entry(key).or_default();
            sums.input += check.input;
            sums.cache_write += check.cache_write;
            sums.cache_read += check.cache_read;
            sums.output += check.output;
        }
    }
    // Main transcript plus every sub-agent: the provider's totals include sub-agents (#3).
    let mut transcripts: BTreeMap<(&str, &str), (u64, u64, CheckTokens)> = BTreeMap::new();
    for window in windows {
        if cfg!(mutant_report_main_only) && window.agent.is_some() {
            // Mutant: the agreement sees the main transcript only.
            continue;
        }
        let key = (window.provider.as_str(), window.session.as_str());
        if !provider.contains_key(&key) {
            continue;
        }
        let (count, messages, sums) = transcripts.entry(key).or_default();
        *count += 1;
        *messages += window.messages;
        let t = &window.tokens;
        sums.input += t.input;
        sums.cache_write += t.cache_write_5m + t.cache_write_1h;
        sums.cache_read += t.cache_read;
        sums.output += t.output;
    }

    let mut total = (CheckTokens::default(), CheckTokens::default());
    let mut rows = Vec::with_capacity(provider.len());
    for (key, mut theirs) in provider {
        let (windows, messages, mut ours) = transcripts.remove(&key).unwrap_or_default();
        finish(&mut ours);
        finish(&mut theirs);
        add(&mut total.0, &ours);
        add(&mut total.1, &theirs);
        rows.push(SessionAgreement {
            session: key.1.to_owned(),
            windows,
            messages,
            comparison: compare(ours, theirs),
        });
    }
    Agreement {
        tolerance_percent: TOLERANCE_PERCENT,
        sessions: rows.len() as u64,
        within: rows.iter().filter(|r| r.comparison.within).count() as u64,
        total: compare(total.0, total.1),
        rows,
    }
}

fn finish(sums: &mut CheckTokens) {
    sums.total = sums.input + sums.cache_write + sums.cache_read + sums.output;
}

fn add(into: &mut CheckTokens, sums: &CheckTokens) {
    into.input += sums.input;
    into.cache_write += sums.cache_write;
    into.cache_read += sums.cache_read;
    into.output += sums.output;
    into.total += sums.total;
}

/// Within the tolerance: `|ours - theirs| * 100 <= tolerance * theirs`, in exact integers.
fn compare(transcripts: CheckTokens, provider: CheckTokens) -> Comparison {
    let difference = u128::from(transcripts.total.abs_diff(provider.total));
    let within = difference * 100 <= u128::from(TOLERANCE_PERCENT) * u128::from(provider.total);
    Comparison {
        transcripts,
        provider,
        within,
    }
}

fn health(versions: &BTreeMap<u32, VersionHealth>, agreement: Agreement) -> Health {
    let rows: Vec<VersionRow> = versions
        .iter()
        .map(|(version, h)| VersionRow {
            parser_version: *version,
            files: h.files,
            files_counted: h.files_counted,
            lines: h.lines,
            gaps: h.gaps,
        })
        .collect();
    Health {
        files: rows.iter().map(|r| r.files).sum(),
        files_counted: rows.iter().map(|r| r.files_counted).sum(),
        lines: rows.iter().map(|r| r.lines).sum(),
        gaps: rows.iter().map(|r| r.gaps).sum(),
        versions: rows,
        agreement,
    }
}

#[cfg(test)]
mod tests;
