//! The context detectors over synthetic rows built in memory (the builders of `metrics::tests`),
//! in `/fixture/alpha` (or its Windows twin `c:\fixture\alpha`) over a 14-day window, without a
//! store.

use bosun_core::TouchKind;
use bosun_store::{ToolRow, UsageRow};

use crate::detect::{
    ANY_PROVIDER, Detection, DirectoryEvidence, DocumentEvidence, Evidence, FINDING_BY_DIRECTORY,
    Finding, START_CONTEXT, Thresholds, UNOPENED_DOCUMENTS, detect,
};
use crate::estimate::BYTES_PER_TOKEN;
use crate::exact::Scope;
use crate::metrics::tests::{T, call, inputs, message, tokens};

use TouchKind::{Read, Search, Unknown, Write};

const DAY: i64 = 86_400_000;
const ALPHA: &str = "/fixture/alpha";
const ALPHA_WINDOWS: &str = r"c:\fixture\alpha";

/// Synthetic rows of one repository, each message at its own time.
struct Rows {
    repo: &'static str,
    usage: Vec<UsageRow>,
    calls: Vec<ToolRow>,
    next: i64,
}

impl Rows {
    fn new(repo: &'static str) -> Self {
        Self {
            repo,
            usage: Vec::new(),
            calls: Vec::new(),
            next: 0,
        }
    }

    /// A message of `provider`'s session `session` of `kind` whose prompt is `prompt` tokens;
    /// its id.
    fn message(&mut self, provider: &str, session: &str, kind: &str, prompt: u64) -> String {
        self.next += 1;
        let row = message(
            provider,
            session,
            None,
            kind,
            Some(self.repo),
            T + self.next * 1000,
            tokens(prompt, 0, 0, 10),
        );
        let id = row.message.clone();
        self.usage.push(row);
        id
    }

    /// A call `name` of `bytes` touching `touches`, in its own message.
    fn call(
        &mut self,
        provider: &str,
        session: &str,
        kind: &str,
        (name, bytes): (&str, u64),
        touches: &[(&str, TouchKind)],
    ) {
        let id = self.message(provider, session, kind, 10);
        self.calls
            .push(call(provider, &id, name, bytes, false, touches));
    }

    fn without(mut self, session: &str) -> Self {
        self.usage.retain(|row| row.session != session);
        let kept: Vec<String> = self.usage.iter().map(|r| r.message.clone()).collect();
        self.calls.retain(|c| kept.contains(&c.message));
        self
    }

    fn detect(&self, documents: Option<&[String]>) -> Detection {
        let table = bosun_core::default_table();
        let mut inputs = inputs(&table, &self.usage, &self.calls, &[], &[]);
        inputs.documents = documents;
        let scope = Scope {
            from_ms: T,
            to_ms: T + 14 * DAY,
            repo: Some(self.repo),
        };
        detect(&inputs, scope, &Thresholds::default())
    }
}

fn of<'d>(detection: &'d Detection, id: &str) -> Vec<&'d Finding> {
    detection
        .findings
        .iter()
        .filter(|f| f.detector.id == id)
        .collect()
}

/// C1 (#11 D): ten `claude` run sessions start at 50 000 tokens, ten `codex` ones at 5 000 (the
/// twenty's lower median, 5 000, is under the band) and five interactive `codex` ones at
/// 90 000 (never counted). One finding, `claude`'s; nine sessions are insufficient
/// (`mutant_heat_c1_threshold_inf` puts the band at `u64::MAX`).
#[test]
fn a_heavy_start_context_fires_per_provider() {
    let mut rows = Rows::new(ALPHA);
    for n in 0..10 {
        rows.message("claude", &format!("s{n:02}"), "run", 50_000);
        rows.message("codex", &format!("c{n:02}"), "run", 5_000);
    }
    for n in 0..5 {
        rows.message("codex", &format!("i{n:02}"), "interactive", 90_000);
    }

    let detection = rows.detect(None);
    let c1 = of(&detection, START_CONTEXT.id);
    assert_eq!(c1.len(), 1, "{detection:#?}");
    let f = c1[0];
    assert_eq!(f.provider, "claude");
    assert_eq!(f.repo, ALPHA);
    assert_eq!((f.value, f.threshold, f.target), (50_000, 30_000, 20_000));
    assert_eq!((f.sample, f.sample_unit), (10, "session"));
    assert_eq!((f.subject_kind, f.subject.as_str()), ("repo", "."));
    assert_eq!(f.detector.unit, "tokens");
    assert_eq!(f.detector.remedy, "trim_standing_instructions");
    // Exact: no estimate at all.
    assert_eq!(f.impact_eqv_estimate, None);
    assert!(
        !detection
            .insufficient
            .iter()
            .any(|i| i.detector == START_CONTEXT.id),
        "{detection:#?}"
    );

    let rows = rows.without("s09");
    let detection = rows.detect(None);
    assert_eq!(of(&detection, START_CONTEXT.id), Vec::<&Finding>::new());
    let silent: Vec<(&str, u64)> = detection
        .insufficient
        .iter()
        .filter(|i| i.detector == START_CONTEXT.id)
        .map(|i| (i.provider.as_str(), i.run_sessions))
        .collect();
    assert_eq!(silent, [("claude", 9)], "{detection:#?}");
}

/// Ten run sessions finding, before their first write, in `parse` (two reads, 6000 bytes, and
/// a search, 1000), `docs` (a read, 1000) and with a shell call naming two files (10 000,
/// attributed to none); after the write, reads of `docs` (not finding). `sep` is the
/// repository's separator.
fn finding_scene(repo: &'static str, sep: &str) -> Rows {
    let p = |path: &str| path.replace('/', sep);
    let mut rows = Rows::new(repo);
    for n in 0..10 {
        let s = format!("s{n:02}");
        rows.message("claude", &s, "run", 10);
        rows.call(
            "claude",
            &s,
            "run",
            ("Read", 3000),
            &[(&p("src/parse/a.rs"), Read)],
        );
        rows.call(
            "claude",
            &s,
            "run",
            ("Read", 3000),
            &[(&p("src/parse/b.rs"), Read)],
        );
        rows.call(
            "claude",
            &s,
            "run",
            ("Read", 1000),
            &[(&p("docs/x.md"), Read)],
        );
        rows.call(
            "claude",
            &s,
            "run",
            ("Grep", 1000),
            &[(&p("src/parse"), Search)],
        );
        let two = [(p("src/parse/a.rs"), Read), (p("docs/x.md"), Read)];
        let two: Vec<(&str, TouchKind)> = two.iter().map(|(t, k)| (t.as_str(), *k)).collect();
        rows.call("claude", &s, "run", ("Bash", 10_000), &two);
        rows.call(
            "claude",
            &s,
            "run",
            ("Edit", 10),
            &[(&p("src/parse/a.rs"), Write)],
        );
        rows.call(
            "claude",
            &s,
            "run",
            ("Read", 50_000),
            &[(&p("docs/y.md"), Read)],
        );
    }
    rows
}

/// N1 (#11 D): 70 000 of the 80 000 finding bytes are under `src/parse`: one finding naming it
/// at 875 000 ppm, on a Unix key and on a Windows one, whose parent is cut at `\`
/// (`mutant_heat_n1_threshold_inf` puts the threshold above any share).
#[test]
fn finding_concentrated_in_one_directory_names_it() {
    for (repo, sep, subject, docs) in [
        (ALPHA, "/", "src/parse", "docs"),
        (ALPHA_WINDOWS, "\\", r"src\parse", "docs"),
    ] {
        let detection = finding_scene(repo, sep).detect(None);
        let n1 = of(&detection, FINDING_BY_DIRECTORY.id);
        assert_eq!(n1.len(), 1, "{repo}: {detection:#?}");
        let f = n1[0];
        assert_eq!((f.subject_kind, f.subject.as_str()), ("dir", subject));
        assert_eq!(
            (f.value, f.threshold, f.target),
            (875_000, 400_000, 200_000)
        );
        assert_eq!(f.detector.unit, "ppm");
        assert_eq!(f.detector.remedy, "add_map");
        assert_eq!((f.provider.as_str(), f.sample), ("claude", 10));
        assert_eq!(f.impact_bytes, 70_000);
        assert_eq!(
            f.impact_eqv_estimate,
            Some(70_000_u64.div_ceil(BYTES_PER_TOKEN))
        );
        let Evidence::Directory(DirectoryEvidence {
            directory,
            bytes,
            total,
            sessions_total,
            top,
            ..
        }) = &f.evidence
        else {
            panic!("{f:#?}");
        };
        assert_eq!(directory, subject);
        assert_eq!((*bytes, *total, *sessions_total), (70_000, 80_000, 10));
        assert_eq!(
            top,
            &[(subject.to_owned(), 70_000), (docs.to_owned(), 10_000)]
        );
        let lines = f.evidence.lines();
        assert!(
            lines[0].contains(subject) && lines[0].contains("80000"),
            "{lines:?}"
        );
    }
}

/// The tracked files of the U1 scene, `Guide` spelled `guide`, as `git ls-tree` lists them.
fn tracked(guide: &str) -> Vec<String> {
    let mut out = vec![
        "README.md".to_owned(),
        format!("docs/{guide}.md"),
        "CLAUDE.md".to_owned(),
        "crates/demo/AGENTS.md".to_owned(),
        "src/lib.rs".to_owned(),
    ];
    out.extend((1..=10).map(|n| format!("docs/old-{n:02}.md")));
    out
}

/// Ten run sessions of `repo`, six of `claude` and four of `codex`; an interactive session
/// reads `README.md`, a run session writes `docs/guide.md`, another names `docs/old-01.md` to
/// an unknown effect. `sep` is the repository's separator.
fn documents_scene(repo: &'static str, sep: &str) -> Rows {
    let p = |path: &str| path.replace('/', sep);
    let mut rows = Rows::new(repo);
    for n in 0..6 {
        rows.message("claude", &format!("s{n:02}"), "run", 10);
    }
    for n in 0..4 {
        rows.message("codex", &format!("c{n:02}"), "run", 10);
    }
    rows.call(
        "claude",
        "i00",
        "interactive",
        ("Read", 100),
        &[("README.md", Read)],
    );
    rows.call(
        "claude",
        "s00",
        "run",
        ("Edit", 10),
        &[(&p("docs/guide.md"), Write)],
    );
    rows.call(
        "codex",
        "c01",
        "run",
        ("Bash", 10),
        &[(&p("docs/old-01.md"), Unknown)],
    );
    rows
}

fn old_documents(upto: usize) -> Vec<String> {
    (1..=upto).map(|n| format!("docs/old-{n:02}.md")).collect()
}

/// U1 (#210): ten tracked documents nobody opened make one finding for the repository, over
/// every provider; the instruction files, a file of another suffix, a document read by an
/// interactive session and one written are not listed, and an `Unknown` touch does not open
/// one. Nine are no finding; no listing is `insufficient`; on a Windows key a write stored as
/// `docs\guide.md` opens the tracked `docs/Guide.md` (`mutant_heat_u1_threshold_inf` fires at
/// `u64::MAX` only, `mutant_heat_u1_lists_instruction_files` lists `CLAUDE.md` and `AGENTS.md`).
#[test]
fn a_tracked_document_nobody_opened_is_listed_and_instruction_files_are_not() {
    let documents = tracked("guide");
    let rows = documents_scene(ALPHA, "/");
    let detection = rows.detect(Some(&documents));
    let u1 = of(&detection, UNOPENED_DOCUMENTS.id);
    assert_eq!(u1.len(), 1, "{detection:#?}");
    let f = u1[0];
    assert_eq!(f.provider, ANY_PROVIDER);
    assert_eq!(f.provider, "any");
    assert_eq!(f.repo, ALPHA);
    assert_eq!((f.subject_kind, f.subject.as_str()), ("repo", "."));
    assert_eq!((f.value, f.threshold, f.target), (10, 10, 5));
    assert_eq!((f.sample, f.sample_unit), (10, "session"));
    assert_eq!(f.detector.remedy, "prune_or_link");
    assert_eq!(f.impact_eqv_estimate, None);
    assert_eq!(
        f.evidence,
        Evidence::Documents(DocumentEvidence {
            documents: old_documents(10),
            total: 10,
        })
    );
    assert!(
        !detection
            .insufficient
            .iter()
            .any(|i| i.detector == UNOPENED_DOCUMENTS.id),
        "{detection:#?}"
    );

    // One more opened: nine are not a finding.
    let mut nine = documents_scene(ALPHA, "/");
    nine.call(
        "codex",
        "c02",
        "run",
        ("Read", 10),
        &[("docs/old-10.md", Read)],
    );
    let detection = nine.detect(Some(&documents));
    assert_eq!(
        of(&detection, UNOPENED_DOCUMENTS.id),
        Vec::<&Finding>::new()
    );

    // No listing: insufficient, silent.
    let detection = rows.detect(None);
    assert_eq!(
        of(&detection, UNOPENED_DOCUMENTS.id),
        Vec::<&Finding>::new()
    );
    let silent: Vec<(&str, u64)> = detection
        .insufficient
        .iter()
        .filter(|i| i.detector == UNOPENED_DOCUMENTS.id)
        .map(|i| (i.provider.as_str(), i.run_sessions))
        .collect();
    assert_eq!(silent, [("any", 10)], "{detection:#?}");

    // A Windows key: the write `docs\guide.md` opens the tracked `docs/Guide.md`.
    let documents = tracked("Guide");
    let detection = documents_scene(ALPHA_WINDOWS, "\\").detect(Some(&documents));
    let u1 = of(&detection, UNOPENED_DOCUMENTS.id);
    assert_eq!(u1.len(), 1, "{detection:#?}");
    assert_eq!(u1[0].value, 10);
    assert_eq!(
        u1[0].evidence,
        Evidence::Documents(DocumentEvidence {
            documents: old_documents(10),
            total: 10,
        })
    );

    // Only `*.<suffix>` globs.
    assert_eq!(Thresholds::default().document_suffixes(), Ok(vec!["md"]));
    let other = Thresholds {
        documents: vec!["docs/**/*.md".to_owned()],
        ..Thresholds::default()
    };
    let error = other.check().unwrap_err();
    assert!(error.contains("docs/**/*.md"), "{error}");
}
