//! The file detectors over synthetic rows built in memory (the builders of `metrics::tests`),
//! in `/demo/alpha` over a 14-day window, without a store.

use bosun_core::{Outcome, TouchKind};
use bosun_store::{ItemHistory, ToolRow, UsageRow};

use crate::detect::{
    Detection, Evidence, FILE_READ_ACROSS_RUNS, FILE_REREAD_IN_SESSION, FileEvidence, Finding,
    Insufficient, Thresholds, UNCHANGED_FILE_READ_BYTES, detect,
};
use crate::exact::Scope;
use crate::metrics::tests::{ALPHA, T, call, closed, inputs, message, tokens};

use TouchKind::{Exec, Read, Unknown, Write};

const DAY: i64 = 86_400_000;
/// The end of the window: 14 days after [`T`].
const TO: i64 = T + 14 * DAY;

fn scope() -> Scope<'static> {
    Scope {
        from_ms: T,
        to_ms: TO,
        repo: Some(ALPHA),
    }
}

/// Synthetic rows of `/demo/alpha`, one message per call, each at its own time.
#[derive(Default)]
struct Rows {
    usage: Vec<UsageRow>,
    calls: Vec<ToolRow>,
    histories: Vec<ItemHistory>,
    next: i64,
}

impl Rows {
    /// A message of `provider`'s session `session` of `kind`; its id.
    fn message(&mut self, provider: &str, session: &str, kind: &str) -> String {
        self.next += 1;
        let row = message(
            provider,
            session,
            None,
            kind,
            Some(ALPHA),
            T + self.next * 1000,
            tokens(10, 0, 0, 10),
        );
        let id = row.message.clone();
        self.usage.push(row);
        id
    }

    /// A message of a `claude` run session with no call.
    fn run(&mut self, session: &str) {
        self.message("claude", session, "run");
    }

    /// A call of `bytes` touching `touches`, in its own message.
    fn call(
        &mut self,
        provider: &str,
        session: &str,
        kind: &str,
        bytes: u64,
        touches: &[(&str, TouchKind)],
    ) {
        let id = self.message(provider, session, kind);
        self.calls
            .push(call(provider, &id, "Tool", bytes, false, touches));
    }

    /// A read of `path` of `bytes` in a `claude` run session.
    fn read(&mut self, session: &str, path: &str, bytes: u64) {
        self.call("claude", session, "run", bytes, &[(path, Read)]);
    }

    /// An item of `claude` closed in the window.
    fn closed(&mut self, id: &str) {
        let done = T + DAY + i64::try_from(self.histories.len()).unwrap();
        self.histories
            .push(closed(id, ALPHA, "claude", Outcome::Pass, done, &[]));
    }

    /// The rows without the session `session`.
    fn without(mut self, session: &str) -> Self {
        self.usage.retain(|row| row.session != session);
        let kept: Vec<String> = self.usage.iter().map(|r| r.message.clone()).collect();
        self.calls.retain(|c| kept.contains(&c.message));
        self
    }

    fn detect(&self, thresholds: &Thresholds) -> Detection {
        let table = bosun_core::default_table();
        let inputs = inputs(&table, &self.usage, &self.calls, &self.histories, &[]);
        detect(&inputs, scope(), thresholds)
    }
}

/// The run sessions `s00` to `s09` of `claude`.
fn runs(rows: &mut Rows) {
    for n in 0..10 {
        rows.run(&format!("s{n:02}"));
    }
}

/// Ten run sessions: `docs/guide.md` read in `s00`..`s05` (twice in `s00`), 1000 bytes a read;
/// `src/lib.rs` in three run sessions and three interactive ones.
fn guide_scene(rows: &mut Rows) {
    runs(rows);
    rows.read("s00", "docs/guide.md", 1000);
    for n in 0..6 {
        rows.read(&format!("s{n:02}"), "docs/guide.md", 1000);
    }
    for n in 6..9 {
        rows.read(&format!("s{n:02}"), "src/lib.rs", 500);
        let interactive = format!("i{n:02}");
        rows.call(
            "claude",
            &interactive,
            "interactive",
            500,
            &[("src/lib.rs", Read)],
        );
    }
}

/// Ten run sessions; in `s00` `tool/check.py` runs twelve times, `tool/notes.txt` is named
/// twelve times and `docs/big.md` is read five times; every session runs `tool/check.py` once.
fn script_scene(rows: &mut Rows) {
    runs(rows);
    for _ in 0..12 {
        rows.call("claude", "s00", "run", 300, &[("tool/check.py", Exec)]);
        rows.call("claude", "s00", "run", 200, &[("tool/notes.txt", Unknown)]);
    }
    for n in 0..10 {
        rows.call(
            "claude",
            &format!("s{n:02}"),
            "run",
            300,
            &[("tool/check.py", Exec)],
        );
    }
    for _ in 0..5 {
        rows.read("s00", "docs/big.md", 100);
    }
}

/// Ten run sessions and five closed items: `docs/a.md` read twice (400 000 bytes each),
/// `docs/b.md` twenty times (10 000 each), `src/c.rs` ten times (100 000 each) and written in an
/// interactive session; `cat docs/a.md docs/b.md` of 1 000 000 bytes.
fn bytes_scene(rows: &mut Rows) {
    runs(rows);
    rows.read("s00", "docs/a.md", 400_000);
    rows.read("s01", "docs/a.md", 400_000);
    for n in 0..20 {
        rows.read(&format!("s{:02}", n % 10), "docs/b.md", 10_000);
    }
    for n in 0..10 {
        rows.read(&format!("s{n:02}"), "src/c.rs", 100_000);
    }
    rows.call("claude", "i10", "interactive", 50, &[("src/c.rs", Write)]);
    rows.call(
        "claude",
        "s02",
        "run",
        1_000_000,
        &[("docs/a.md", Read), ("docs/b.md", Read)],
    );
    for n in 1..=5 {
        rows.closed(&format!("item-{n}"));
    }
}

fn of<'a>(detection: &'a Detection, id: &str, provider: &str) -> Vec<&'a Finding> {
    detection
        .findings
        .iter()
        .filter(|f| f.detector.id == id && f.provider == provider)
        .collect()
}

fn subjects(findings: &[&Finding]) -> Vec<String> {
    findings.iter().map(|f| f.subject.clone()).collect()
}

fn sessions(range: std::ops::Range<u32>) -> Vec<String> {
    range.map(|n| format!("s{n:02}")).collect()
}

#[test]
fn a_file_read_in_many_run_sessions_fires_once_with_its_evidence() {
    let mut rows = Rows::default();
    guide_scene(&mut rows);
    let detection = rows.detect(&Thresholds::default());
    let d1 = of(&detection, FILE_READ_ACROSS_RUNS.id, "claude");
    assert_eq!(subjects(&d1), ["docs/guide.md"], "{detection:#?}");
    let finding = d1[0];
    assert_eq!(finding.repo, ALPHA);
    assert_eq!(finding.subject_kind, "file");
    // Six sessions, not seven reads.
    assert_eq!(finding.value, 6);
    assert_eq!(finding.threshold, 5);
    assert_eq!(finding.target, 2);
    assert_eq!((finding.sample, finding.sample_unit), (10, "session"));
    assert_eq!((finding.window_from_ms, finding.window_to_ms), (T, TO));
    assert_eq!(
        finding.evidence,
        Evidence::File(FileEvidence {
            reads: 7,
            result_bytes: 7000,
            sessions: sessions(0..6),
            sessions_total: 6,
        })
    );
    assert_eq!(finding.impact_bytes, 7000);
    let lines = finding.evidence.lines();
    assert!(lines[0].starts_with("7 reads"), "{lines:?}");
    for session in sessions(0..6) {
        assert!(lines.iter().any(|l| l.contains(&session)), "{lines:?}");
    }

    // Another provider's sessions are never counted with claude's.
    for n in 0..10 {
        let session = format!("codex-{n:02}");
        rows.message("codex", &session, "run");
        if n < 4 {
            rows.call("codex", &session, "run", 1000, &[("docs/guide.md", Read)]);
        }
    }
    let both = rows.detect(&Thresholds::default());
    let d1 = of(&both, FILE_READ_ACROSS_RUNS.id, "claude");
    assert_eq!(subjects(&d1), ["docs/guide.md"]);
    assert_eq!((d1[0].value, d1[0].sample), (6, 10));
    assert!(
        both.findings.iter().all(|f| f.provider != "codex"),
        "{both:#?}"
    );
}

#[test]
fn twelve_runs_of_a_script_are_not_rereads() {
    let mut rows = Rows::default();
    script_scene(&mut rows);
    let detection = rows.detect(&Thresholds::default());
    let d2 = of(&detection, FILE_REREAD_IN_SESSION.id, "claude");
    assert_eq!(subjects(&d2), ["docs/big.md"], "{detection:#?}");
    assert_eq!(d2[0].value, 5);
    assert_eq!(
        d2[0].evidence,
        Evidence::File(FileEvidence {
            reads: 5,
            result_bytes: 500,
            sessions: vec!["s00".to_owned()],
            sessions_total: 1,
        })
    );
    for finding in &detection.findings {
        assert!(
            !finding.subject.starts_with("tool/"),
            "an exec or an unknown touch was taken for a read: {finding:#?}"
        );
    }
}

#[test]
fn unchanged_files_are_ranked_by_bytes_not_opens() {
    let mut rows = Rows::default();
    bytes_scene(&mut rows);
    let detection = rows.detect(&Thresholds::default());
    let d3 = of(&detection, UNCHANGED_FILE_READ_BYTES.id, "claude");
    // The bytes of `cat docs/a.md docs/b.md` are in neither value; src/c.rs was written.
    assert_eq!(subjects(&d3), ["docs/a.md", "docs/b.md"], "{detection:#?}");
    assert_eq!(d3[0].value, 160_000);
    assert_eq!(d3[1].value, 40_000);
    assert_eq!((d3[0].sample, d3[0].sample_unit), (5, "closed_item"));
    assert_eq!(d3[0].threshold, 32 * 1024);
    assert_eq!(d3[0].impact_bytes, 800_000);
    assert_eq!(d3[0].impact_eqv_estimate, Some(200_000));
    assert_eq!(d3[1].impact_bytes, 200_000);

    let higher = Thresholds {
        d3_bytes_per_item: 50_000,
        ..Thresholds::default()
    };
    let detection = rows.detect(&higher);
    let d3 = of(&detection, UNCHANGED_FILE_READ_BYTES.id, "claude");
    assert_eq!(subjects(&d3), ["docs/a.md"]);

    // The estimate rounds up: 5 bytes are 2 tokens.
    rows.read("s03", "docs/tiny.md", 5);
    let lowest = Thresholds {
        d3_bytes_per_item: 1,
        ..Thresholds::default()
    };
    let detection = rows.detect(&lowest);
    let d3 = of(&detection, UNCHANGED_FILE_READ_BYTES.id, "claude");
    let tiny = d3.iter().find(|f| f.subject == "docs/tiny.md").unwrap();
    assert_eq!((tiny.value, tiny.impact_bytes), (1, 5));
    assert_eq!(tiny.impact_eqv_estimate, Some(2));
}

#[test]
fn a_window_below_the_minimum_sample_fires_nothing() {
    let mut rows = Rows::default();
    guide_scene(&mut rows);
    script_scene(&mut rows);
    bytes_scene(&mut rows);
    let mut rows = rows.without("s09");
    rows.histories.pop();

    let detection = rows.detect(&Thresholds::default());
    assert!(detection.findings.is_empty(), "{detection:#?}");
    let insufficient = |id: &'static str| Insufficient {
        detector: id,
        repo: ALPHA.to_owned(),
        provider: "claude".to_owned(),
        closed_items: 4,
        run_sessions: 9,
        reason: None,
    };
    // The file detectors first; the loop detectors (`detect::loops`), the context ones
    // (`detect::context`) and the tool ones (`detect::tools`) are silent too.
    assert_eq!(detection.insufficient.len(), 14, "{detection:#?}");
    assert_eq!(
        detection.insufficient[..3],
        [
            insufficient(FILE_READ_ACROSS_RUNS.id),
            insufficient(FILE_REREAD_IN_SESSION.id),
            insufficient(UNCHANGED_FILE_READ_BYTES.id),
        ]
    );

    // A switched-off detector is silent, insufficient or not.
    let off = Thresholds {
        disabled: vec![FILE_REREAD_IN_SESSION.id],
        ..Thresholds::default()
    };
    let detection = rows.detect(&off);
    assert!(
        detection
            .insufficient
            .iter()
            .all(|i| i.detector != FILE_REREAD_IN_SESSION.id)
    );
    assert_eq!(detection.insufficient.len(), 13);

    // One more run session reaches the minimum.
    rows.run("s10");
    let detection = rows.detect(&Thresholds::default());
    let d1 = of(&detection, FILE_READ_ACROSS_RUNS.id, "claude");
    assert!(
        d1.iter().any(|f| f.subject == "docs/guide.md"),
        "{detection:#?}"
    );
    let d2 = of(&detection, FILE_REREAD_IN_SESSION.id, "claude");
    assert!(
        d2.iter().any(|f| f.subject == "docs/big.md"),
        "{detection:#?}"
    );
    // But U1, which has no listing of the documents here.
    assert_eq!(
        detection
            .insufficient
            .iter()
            .map(|i| i.detector)
            .collect::<Vec<_>>(),
        [crate::detect::UNOPENED_DOCUMENTS.id]
    );

    let detection = rows.detect(&off);
    assert_eq!(
        subjects(&of(&detection, FILE_REREAD_IN_SESSION.id, "claude")),
        [""; 0]
    );
    let d1 = subjects(&of(&detection, FILE_READ_ACROSS_RUNS.id, "claude"));
    assert!(d1.contains(&"docs/guide.md".to_owned()), "{d1:?}");
}
