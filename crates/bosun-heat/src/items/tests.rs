//! The loop's sections over a seeded store: items driven through the machine
//! ([`bosun_core::step`]) and recorded with the writer messages the loop uses, run sessions
//! whose transcripts are synthetic files ingested by `bosun-ingest`, and commits.
//!
//! The golden files `tests/golden/items-report.json` and `items-estimates.json` are the results
//! of `report.exact` and `report.estimates` over it. Run with `BOSUN_BLESS=1` to rewrite them
//! after a deliberate change, and read the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bosun_api::{CommitScan, Estimates, Evidence, RepoRef, Report};
use bosun_core::{
    Commit, Ctx, Event, Exit, Guard, Item, ItemId, RunResult, SpawnMode, Stepped, step,
};
use bosun_ingest::{Ingest, Options as IngestOptions, claude};
use bosun_store::{
    AgentCall, AgentCallKind, NewItem, Options, RunCommits, RunEnd, RunStart, Store, Write,
};
use tempfile::TempDir;

use crate::Scope;

/// 2000-01-10T00:00:00Z.
const T0: i64 = 947_462_400_000;
const ROOT: &str = "/fixture/repo-a";
const PROVIDER: &str = "claude";

/// The run sessions and the interactive one.
const S1: &str = "00000000-0000-4000-8000-072000000001";
const S2: &str = "00000000-0000-4000-8000-072000000002";
const S3: &str = "00000000-0000-4000-8000-072000000003";
const I1: &str = "00000000-0000-4000-8000-072000000009";
const AGENT: &str = "a0000000072000001";

fn at(seconds: i64) -> i64 {
    T0 + seconds * 1000
}

fn item_a() -> ItemId {
    ItemId::new("local:demo#1")
}

fn item_b() -> ItemId {
    ItemId::new("local:demo#2")
}

fn sha(n: u32) -> String {
    format!("{n:02x}").repeat(20)
}

/// One synthetic assistant message.
#[derive(Debug, Clone, Copy)]
struct Msg {
    session: &'static str,
    agent: Option<&'static str>,
    n: u32,
    second: i64,
    model: &'static str,
    input: u64,
    write_5m: u64,
    read: u64,
    output: u64,
}

impl Msg {
    fn line(&self) -> String {
        let seconds = self.second;
        let time = format!(
            "2000-01-10T{:02}:{:02}:{:02}.000Z",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        let (sidechain, agent) = match self.agent {
            Some(id) => ("true", format!(r#""agentId":"{id}","#)),
            None => ("false", String::new()),
        };
        format!(
            concat!(
                r#"{{"parentUuid":null,"isSidechain":{sidechain},"userType":"external","#,
                r#""cwd":"{root}","sessionId":"{session}","version":"0.0.0","gitBranch":"main","#,
                r#"{agent}"type":"assistant","uuid":"00000000-0000-4000-8000-172{n:09}","#,
                r#""timestamp":"{time}","entrypoint":"cli","message":{{"id":"msg_072{n:021}","#,
                r#""type":"message","role":"assistant","model":"{model}","content":[{{"type":"text","#,
                r#""text":"Synthetic."}}],"stop_reason":"end_turn","stop_sequence":null,"#,
                r#""usage":{{"input_tokens":{input},"cache_creation_input_tokens":{write},"#,
                r#""cache_read_input_tokens":{read},"cache_creation":{{"ephemeral_5m_input_tokens":{write},"#,
                r#""ephemeral_1h_input_tokens":0}},"output_tokens":{output},"#,
                r#""service_tier":"standard"}}}},"requestId":"req_072{n:021}"}}"#,
                "\n"
            ),
            sidechain = sidechain,
            root = ROOT,
            session = self.session,
            agent = agent,
            n = self.n,
            time = time,
            model = self.model,
            input = self.input,
            write = self.write_5m,
            read = self.read,
            output = self.output,
        )
    }
}

/// Every message of the corpus: S1 (with a sub-agent) in two spans of attempt 1, S2 of
/// attempt 2, S3 of item B, and the interactive I1 inside S2's span.
fn messages() -> Vec<Msg> {
    let mut out = Vec::new();
    let mut n = 0;
    let mut add = |session, agent, second, model, base: u64| {
        n += 1;
        out.push(Msg {
            session,
            agent,
            n,
            second,
            model,
            input: base,
            write_5m: base * 10,
            read: base * 100,
            output: base * 3,
        });
    };
    for s in [2, 3, 4] {
        add(S1, None, s, "claude-opus-5-5", 5);
    }
    add(S1, Some(AGENT), 5, "claude-sonnet-5-5", 7);
    add(S1, Some(AGENT), 6, "claude-sonnet-5-5", 8);
    for s in [1001, 1002] {
        add(S1, None, s, "claude-opus-5-5", 11);
    }
    for s in [2001, 2002, 2003] {
        add(S2, None, s, "claude-sonnet-5-5", 13);
    }
    for s in [3002, 3003] {
        add(S3, None, s, "claude-opus-5-5", 17);
    }
    for s in [2020, 2040] {
        add(I1, None, s, "claude-opus-5-5", 19);
    }
    out
}

fn write_transcripts(projects: &Path) {
    let dir = projects.join("-fixture-repo-a");
    std::fs::create_dir_all(&dir).unwrap();
    let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
    for m in messages() {
        let path = match m.agent {
            None => dir.join(format!("{}.jsonl", m.session)),
            Some(agent) => dir
                .join(m.session)
                .join("subagents")
                .join(format!("agent-{agent}.jsonl")),
        };
        files.entry(path).or_default().push_str(&m.line());
    }
    for (path, text) in files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let meta = dir
        .join(S1)
        .join("subagents")
        .join(format!("agent-{AGENT}.meta.json"));
    std::fs::write(
        meta,
        r#"{"agentType":"general-purpose","description":"Synthetic task","model":"claude-sonnet-5-5","requestNonInteractive":false,"spawnDepth":1,"toolUseId":"toolu_000000000000000072000001","isFork":false}"#,
    )
    .unwrap();
}

/// An item as it goes through the machine, with its writes.
struct Machine {
    item: Item,
    writes: Vec<Write>,
}

impl Machine {
    fn new(id: ItemId) -> Self {
        let writes = vec![Write::Item(NewItem {
            item: id.clone(),
            repo: ROOT.to_owned(),
            card_key: None,
            at_ms: T0,
        })];
        Self {
            item: Item::new(id),
            writes,
        }
    }

    fn go(&mut self, second: i64, event: &Event) -> Stepped {
        let stepped = step(&self.item, event, &Ctx::at(at(second)))
            .unwrap_or_else(|| panic!("{event:?} ignored in {:?}", self.item.state));
        self.writes.push(Write::Transition {
            stepped: stepped.clone(),
            at_ms: at(second),
        });
        self.item = stepped.item.clone();
        stepped
    }

    fn attempt(&self) -> (u32, u32, u32) {
        let a = self.item.attempt.as_ref().unwrap();
        (a.number, a.spawn, a.run)
    }

    fn spawned(&mut self, second: i64, mode: SpawnMode, session: &str) {
        let (attempt, spawn, _) = self.attempt();
        self.writes.push(Write::Run(RunStart {
            item: self.item.id.clone(),
            attempt,
            spawn,
            mode,
            provider: PROVIDER.to_owned(),
            session_key: session.to_owned(),
            started_ms: at(second),
        }));
    }

    /// The current session exits at `second` with its commit range.
    fn exit(&mut self, second: i64, exit: Exit, range: (u32, u32), commits: &[u32]) {
        let (attempt, spawn, _) = self.attempt();
        let id = self.item.id.clone();
        self.writes.push(Write::RunEnded(RunEnd {
            item: id.clone(),
            attempt,
            spawn,
            exit,
            head_sha: Some(sha(range.1)),
            ended_ms: at(second),
        }));
        self.writes.push(Write::RunCommits(RunCommits {
            item: id,
            attempt,
            spawn,
            base_sha: sha(range.0),
            head_sha: sha(range.1),
            commits: commits.iter().map(|&n| sha(n)).collect(),
        }));
        self.go(
            second,
            &Event::Exited {
                attempt,
                spawn,
                exit,
                text: String::new(),
            },
        );
    }

    fn verdict(&mut self, second: i64, result: RunResult) {
        let (attempt, _, run) = self.attempt();
        self.go(
            second,
            &Event::Verdict {
                attempt,
                run,
                result,
            },
        );
    }

    fn preconditions(&mut self, second: i64) {
        let attempt = self.attempt().0;
        self.go(
            second,
            &Event::Preconditions {
                attempt,
                passed: true,
            },
        );
    }
}

fn commit(n: u32, second: i64, trailers: &[(&str, &str)]) -> Write {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in trailers {
        map.entry((*name).to_owned())
            .or_default()
            .push((*value).to_owned());
    }
    Write::Commit {
        repo: ROOT.to_owned(),
        commit: Commit {
            sha: sha(n),
            ts_ms: at(second),
            author: "A U Thor <author@example.invalid>".to_owned(),
            lines_added: 1,
            lines_removed: 0,
            trailers: map,
        },
    }
}

fn trailers<'a>(item: &'a str, session: &'a str) -> [(&'a str, &'a str); 3] {
    [("Item", item), ("Run", "1"), ("Session", session)]
}

/// The writes of the two items.
///
/// Item A: attempt 1 runs S1, parks on a limit from 100 s to 1000 s, resumes S1, then fails
/// its verdict twice; attempt 2 runs S2 and its verdict is flaky; landed. Commits 1 and 2 are
/// attempt 1's (one per span of S1), 3 is attempt 2's. Item B runs S3, whose commit 4 has no
/// `Run:` or `Session:` trailer, and is refused for touching a protected path. Commit 5 is
/// made in the interactive session I1, inside S2's time span, and names item A.
fn seed() -> Vec<Write> {
    let mut a = Machine::new(item_a());
    a.go(0, &Event::Claim);
    a.go(1, &Event::ClaimConfirmed);
    a.spawned(1, SpawnMode::Fresh, S1);
    a.exit(
        100,
        Exit::Limit {
            reset_ms: Some(at(1000)),
        },
        (0, 1),
        &[1],
    );
    a.go(1000, &Event::Tick);
    a.spawned(1000, SpawnMode::Resume, S1);
    a.exit(1100, Exit::Ok, (1, 2), &[2]);
    a.preconditions(1101);
    a.verdict(1110, RunResult::Fail);
    a.verdict(1120, RunResult::Fail);
    assert_eq!(
        a.attempt().0,
        2,
        "two fails are one failed attempt; a new one runs"
    );
    a.spawned(2000, SpawnMode::Fresh, S2);
    a.writes.push(Write::AgentCall(AgentCall {
        item: item_a(),
        attempt: 2,
        kind: AgentCallKind::Note,
        body: "synthetic".to_owned(),
        head_sha: None,
        refused: true,
        at_ms: at(2005),
    }));
    a.exit(2100, Exit::Ok, (2, 3), &[3]);
    a.preconditions(2101);
    a.verdict(2110, RunResult::Fail);
    a.verdict(2120, RunResult::Pass);
    a.writes.push(Write::FlakyTests {
        item: item_a(),
        attempt: 2,
        names: vec!["tests::shared_b".to_owned(), "tests::shared_a".to_owned()],
        at_ms: at(2120),
    });
    a.go(2130, &Event::Landed);

    let mut b = Machine::new(item_b());
    b.go(3000, &Event::Claim);
    b.go(3001, &Event::ClaimConfirmed);
    b.spawned(3001, SpawnMode::Fresh, S3);
    b.exit(3100, Exit::Ok, (3, 4), &[4]);
    let attempt = b.attempt().0;
    b.go(
        3101,
        &Event::Refused {
            attempt,
            guard: Guard::ProtectedPath,
        },
    );

    let mut writes = vec![
        commit(1, 50, &trailers("local:demo#1", S1)),
        commit(2, 1050, &trailers("local:demo#1", S1)),
        commit(3, 2050, &trailers("local:demo#1", S2)),
        commit(4, 3050, &[("Item", "local:demo#2")]),
        commit(5, 2030, &[("Item", "local:demo#1")]),
    ];
    writes.extend(a.writes);
    writes.extend(b.writes);
    writes
}

struct Seeded {
    _dir: TempDir,
    store: Store,
}

impl Seeded {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        write_transcripts(&projects);
        let store = Store::open(dir.path().join("bosun.db"), Options::default())
            .await
            .unwrap();
        // Half the run rows before the transcripts are ingested, half after: a run session is
        // one either way.
        let writes = seed();
        store.writer().write(writes).await.unwrap();
        let ingest = Ingest::new(claude::Parser, projects, &store, IngestOptions::default());
        ingest.register_repos(&[ROOT.to_owned()]).await.unwrap();
        let report = ingest.scan().await.unwrap();
        assert_eq!(report.failed, []);
        Self { _dir: dir, store }
    }

    fn scope() -> Scope<'static> {
        Scope {
            from_ms: T0,
            to_ms: T0 + 86_400_000,
            repo: None,
        }
    }

    async fn report(&self) -> Report {
        crate::exact(
            self.store.reader(),
            "personal",
            Self::scope(),
            name,
            Vec::<CommitScan>::new(),
        )
        .await
        .unwrap()
    }

    async fn estimates(&self) -> Estimates {
        crate::estimates(self.store.reader(), "personal", Self::scope(), name)
            .await
            .unwrap()
    }
}

fn name(key: &str) -> RepoRef {
    assert_eq!(key, ROOT, "an unexpected repository key");
    RepoRef {
        name: Some("a".to_owned()),
        path: key.to_owned(),
    }
}

/// The raw sums of the generated messages of `sessions`, computed from the generator alone:
/// (messages, input, cache write, cache read, output).
fn expected(sessions: &[&str]) -> (u64, u64, u64, u64, u64) {
    let mut out = (0, 0, 0, 0, 0);
    for m in messages().iter().filter(|m| sessions.contains(&m.session)) {
        out.0 += 1;
        out.1 += m.input;
        out.2 += m.write_5m;
        out.3 += m.read;
        out.4 += m.output;
    }
    out
}

fn raw(sums: &bosun_api::TokenSums) -> (u64, u64, u64, u64, u64) {
    (
        sums.messages,
        sums.input,
        sums.cache_write_5m + sums.cache_write_1h,
        sums.cache_read,
        sums.output,
    )
}

#[tokio::test]
async fn an_items_sums_are_its_run_sessions_messages() {
    let seeded = Seeded::new().await;
    let report = seeded.report().await;
    let rows = &report.items.rows;
    assert_eq!(
        rows.iter().map(|r| r.item.as_str()).collect::<Vec<_>>(),
        ["local:demo#1", "local:demo#2"]
    );
    let a = &rows[0];
    assert_eq!(raw(&a.runs), expected(&[S1, S2]), "every attempt is billed");
    assert_eq!(a.sessions, 2, "a resume keeps its session");
    assert_eq!(a.attempts.len(), 2);
    assert_eq!(raw(&a.attempts[0].sums), expected(&[S1]));
    assert_eq!(raw(&a.attempts[1].sums), expected(&[S2]));
    // The sub-agent's messages are its session's, and its model is listed.
    let models: Vec<&str> = a.attempts[0]
        .models
        .iter()
        .map(|m| m.model.as_str())
        .collect();
    assert_eq!(models, ["claude-opus-5-5", "claude-sonnet-5-5"]);
    let spawns: Vec<(u32, &str, u64)> = a.attempts[0]
        .sessions
        .iter()
        .map(|s| (s.spawn, s.mode.as_str(), s.sums.messages))
        .collect();
    assert_eq!(
        spawns,
        [(1, "fresh", expected(&[S1]).0), (2, "resume", 0)],
        "a resumed session counts once, under its first spawn"
    );
    assert_eq!(a.attempts[1].refused_calls, 1);
    // The machine goes from the second failing verdict run straight to attempt 2, so the
    // stored state of attempt 1 is the last one recorded for it; it has ended.
    assert_eq!(a.attempts[0].ended_ms, Some(at(1120)));
    assert_eq!(a.attempts[0].wall_clock_ms, Some(1_119_000));
    assert_eq!(a.attempts[1].state, "flaky");
    assert_eq!(a.attempts[1].wall_clock_ms, Some(1_000_000));
    assert_eq!(a.state, "done");
    assert_eq!(a.detail.as_deref(), Some("flaky"));
    assert_eq!(raw(&rows[1].runs), expected(&[S3]));
    assert_eq!(
        report.items.retried,
        [bosun_api::RetriedItem {
            item: "local:demo#1".to_owned(),
            attempts: 2,
            failed: 1,
        }]
    );
    seeded.store.close().await.unwrap();
}

#[tokio::test]
async fn every_commit_of_the_runs_maps_to_one_session_and_item() {
    let seeded = Seeded::new().await;
    let report = seeded.report().await;
    let links: Vec<(String, &str, u32, &str)> = report
        .items
        .commits
        .iter()
        .map(|c| {
            (
                c.sha.clone(),
                c.item.as_str(),
                c.attempt,
                c.session.as_str(),
            )
        })
        .collect();
    assert_eq!(
        links,
        [
            (sha(1), "local:demo#1", 1, S1),
            (sha(2), "local:demo#1", 1, S1),
            (sha(3), "local:demo#1", 2, S2),
            (sha(4), "local:demo#2", 1, S3),
        ],
        "each commit of a run's range, once; the interactive commit made inside S2's span is \
         not S2's"
    );
    assert_eq!(report.items.outside_runs, 1);
    let missing: Vec<(String, Vec<String>)> = report
        .items
        .trailer_missing
        .iter()
        .map(|t| (t.sha.clone(), t.missing.clone()))
        .collect();
    assert_eq!(
        missing,
        [(sha(4), vec!["Run".to_owned(), "Session".to_owned()])]
    );
    assert!(report.items.commits[0].trailers.item && report.items.commits[0].trailers.session);
    seeded.store.close().await.unwrap();
}

#[tokio::test]
async fn flaky_verdicts_refusals_and_parked_time_are_listed() {
    let seeded = Seeded::new().await;
    let items = seeded.report().await.items;
    assert_eq!(
        items.flaky,
        [bosun_api::FlakyVerdict {
            item: "local:demo#1".to_owned(),
            attempt: 2,
            tests: vec!["tests::shared_a".to_owned(), "tests::shared_b".to_owned()],
        }]
    );
    assert_eq!(
        items.refused_edits,
        [bosun_api::RefusedEdit {
            item: "local:demo#2".to_owned(),
            attempt: 1,
            ts_ms: at(3101),
        }]
    );
    let parked = bosun_api::ParkedTime {
        reason: "limit".to_owned(),
        parks: 1,
        ms: 900_000,
        ongoing: false,
    };
    assert_eq!(items.rows[0].parked, std::slice::from_ref(&parked));
    assert_eq!(items.rows[1].parked, []);
    assert_eq!(items.parked, [parked]);
    assert_eq!(
        items.rows[1].detail.as_deref(),
        Some("guard_protected_path")
    );
    seeded.store.close().await.unwrap();
}

#[tokio::test]
async fn an_inferred_session_is_an_estimate_only() {
    let seeded = Seeded::new().await;
    let report = seeded.report().await;
    let estimates = seeded.estimates().await;

    // The exact side: the item's runs only.
    let a = &report.items.rows[0];
    assert_eq!(raw(&a.runs), expected(&[S1, S2]));
    assert!(
        a.attempts
            .iter()
            .flat_map(|t| &t.sessions)
            .all(|s| s.session != I1),
        "the interactive session is no run of the item"
    );

    // The estimate side: I1, by the commit that names the item; S2, whose span also holds
    // that commit, is a run and is never inferred.
    let inferred = &estimates.items;
    assert!(inferred.estimate);
    assert_eq!(inferred.rows.len(), 1);
    let row = &inferred.rows[0];
    assert_eq!(row.item, "local:demo#1");
    let sessions: Vec<&str> = row.sessions.iter().map(|s| s.session.as_str()).collect();
    assert_eq!(sessions, [I1]);
    assert_eq!(
        row.sessions[0].evidence,
        [Evidence::CommitTrailer { sha: sha(5) }]
    );
    assert_eq!(raw(&row.sums), expected(&[I1]));
    seeded.store.close().await.unwrap();
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn check_golden(name: &str, text: &str) {
    let path = golden(name);
    if std::env::var_os("BOSUN_BLESS").is_some() {
        std::fs::write(&path, text).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        text == expected,
        "the result differs from {}; rerun with BOSUN_BLESS=1 and review the diff:\n{text}",
        path.display()
    );
}

#[tokio::test]
async fn the_seeded_report_matches_the_golden_files() {
    let seeded = Seeded::new().await;
    let report = seeded.report().await;
    let estimates = seeded.estimates().await;
    check_golden(
        "items-report.json",
        &(serde_json::to_string_pretty(&report).unwrap() + "\n"),
    );
    check_golden(
        "items-estimates.json",
        &(serde_json::to_string_pretty(&estimates).unwrap() + "\n"),
    );
    seeded.store.close().await.unwrap();
}
