//! The metrics over a synthetic corpus built by hand, without a store.
//!
//! Every message is of `claude-opus-5-5`, the default table's reference model, so a message's
//! eqv is `input + 1.25 * cache_write_5m + 2 * cache_write_1h + 0.05 * cache_read + 5 *
//! output` tokens. The golden file `tests/golden/heat-metrics.json` is the `heat.metrics` of
//! the corpus; run with `BOSUN_BLESS=1` to rewrite it after a deliberate change, and read the
//! diff.

use std::path::{Path, PathBuf};

use bosun_api::RepoRef;
use bosun_core::{Commit, Item, ItemId, Outcome, PriceTable, SpawnMode, State, Tokens, TouchKind};
use bosun_store::{
    ItemHistory, RunCommitRow, RunRow, StoredItem, ToolRow, TransitionRow, UsageRow,
};

use super::*;

/// The start of the corpus's range.
pub(crate) const T: i64 = 1_000_000_000_000;
/// The end of the corpus's range: one day.
pub(crate) const TO: i64 = T + 86_400_000;
pub(crate) const ALPHA: &str = "/demo/alpha";

/// The corpus's range over every repository.
pub(crate) fn scope() -> Scope<'static> {
    Scope {
        from_ms: T,
        to_ms: TO,
        repo: None,
    }
}

/// A repository's reference as the configuration names `/demo/<name>`.
pub(crate) fn repo_ref(key: &str) -> RepoRef {
    RepoRef {
        name: key.strip_prefix("/demo/").map(str::to_owned),
        path: key.to_owned(),
    }
}

/// `(input, cache_write_5m, cache_read, output)`; no 1-hour writes.
pub(crate) fn tokens(input: u64, write: u64, read: u64, output: u64) -> Tokens {
    Tokens {
        input,
        cache_write_5m: write,
        cache_write_1h: 0,
        cache_read: read,
        output,
    }
}

/// A message of `claude-opus-5-5`, its id `<session>-<ts_ms - T>`.
pub(crate) fn message(
    provider: &str,
    session: &str,
    agent: Option<&str>,
    kind: &str,
    repo: Option<&str>,
    ts_ms: i64,
    tokens: Tokens,
) -> UsageRow {
    UsageRow {
        provider: provider.to_owned(),
        message: format!("{session}-{}", ts_ms - T),
        session: session.to_owned(),
        agent: agent.map(str::to_owned),
        kind: kind.to_owned(),
        model: "claude-opus-5-5".to_owned(),
        ts_ms,
        repo: repo.map(str::to_owned),
        tokens,
    }
}

/// A tool call of `message` (a [`UsageRow::message`]).
pub(crate) fn call(
    provider: &str,
    message: &str,
    name: &str,
    result_bytes: u64,
    is_error: bool,
    touches: &[(&str, TouchKind)],
) -> ToolRow {
    ToolRow {
        provider: provider.to_owned(),
        message: message.to_owned(),
        tool_use: format!("{message}-{name}-{result_bytes}"),
        name: name.to_owned(),
        result_bytes,
        is_error,
        touches: touches
            .iter()
            .map(|(path, kind)| ((*path).to_owned(), *kind))
            .collect(),
    }
}

pub(crate) fn transition(
    id: &str,
    seq: u32,
    from: &str,
    to: &str,
    detail: Option<&str>,
    ts_ms: i64,
) -> TransitionRow {
    TransitionRow {
        key: format!("{id}/t{seq}"),
        seq,
        attempt: 1,
        from: from.to_owned(),
        to: to.to_owned(),
        to_detail: detail.map(str::to_owned),
        attempt_from: None,
        attempt_to: None,
        cause: "synthetic".to_owned(),
        ts_ms,
    }
}

/// An item in `Done(outcome)` since `done_ms`, with one session of `provider` and its parks
/// `(reason, from_ms, to_ms)` before it.
pub(crate) fn closed(
    id: &str,
    repo: &str,
    provider: &str,
    outcome: Outcome,
    done_ms: i64,
    parks: &[(&str, i64, i64)],
) -> ItemHistory {
    let mut item = Item::new(ItemId::new(id));
    item.state = State::Done(outcome);
    let detail = match outcome {
        Outcome::Pass => "pass",
        Outcome::Flaky => "flaky",
        Outcome::UserClosed => "user_closed",
    };
    let mut transitions = vec![transition(id, 1, "ready", "running", None, T)];
    for (reason, from_ms, to_ms) in parks {
        let seq = u32::try_from(transitions.len()).unwrap() + 1;
        transitions.push(transition(
            id,
            seq,
            "running",
            "parked",
            Some(reason),
            *from_ms,
        ));
        transitions.push(transition(id, seq + 1, "parked", "running", None, *to_ms));
    }
    let seq = u32::try_from(transitions.len()).unwrap() + 1;
    transitions.push(transition(
        id,
        seq,
        "running",
        "done",
        Some(detail),
        done_ms,
    ));
    ItemHistory {
        item: StoredItem {
            item,
            repo: repo.to_owned(),
            repo_name: repo.strip_prefix("/demo/").map(str::to_owned),
            card_key: None,
            snapshot_hash: None,
            created_ms: T,
            updated_ms: done_ms,
        },
        detail: Some(detail.to_owned()),
        attempts: Vec::new(),
        runs: vec![RunRow {
            attempt: 1,
            spawn: 1,
            mode: SpawnMode::Fresh,
            provider: provider.to_owned(),
            session_key: format!("{id}-session"),
            started_ms: T,
            ended_ms: Some(done_ms),
            exit: None,
            base_sha: None,
            head_sha: None,
            start_sha: None,
        }],
        transitions,
        flaky_tests: Vec::new(),
        agent_calls: Vec::new(),
        denials: Vec::new(),
        verdicts: Vec::new(),
    }
}

/// A commit `sha` of `repo` listed by the commit range of `provider`'s session `session`.
pub(crate) fn run_commit(
    repo: &str,
    sha: &str,
    item: &str,
    provider: &str,
    session: &str,
) -> RunCommitRow {
    RunCommitRow {
        repo: repo.to_owned(),
        commit: Commit {
            sha: sha.to_owned(),
            ts_ms: T,
            author: "A U Thor <author@example.invalid>".to_owned(),
            lines_added: 1,
            lines_removed: 0,
            trailers: std::collections::BTreeMap::new(),
        },
        item: ItemId::new(item),
        attempt: 1,
        spawn: 1,
        provider: provider.to_owned(),
        session_key: session.to_owned(),
    }
}

pub(crate) fn inputs<'a>(
    table: &'a PriceTable,
    usage: &'a [UsageRow],
    calls: &'a [ToolRow],
    histories: &'a [ItemHistory],
    run_commits: &'a [RunCommitRow],
) -> Inputs<'a> {
    Inputs {
        table,
        usage,
        calls,
        histories,
        run_commits,
        documents: None,
    }
}

/// The rows of one provider.
pub(crate) struct Corpus {
    pub(crate) usage: Vec<UsageRow>,
    pub(crate) calls: Vec<ToolRow>,
    pub(crate) histories: Vec<ItemHistory>,
    pub(crate) run_commits: Vec<RunCommitRow>,
}

impl Corpus {
    fn extend(&mut self, other: Self) {
        self.usage.extend(other.usage);
        self.calls.extend(other.calls);
        self.histories.extend(other.histories);
        self.run_commits.extend(other.run_commits);
    }

    fn metrics(&self, table: &PriceTable) -> Vec<Metric> {
        let inputs = inputs(
            table,
            &self.usage,
            &self.calls,
            &self.histories,
            &self.run_commits,
        );
        metrics(&inputs, scope())
    }
}

/// The claude corpus of `/demo/alpha`. Messages (input, 5 min write, cache read, output;
/// prompt = input + write + read; eqv as in the module doc):
///
/// | message | session | agent | prompt | eqv |
/// | --- | --- | --- | --- | --- |
/// | r1-1000 | r1 run | main | 3000 | 1000 + 2500 + 0 + 500 = 4000 |
/// | r1-2000 | r1 run | main | 3500 | 100 + 500 + 150 + 1000 = 1750 |
/// | r1-3000 | r1 run | a1 | 1500 | 500 + 1250 + 0 + 250 = 2000 |
/// | r2-10000 | r2 run | main | 6000 | 2000 + 5000 + 0 + 500 = 7500 |
/// | r2-11000 | r2 run | main | 6200 | 200 + 0 + 300 + 1500 = 2000 |
/// | i1-20000 | i1 interactive | main | 2400 | |
/// | i1-21000 | i1 interactive | main | 2500 | |
#[allow(clippy::too_many_lines)] // One corpus, by hand.
pub(crate) fn claude() -> Corpus {
    let c = "claude";
    let a = Some(ALPHA);
    let usage = vec![
        message(
            c,
            "r1",
            None,
            "run",
            a,
            T + 1000,
            tokens(1000, 2000, 0, 100),
        ),
        message(
            c,
            "r1",
            None,
            "run",
            a,
            T + 2000,
            tokens(100, 400, 3000, 200),
        ),
        message(
            c,
            "r1",
            Some("a1"),
            "run",
            a,
            T + 3000,
            tokens(500, 1000, 0, 50),
        ),
        message(
            c,
            "r2",
            None,
            "run",
            a,
            T + 10_000,
            tokens(2000, 4000, 0, 100),
        ),
        message(
            c,
            "r2",
            None,
            "run",
            a,
            T + 11_000,
            tokens(200, 0, 6000, 300),
        ),
        message(
            c,
            "i1",
            None,
            "interactive",
            a,
            T + 20_000,
            tokens(800, 1600, 0, 40),
        ),
        message(
            c,
            "i1",
            None,
            "interactive",
            a,
            T + 21_000,
            tokens(100, 0, 2400, 60),
        ),
        // In no repository: counted nowhere.
        message(
            c,
            "r3",
            None,
            "run",
            None,
            T + 30_000,
            tokens(9000, 0, 9000, 900),
        ),
    ];
    let calls = vec![
        // r1: three finding calls before its first write.
        call(
            c,
            "r1-1000",
            "Read",
            1001,
            false,
            &[("src/lib.rs", TouchKind::Read)],
        ),
        call(
            c,
            "r1-1000",
            "Grep",
            202,
            false,
            &[("src", TouchKind::Search)],
        ),
        call(
            c,
            "r1-2000",
            "Read",
            403,
            false,
            &[("src/lib.rs", TouchKind::Read)],
        ),
        call(
            c,
            "r1-2000",
            "Edit",
            50,
            false,
            &[("src/lib.rs", TouchKind::Write)],
        ),
        // After the first write: no finding. A script read, then run, then named.
        call(
            c,
            "r1-3000",
            "Read",
            300,
            false,
            &[("tool/check.py", TouchKind::Read)],
        ),
        call(
            c,
            "r1-3000",
            "Bash",
            120,
            true,
            &[("tool/check.py", TouchKind::Exec)],
        ),
        call(
            c,
            "r1-3000",
            "Bash",
            80,
            false,
            &[("tool/check.py", TouchKind::Unknown)],
        ),
        // r2: no write, so both reads are finding.
        call(
            c,
            "r2-10000",
            "Read",
            2000,
            false,
            &[("src/main.rs", TouchKind::Read)],
        ),
        call(c, "r2-11000", "Bash", 10, false, &[]),
        call(
            c,
            "r2-11000",
            "Read",
            600,
            false,
            &[("src/main.rs", TouchKind::Read)],
        ),
        // i1.
        call(
            c,
            "i1-20000",
            "Read",
            500,
            false,
            &[("src/lib.rs", TouchKind::Read)],
        ),
        call(
            c,
            "i1-21000",
            "Write",
            20,
            false,
            &[("notes.md", TouchKind::Write)],
        ),
        // A call whose message is not in the rows: dropped.
        call(
            c,
            "gone-1",
            "Read",
            7777,
            true,
            &[("src/lib.rs", TouchKind::Read)],
        ),
    ];
    let histories = vec![
        closed("alpha-1", ALPHA, c, Outcome::Flaky, T + 30_000, &[]),
        closed(
            "alpha-2",
            ALPHA,
            c,
            Outcome::Pass,
            T + 1_000_000,
            &[("limit", T + 40_000, T + 940_000)],
        ),
        closed(
            "alpha-3",
            ALPHA,
            c,
            Outcome::UserClosed,
            T + 50_000,
            &[("limit", T + 45_000, T + 48_000)],
        ),
    ];
    let run_commits = vec![
        run_commit(ALPHA, "aaaa", "alpha-1", c, "r1"),
        run_commit(ALPHA, "bbbb", "alpha-2", c, "r2"),
        // The same commit listed by a second range: one commit.
        run_commit(ALPHA, "bbbb", "alpha-2", c, "r2"),
    ];
    Corpus {
        usage,
        calls,
        histories,
        run_commits,
    }
}

/// Sessions, calls, a closed item and a commit of `codex` in the same repository.
fn codex() -> Corpus {
    let x = "codex";
    let a = Some(ALPHA);
    let usage = vec![
        message(x, "x1", None, "run", a, T + 5000, tokens(3000, 0, 0, 400)),
        message(x, "x1", None, "run", a, T + 6000, tokens(500, 0, 3000, 100)),
        message(
            x,
            "y1",
            None,
            "interactive",
            a,
            T + 7000,
            tokens(1000, 0, 0, 10),
        ),
        message(
            x,
            "y1",
            None,
            "interactive",
            a,
            T + 8000,
            tokens(100, 0, 1000, 10),
        ),
    ];
    let calls = vec![
        call(
            x,
            "x1-5000",
            "shell",
            4000,
            true,
            &[("src/lib.rs", TouchKind::Read)],
        ),
        call(x, "x1-6000", "shell", 100, true, &[]),
        call(
            x,
            "x1-6000",
            "apply_patch",
            30,
            false,
            &[("src/lib.rs", TouchKind::Write)],
        ),
        call(
            x,
            "y1-7000",
            "shell",
            40,
            true,
            &[("src/lib.rs", TouchKind::Read)],
        ),
    ];
    Corpus {
        usage,
        calls,
        histories: vec![closed("alpha-4", ALPHA, x, Outcome::Pass, T + 60_000, &[])],
        run_commits: vec![run_commit(ALPHA, "cccc", "alpha-4", x, "x1")],
    }
}

/// `(value, total, sample)` of the metric `id` of `kind`.
fn find<'a>(rows: &'a [Metric], provider: &str, kind: &str, id: &str) -> Option<&'a Metric> {
    rows.iter()
        .find(|m| m.provider == provider && m.kind == Some(kind) && m.id == id)
}

fn numbers(rows: &[Metric], provider: &str, kind: &str, id: &str) -> (u64, u64, u64) {
    let m = find(rows, provider, kind, id).unwrap_or_else(|| panic!("no {kind} {id} in {rows:#?}"));
    (m.value, m.total, m.sample)
}

#[test]
fn every_generic_metric_is_computed_from_the_synthetic_corpus() {
    let table = bosun_core::default_table();
    let rows = claude().metrics(&table);
    let run = |id| numbers(&rows, "claude", "run", id);
    let interactive = |id| numbers(&rows, "claude", "interactive", id);

    // eqv of the run messages: 4000 + 1750 + 2000 + 7500 + 2000 = 17250; commits aaaa, bbbb.
    assert_eq!(run("eqv_per_commit"), (8625, 17250, 2));
    // r1 starts at 3000 (its sub-agent's 1500 is not a start), r2 at 6000; lower median 3000.
    assert_eq!(run("context_at_start"), (3000, 9000, 2));
    // src/lib.rs twice in r1, src/main.rs twice in r2; tool/check.py read once, then run and
    // named, which are not reads: 1 + 1 = 2 over 2 sessions.
    assert_eq!(run("rereads_per_session"), (1, 2, 2));
    // r1: 1001 + 202 + 403 before the edit (the read of tool/check.py after it is not
    // finding); r2: 2000 + 600 without a write. 1606 + 2600 = 4206; 4206 / 2 = 2103.
    assert_eq!(run("finding_bytes_per_session"), (2103, 4206, 2));
    // ceil per call: 251 + 51 + 101 + 500 + 150 = 1053; 1053 / 2 = 526.
    assert_eq!(run("finding_tokens_per_session"), (526, 1053, 2));
    // cache reads 3000 + 6000 = 9000 of prompts 3000 + 3500 + 1500 + 6000 + 6200 = 20200:
    // 9000 * 1_000_000 / 20200 = 445544.
    assert_eq!(run("cache_read_share"), (445_544, 9000, 20_200));
    // One error (the run of tool/check.py) of ten calls: 1 * 1_000_000 / 10.
    assert_eq!(run("tool_error_rate"), (100_000, 1, 10));
    // alpha-1 none, alpha-2 900 s: 900_000 / 2. alpha-3 (closed by the user) is not counted.
    assert_eq!(run("parked_ms_per_item"), (450_000, 900_000, 2));
    // alpha-1 flaky of 2: 1 * 1_000_000 / 2.
    assert_eq!(run("flaky_verdict_rate"), (500_000, 1, 2));

    // i1 starts at 2400.
    assert_eq!(interactive("context_at_start"), (2400, 2400, 1));
    assert_eq!(interactive("rereads_per_session"), (0, 0, 1));
    // The read of 500 bytes before the write.
    assert_eq!(interactive("finding_bytes_per_session"), (500, 500, 1));
    // ceil(500 / 4) = 125.
    assert_eq!(interactive("finding_tokens_per_session"), (125, 125, 1));
    // 2400 of 2400 + 2500 = 4900: 2400 * 1_000_000 / 4900 = 489795.
    assert_eq!(interactive("cache_read_share"), (489_795, 2400, 4900));
    assert_eq!(interactive("tool_error_rate"), (0, 0, 2));

    // Nine run metrics and six interactive ones, and nothing else: no item metric for the
    // interactive kind, no row for the message in no repository.
    assert_eq!(rows.len(), 15, "{rows:#?}");
    assert!(
        rows.iter()
            .all(|m| m.repo == ALPHA && m.provider == "claude")
    );
    for id in ["eqv_per_commit", "parked_ms_per_item", "flaky_verdict_rate"] {
        assert!(find(&rows, "claude", "interactive", id).is_none(), "{id}");
    }
    // Sorted by repository, provider, kind, id.
    let keys: Vec<_> = rows
        .iter()
        .map(|m| (&m.repo, &m.provider, m.kind, m.id))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn a_metric_derived_from_bytes_is_labelled_an_estimate() {
    let table = bosun_core::default_table();
    let rows = claude().metrics(&table);
    for m in &rows {
        if m.id == "finding_tokens_per_session" {
            assert_eq!(m.exactness, Exactness::Estimate, "{m:?}");
            assert_eq!(m.bytes_per_token, Some(4), "{m:?}");
            assert!(!m.runner_comparable, "{m:?}");
        } else {
            assert_eq!(m.exactness, Exactness::Exact, "{m:?}");
            assert_eq!(m.bytes_per_token, None, "{m:?}");
        }
    }
    // The sum of ceil per call (251 + 51 + 101 + 500 + 150 = 1053), not ceil of the sum of
    // the bytes (ceil(4206 / 4) = 1052).
    assert_eq!(
        numbers(&rows, "claude", "run", "finding_tokens_per_session").1,
        1053
    );

    let json = serde_json::to_value(to_api("personal", scope(), rows, repo_ref)).unwrap();
    let metrics = json["metrics"].as_array().unwrap();
    assert_eq!(metrics.len(), 15);
    for m in metrics {
        let estimate = m["id"] == "finding_tokens_per_session";
        assert_eq!(m["estimate"], serde_json::Value::Bool(estimate), "{m}");
        if estimate {
            assert_eq!(m["bytes_per_token"], 4, "{m}");
        } else {
            assert!(m["bytes_per_token"].is_null(), "{m}");
        }
    }
    assert_eq!(json["bytes_per_token"], 4);
}

#[test]
fn metrics_of_two_providers_are_never_summed() {
    let table = bosun_core::default_table();
    let alone = (claude().metrics(&table), codex().metrics(&table));
    let mut both = claude();
    both.extend(codex());
    let rows = both.metrics(&table);

    // Every metric once per provider.
    assert_eq!(rows.len(), alone.0.len() + alone.1.len(), "{rows:#?}");
    for m in &rows {
        let own = if m.provider == "claude" {
            &alone.0
        } else {
            &alone.1
        };
        assert!(m.provider == "claude" || m.provider == "codex", "{m:?}");
        let kind = m.kind.unwrap();
        let expected = find(own, &m.provider, kind, m.id).unwrap_or_else(|| panic!("{m:?}"));
        assert_eq!(m, expected);
        if let (Some(a), Some(b)) = (
            find(&alone.0, "claude", kind, m.id),
            find(&alone.1, "codex", kind, m.id),
        ) {
            assert_ne!(m.sample, a.sample + b.sample, "{m:?}");
        }
    }
    for id in ["eqv_per_commit", "tool_error_rate", "flaky_verdict_rate"] {
        assert!(find(&rows, "codex", "run", id).is_some(), "{id}");
        assert!(find(&rows, "claude", "run", id).is_some(), "{id}");
    }
    // codex's run calls: two errors of three, 2 * 1_000_000 / 3 = 666666.
    assert_eq!(
        numbers(&rows, "codex", "run", "tool_error_rate"),
        (666_666, 2, 3)
    );
    // claude's stays one of ten.
    assert_eq!(
        numbers(&rows, "claude", "run", "tool_error_rate"),
        (100_000, 1, 10)
    );
    // codex's eqv: 3000 + 2000 = 5000, then 500 + 150 + 500 = 1150; one commit, cccc.
    assert_eq!(
        numbers(&rows, "codex", "run", "eqv_per_commit"),
        (6150, 6150, 1)
    );
}

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/heat-metrics.json")
}

#[test]
fn the_metrics_match_the_golden_file() {
    let table = bosun_core::default_table();
    let heat = to_api("personal", scope(), claude().metrics(&table), repo_ref);
    let mut text = serde_json::to_string_pretty(&heat).unwrap();
    text.push('\n');
    if std::env::var_os("BOSUN_BLESS").is_some() {
        std::fs::create_dir_all(golden().parent().unwrap()).unwrap();
        std::fs::write(golden(), &text).unwrap();
    }
    let expected = std::fs::read_to_string(golden())
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        text == expected,
        "the metrics differ from {}; rerun with BOSUN_BLESS=1 and review the diff:\n{text}",
        golden().display()
    );
}
