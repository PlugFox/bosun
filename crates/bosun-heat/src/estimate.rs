//! The estimate sections of the report (#27). See `bosun_api::Estimates` for what each one
//! means; the formulas are the constants below and travel in the result as text.
//!
//! Everything here prices tool results, which the provider does not bill separately: the
//! tokens are guessed from bytes, and the cost follows the cache through the context window.
//! Nothing computed here is ever added to a figure of the exact report.

use std::collections::{BTreeMap, BTreeSet};

use bosun_api::{
    ByFile, ByTool, Estimates, FileRow, Finding, FindingRow, Picodollars, RepeatedRead,
    RepeatedReads, RepoRef, ResultCost, ToolRow as ToolSummary,
};
use bosun_core::{Cost, PriceTable, Tokens, TouchKind};
use bosun_store::{Reader, ToolRow, UsageRow};

use crate::curve::{stretch_ends, windows};
use crate::exact::{Scope, rates};

/// Bytes per token: the one global ratio that turns a result's stored size into tokens.
///
/// About four bytes of English text or source code make one token for the Claude tokenizers;
/// it is a rule of thumb, not measured per model, and the same for every tool and provider.
/// A result of `b` bytes is `ceil(b / BYTES_PER_TOKEN)` tokens.
pub const BYTES_PER_TOKEN: u64 = 4;

/// How a result is priced (see `bosun_api::ResultCost`).
const FORMULA: &str = "tokens = ceil(result bytes / bytes_per_token); a result is carried by \
     the messages after its call up to the first compaction after it; cost = tokens * \
     cache-write rate of the first carrying message (1 h if it wrote more 1 h than 5 min \
     tokens, else 5 min) + sum over the later carrying messages of tokens * their cache-read \
     rate";

const BY_TOOL: &str = "per tool name, the results of its calls, priced by the result formula";

const BY_FILE: &str = "per file, the results of every call that read, searched, wrote, ran \
     (exec) or named it with an unknown effect, priced by the result formula; a search touches \
     the directory it searched";

const REPEATED: &str = "per file read more than once: within = every read of a session after \
     that session's first read of the file; across = the first read in each session after the \
     first session that read it; both priced by the result formula";

const FINDING: &str = "per root session (sub-agents included), its read and search calls \
     before the call of its first write (all of them without a write), ordered by message time, \
     priced by the result formula";

const KNOWN_GAPS: [&str; 4] = [
    "Tokens are bytes divided by one global ratio, the same for every tool, language and model.",
    "Images are estimated from their base64 bytes, which says little about their real token \
     count.",
    "A call's input is part of the calling message's output and is not counted here; only \
     results are.",
    "A result is carried only within the range: messages of the window before or after it \
     are not seen, nor what a compaction keeps of it in its summary.",
];

/// The estimated cost of one call's result.
#[derive(Debug, Clone, Copy, Default)]
struct Carry {
    tokens: u64,
    /// Messages that read it from the cache after the first wrote it.
    turns: u64,
    write: Cost,
    read: Cost,
}

/// A call of the scope, its message and its result's cost.
#[derive(Debug, Clone, Copy)]
struct Priced<'a> {
    call: &'a ToolRow,
    message: &'a UsageRow,
    carry: Carry,
}

impl Priced<'_> {
    fn touches(&self, kind: TouchKind) -> bool {
        self.call.touches.iter().any(|(_, k)| *k == kind)
    }

    /// A read or a search that writes nothing.
    fn finds(&self) -> bool {
        !self.touches(TouchKind::Write)
            && (self.touches(TouchKind::Read) || self.touches(TouchKind::Search))
    }

    fn root(&self) -> (&str, &str) {
        (&self.message.provider, &self.message.session)
    }
}

/// The calls of `calls` made by a message of `rows`, in the order of `calls`, each with the
/// cost of its result.
fn price<'a>(table: &PriceTable, rows: &[&'a UsageRow], calls: &'a [ToolRow]) -> Vec<Priced<'a>> {
    let windows = windows(rows);
    // (provider, message id) -> (its window's messages, their stretch ends, its position).
    let mut index: BTreeMap<(&str, &str), (&[&UsageRow], usize)> = BTreeMap::new();
    let ends: BTreeMap<_, Vec<usize>> = windows
        .iter()
        .map(|(key, messages)| (*key, stretch_ends(messages)))
        .collect();
    for messages in windows.values() {
        for (position, message) in messages.iter().enumerate() {
            index.insert(
                (&message.provider, &message.message),
                (messages.as_slice(), position),
            );
        }
    }
    calls
        .iter()
        .filter_map(|call| {
            let (messages, i) = *index.get(&(call.provider.as_str(), call.message.as_str()))?;
            let message = messages[i];
            let key = (
                message.provider.as_str(),
                message.session.as_str(),
                message.agent.as_deref(),
            );
            let end = ends[&key][i];
            Some(Priced {
                call,
                message,
                carry: carry(table, call.result_bytes, &messages[i + 1..end]),
            })
        })
        .collect()
}

/// The cost of a result of `bytes` carried by `carriers`: the first writes it, the rest read it.
fn carry(table: &PriceTable, bytes: u64, carriers: &[&UsageRow]) -> Carry {
    let tokens = bytes.div_ceil(BYTES_PER_TOKEN);
    let Some((first, rest)) = carriers.split_first() else {
        return Carry {
            tokens,
            ..Carry::default()
        };
    };
    // Mutant: a result costed once, by its first write, as if nothing carried it.
    let rest = if cfg!(mutant_report_no_carry) {
        &[]
    } else {
        rest
    };
    let at = |row: &UsageRow, tokens: Tokens| rates(table, row).unwrap_or_default().cost(&tokens);
    let mut written = Tokens::default();
    if first.tokens.cache_write_1h > first.tokens.cache_write_5m {
        written.cache_write_1h = tokens;
    } else {
        written.cache_write_5m = tokens;
    }
    let read = Tokens {
        cache_read: tokens,
        ..Tokens::default()
    };
    Carry {
        tokens,
        turns: rest.len() as u64,
        write: at(first, written),
        read: rest.iter().map(|row| at(row, read)).sum(),
    }
}

/// The estimated cost of every result of `calls` in the scope of `rows`: what the mutant
/// `mutant_report_merge_estimates` folds into the exact total.
#[cfg(mutant_report_merge_estimates)]
pub(crate) fn total_cost(table: &PriceTable, rows: &[&UsageRow], calls: &[ToolRow]) -> Cost {
    price(table, rows, calls)
        .iter()
        .map(|p| p.carry.write + p.carry.read)
        .sum()
}

/// A [`ResultCost`] as calls are added.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    calls: u64,
    bytes: u64,
    tokens: u64,
    turns: u64,
    write: Cost,
    carry: Cost,
}

impl Acc {
    fn add(&mut self, priced: &Priced<'_>) {
        self.calls += 1;
        self.bytes += priced.call.result_bytes;
        self.tokens += priced.carry.tokens;
        self.turns += priced.carry.turns;
        self.write = self.write + priced.carry.write;
        self.carry = self.carry + priced.carry.read;
    }

    fn merge(&mut self, other: &Self) {
        self.calls += other.calls;
        self.bytes += other.bytes;
        self.tokens += other.tokens;
        self.turns += other.turns;
        self.write = self.write + other.write;
        self.carry = self.carry + other.carry;
    }

    fn cost(&self) -> Cost {
        self.write + self.carry
    }

    fn finish(&self, table: &PriceTable) -> ResultCost {
        ResultCost {
            calls: self.calls,
            result_bytes: self.bytes,
            tokens: self.tokens,
            carry_turns: self.turns,
            write_eqv: table.eqv(self.write).tokens(),
            carry_eqv: table.eqv(self.carry).tokens(),
            cost: Picodollars(self.cost().picodollars),
            eqv: table.eqv(self.cost()).tokens(),
        }
    }
}

/// The estimate sections of the report of `identity` over `scope`.
///
/// # Errors
///
/// A database error, or a stored value that does not read back.
pub async fn estimates(
    reader: &Reader,
    identity: &str,
    scope: Scope<'_>,
    repo_ref: impl Fn(&str) -> RepoRef,
) -> Result<Estimates, bosun_store::Error> {
    let (table, rows) = reader.usage(scope.from_ms, scope.to_ms).await?;
    let calls = reader.tool_calls(scope.from_ms, scope.to_ms).await?;
    let in_scope: Vec<&UsageRow> = rows
        .iter()
        .filter(|row| scope.holds_repo(row.repo.as_deref()))
        .collect();
    let priced = price(&table, &in_scope, &calls);
    Ok(Estimates {
        identity: identity.to_owned(),
        from_ms: scope.from_ms,
        to_ms: scope.to_ms,
        repo: scope.repo.map(&repo_ref),
        reference_model: table.reference_model().to_owned(),
        estimate: true,
        bytes_per_token: BYTES_PER_TOKEN,
        formula: FORMULA.to_owned(),
        known_gaps: KNOWN_GAPS.iter().map(|&gap| gap.to_owned()).collect(),
        by_tool: by_tool(&table, &priced),
        by_file: by_file(&table, &priced, &repo_ref),
        repeated_reads: repeated_reads(&table, &priced, &repo_ref),
        finding: finding(&table, &priced),
    })
}

fn by_tool(table: &PriceTable, priced: &[Priced<'_>]) -> ByTool {
    let mut total = Acc::default();
    let mut tools: BTreeMap<&str, (u64, Acc)> = BTreeMap::new();
    for p in priced {
        total.add(p);
        let (errors, acc) = tools.entry(&p.call.name).or_default();
        *errors += u64::from(p.call.is_error);
        acc.add(p);
    }
    let mut rows: Vec<(&str, u64, Acc)> = tools
        .into_iter()
        .map(|(name, (errors, acc))| (name, errors, acc))
        .collect();
    rows.sort_by(|a, b| b.2.cost().cmp(&a.2.cost()).then(a.0.cmp(b.0)));
    ByTool {
        estimate: true,
        formula: BY_TOOL.to_owned(),
        total: total.finish(table),
        rows: rows
            .into_iter()
            .map(|(name, errors, acc)| ToolSummary {
                name: name.to_owned(),
                errors,
                results: acc.finish(table),
            })
            .collect(),
    }
}

/// A file: the repository path key of the calling message and the stored path.
type FileKey<'a> = (Option<&'a str>, &'a str);

/// The distinct paths `p` touched, with the kinds of each.
fn files<'a>(p: &Priced<'a>) -> BTreeMap<FileKey<'a>, BTreeSet<TouchKind>> {
    let mut out: BTreeMap<FileKey<'a>, BTreeSet<TouchKind>> = BTreeMap::new();
    for (path, kind) in &p.call.touches {
        out.entry((p.message.repo.as_deref(), path.as_str()))
            .or_default()
            .insert(*kind);
    }
    out
}

/// Sorts `rows` by `cost` descending, then by file.
fn by_cost_then_file<T>(rows: &mut [(FileKey<'_>, T)], cost: impl Fn(&T) -> Cost) {
    rows.sort_by(|a, b| cost(&b.1).cmp(&cost(&a.1)).then(a.0.cmp(&b.0)));
}

fn by_file(
    table: &PriceTable,
    priced: &[Priced<'_>],
    repo_ref: &impl Fn(&str) -> RepoRef,
) -> ByFile {
    #[derive(Default)]
    struct File<'a> {
        kinds: [u64; 5],
        sessions: BTreeSet<(&'a str, &'a str)>,
        acc: Acc,
    }
    let mut by_path: BTreeMap<FileKey<'_>, File<'_>> = BTreeMap::new();
    for p in priced {
        for (key, kinds) in files(p) {
            let file = by_path.entry(key).or_default();
            for kind in kinds {
                file.kinds[match kind {
                    TouchKind::Read => 0,
                    TouchKind::Search => 1,
                    TouchKind::Write => 2,
                    TouchKind::Exec => 3,
                    TouchKind::Unknown => 4,
                }] += 1;
            }
            file.sessions.insert(p.root());
            file.acc.add(p);
        }
    }
    let mut rows: Vec<_> = by_path.into_iter().collect();
    by_cost_then_file(&mut rows, |file| file.acc.cost());
    ByFile {
        estimate: true,
        formula: BY_FILE.to_owned(),
        rows: rows
            .into_iter()
            .map(|((repo, path), file)| FileRow {
                repo: repo.map(repo_ref),
                path: path.to_owned(),
                reads: file.kinds[0],
                searches: file.kinds[1],
                writes: file.kinds[2],
                execs: file.kinds[3],
                unknowns: file.kinds[4],
                sessions: file.sessions.len() as u64,
                results: file.acc.finish(table),
            })
            .collect(),
    }
}

fn repeated_reads(
    table: &PriceTable,
    priced: &[Priced<'_>],
    repo_ref: &impl Fn(&str) -> RepoRef,
) -> RepeatedReads {
    #[derive(Default)]
    struct File<'a> {
        reads: u64,
        sessions: BTreeSet<(&'a str, &'a str)>,
        within: Acc,
        across: Acc,
    }
    let mut reads: BTreeMap<FileKey<'_>, File<'_>> = BTreeMap::new();
    // `priced` is in time order, so the first read of a file is its first in the list.
    for p in priced {
        for (key, kinds) in files(p) {
            if !kinds.contains(&TouchKind::Read) {
                continue;
            }
            let file = reads.entry(key).or_default();
            file.reads += 1;
            if file.sessions.contains(&p.root()) {
                file.within.add(p);
            } else {
                if !file.sessions.is_empty() {
                    file.across.add(p);
                }
                file.sessions.insert(p.root());
            }
        }
    }
    let mut rows: Vec<_> = reads.into_iter().filter(|(_, f)| f.reads > 1).collect();
    by_cost_then_file(&mut rows, |file| file.within.cost() + file.across.cost());
    let (mut within, mut across) = (Acc::default(), Acc::default());
    for (_, file) in &rows {
        within.merge(&file.within);
        across.merge(&file.across);
    }
    RepeatedReads {
        estimate: true,
        formula: REPEATED.to_owned(),
        within: within.finish(table),
        across: across.finish(table),
        rows: rows
            .into_iter()
            .map(|((repo, path), file)| RepeatedRead {
                repo: repo.map(repo_ref),
                path: path.to_owned(),
                reads: file.reads,
                sessions: file.sessions.len() as u64,
                within: file.within.finish(table),
                across: file.across.finish(table),
            })
            .collect(),
    }
}

fn finding(table: &PriceTable, priced: &[Priced<'_>]) -> Finding {
    #[derive(Default)]
    struct Session {
        first_write_ms: Option<i64>,
        acc: Acc,
    }
    let mut sessions: BTreeMap<(&str, &str), Session> = BTreeMap::new();
    // `priced` is in time order across the windows of a session.
    for p in priced {
        let session = sessions.entry(p.root()).or_default();
        if session.first_write_ms.is_some() {
            continue;
        }
        if p.touches(TouchKind::Write) {
            session.first_write_ms = Some(p.message.ts_ms);
        } else if p.finds() {
            session.acc.add(p);
        }
    }
    let (mut before, mut without, mut with_write, mut without_write) =
        (Acc::default(), Acc::default(), 0, 0);
    let mut rows = Vec::new();
    for ((_, session), found) in sessions {
        if found.acc.calls == 0 {
            continue;
        }
        if found.first_write_ms.is_some() {
            with_write += 1;
            before.merge(&found.acc);
        } else {
            without_write += 1;
            without.merge(&found.acc);
        }
        rows.push(FindingRow {
            session: session.to_owned(),
            first_write_ms: found.first_write_ms,
            results: found.acc.finish(table),
        });
    }
    Finding {
        estimate: true,
        formula: FINDING.to_owned(),
        sessions_with_write: with_write,
        before_write: before.finish(table),
        sessions_without_write: without_write,
        without_write: without.finish(table),
        rows,
    }
}

#[cfg(test)]
mod tests;
