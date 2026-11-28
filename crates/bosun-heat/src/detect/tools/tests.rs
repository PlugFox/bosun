//! The tool detectors over synthetic messages and calls built in memory (the builders of
//! `metrics::tests`), in `/fixture/alpha` of `claude` over a 14-day window, without a store:
//! every call has a message of its own, and every call is of parser version 4 unless a test
//! says otherwise.

use bosun_store::{ToolRow, UsageRow};

use crate::detect::{
    Detection, Detector, Evidence, Finding, MEDIA_SHARE, NO_TOOL_FIGURES, REINGEST_NEEDED,
    REPEATED_COMMAND, TOOL_WALL_TIME, Thresholds, WallEvidence, detect,
};
use crate::exact::Scope;
use crate::metrics::tests::{Stored, T, call, inputs, message, stored, tokens};

const DAY: i64 = 86_400_000;
const ALPHA: &str = "/fixture/alpha";
const CHECK: &str = "python tool/check.py --all";

fn scope() -> Scope<'static> {
    Scope {
        from_ms: T,
        to_ms: T + 14 * DAY,
        repo: Some(ALPHA),
    }
}

/// The rows of a scene.
#[derive(Default)]
struct Scene {
    usage: Vec<UsageRow>,
    calls: Vec<ToolRow>,
    /// `None` for `claude`.
    provider: Option<&'static str>,
}

impl Scene {
    /// The run sessions `s01` to `s10`, one message each.
    fn ten() -> Self {
        let mut scene = Self::default();
        for n in 1..=10 {
            scene.message(&session(n), "run");
        }
        scene
    }

    fn provider(&self) -> &'static str {
        self.provider.unwrap_or("claude")
    }

    /// A message of `session` of `kind`, at a time of its own; its id.
    fn message(&mut self, session: &str, kind: &str) -> String {
        let ts = T + DAY + i64::try_from(self.usage.len()).unwrap();
        let row = message(
            self.provider(),
            session,
            None,
            kind,
            Some(ALPHA),
            ts,
            tokens(0, 0, 0, 0),
        );
        let id = row.message.clone();
        self.usage.push(row);
        id
    }

    /// A call of `name` with `result_bytes` and `fields`, in a message of its own.
    fn call(
        &mut self,
        session: &str,
        kind: &str,
        (name, result_bytes): (&str, u64),
        fields: Stored<'_>,
    ) {
        let message = self.message(session, kind);
        let tool_use = format!("{message}-call");
        self.calls.push(stored(
            call(self.provider(), &message, name, result_bytes, false, &[]),
            &tool_use,
            fields,
        ));
    }

    /// `times` runs of the command `hash` (`text`) in `session`.
    fn command(&mut self, session: &str, kind: &str, (hash, text): (&str, &str), times: u32) {
        for _ in 0..times {
            let fields = Stored {
                target: text.split(' ').next(),
                command: Some((hash, text)),
                ..Stored::default()
            };
            self.call(session, kind, ("Bash", 100), fields);
        }
    }

    /// A timed call of `name` (`target` its command head) in `session`, from `from` ms after
    /// the window's second day for `wall` ms.
    fn timed(&mut self, session: &str, (name, target): (&str, Option<&str>), from: i64, wall: i64) {
        let at = T + DAY + from;
        let fields = Stored {
            target,
            times: (Some(at), Some(at + wall)),
            media_bytes: Some(0),
            ..Stored::default()
        };
        self.call(session, "run", (name, 0), fields);
    }

    fn detect(&self) -> Detection {
        let table = bosun_core::default_table();
        let inputs = inputs(&table, &self.usage, &self.calls, &[], &[]);
        detect(&inputs, scope(), &Thresholds::default())
    }
}

fn session(n: u32) -> String {
    format!("s{n:02}")
}

fn of(detection: &Detection, detector: Detector) -> Vec<&Finding> {
    detection
        .findings
        .iter()
        .filter(|f| f.detector == detector)
        .collect()
}

/// X1's scene: `H1` three times in `s01` and twice in `s02`; `H2` twice in each of `s03` to
/// `s06`, and five times in the interactive session `i01`.
fn commands_scene(scene: &mut Scene) {
    scene.command("s01", "run", ("H1", CHECK), 3);
    scene.command("s02", "run", ("H1", CHECK), 2);
    for n in 3..=6 {
        scene.command(&session(n), "run", ("H2", "cargo xtask lint"), 2);
    }
    scene.command("i01", "interactive", ("H2", "cargo xtask lint"), 5);
}

/// T1's scene: twelve `Bash cargo` calls of 90 s (two of `s01` overlap by 30 s) and one whose
/// result is before its call; ten `WebFetch` calls of 70 s; forty `Read` calls of 50 ms.
fn wall_scene(scene: &mut Scene) {
    let cargo = ("Bash", Some("cargo"));
    scene.timed("s01", cargo, 0, 90_000);
    scene.timed("s01", cargo, 60_000, 90_000);
    for n in 0..10 {
        let from = 1_000_000 * i64::from(n + 1);
        scene.timed(&session(n % 10 + 1), cargo, from, 90_000);
        scene.timed(&session(n % 10 + 1), ("WebFetch", None), from, 70_000);
        for k in 0..4 {
            scene.timed(&session(n + 1), ("Read", None), from + 100 * k, 50);
        }
    }
    scene.timed("s02", cargo, 50_000_000, -1_000);
}

/// M1's scene: in each run session a `Read` of 200 kB, `media` bytes of it an image, and a
/// `Bash` result of 200 kB of text.
fn media_scene(scene: &mut Scene, media: u64) {
    for n in 1..=10 {
        let image = Stored {
            media_bytes: Some(media),
            ..Stored::default()
        };
        scene.call(&session(n), "run", ("Read", 200_000), image);
        let text = Stored {
            media_bytes: Some(0),
            ..Stored::default()
        };
        scene.call(&session(n), "run", ("Bash", 200_000), text);
    }
}

#[test]
fn a_command_run_three_times_verbatim_fires() {
    let mut scene = Scene::ten();
    commands_scene(&mut scene);
    let detection = scene.detect();

    let x1 = of(&detection, REPEATED_COMMAND);
    assert_eq!(x1.len(), 1, "{detection:#?}");
    let f = x1[0];
    assert_eq!((f.subject_kind, f.subject.as_str()), ("command", "H1"));
    assert_eq!((f.value, f.threshold, f.target), (3, 3, 1));
    assert_eq!((f.sample, f.sample_unit), (10, "session"));
    assert_eq!((f.repo.as_str(), f.provider.as_str()), (ALPHA, "claude"));
    assert_eq!(f.detector.remedy, "missing_script");
    assert_eq!((f.impact_bytes, f.impact_eqv_estimate), (0, None));
    let lines = f.evidence.lines();
    assert_eq!(lines, [CHECK, "session s01: 3 calls"], "{lines:?}");
    // Nothing else fires: the commands have no times and no media.
    assert_eq!(detection.findings.len(), 1, "{detection:#?}");
    assert!(
        !detection
            .insufficient
            .iter()
            .any(|i| i.detector == REPEATED_COMMAND.id),
        "{detection:#?}"
    );
}

#[test]
fn a_window_with_rows_of_an_older_parser_fires_nothing() {
    let mut scene = Scene::ten();
    commands_scene(&mut scene);
    wall_scene(&mut scene);
    media_scene(&mut scene, 200_000);
    // Without the old row, all three fire.
    let detection = scene.detect();
    for detector in [REPEATED_COMMAND, TOOL_WALL_TIME, MEDIA_SHARE] {
        assert!(!of(&detection, detector).is_empty(), "{detection:#?}");
    }

    let old = Stored {
        parser_version: Some(3),
        ..Stored::default()
    };
    scene.call("s11", "run", ("Read", 10), old);
    let detection = scene.detect();
    assert_eq!(detection.findings, Vec::<Finding>::new(), "{detection:#?}");
    let silent: Vec<(&str, &str, &str, Option<&str>)> = detection
        .insufficient
        .iter()
        .filter(|i| i.reason.is_some())
        .map(|i| (i.detector, i.repo.as_str(), i.provider.as_str(), i.reason))
        .collect();
    let reingest = Some(REINGEST_NEEDED);
    assert_eq!(
        silent,
        [
            (REPEATED_COMMAND.id, ALPHA, "claude", reingest),
            (TOOL_WALL_TIME.id, ALPHA, "claude", reingest),
            (MEDIA_SHARE.id, ALPHA, "claude", reingest),
        ],
        "{detection:#?}"
    );
    assert!(REINGEST_NEEDED.contains("bosun reingest"));
}

#[test]
fn wall_time_is_ranked_per_tool() {
    let mut scene = Scene::ten();
    wall_scene(&mut scene);
    let detection = scene.detect();

    let t1 = of(&detection, TOOL_WALL_TIME);
    let subjects: Vec<&str> = t1.iter().map(|f| f.subject.as_str()).collect();
    assert_eq!(subjects, ["Bash cargo", "WebFetch"], "{detection:#?}");
    let cargo = t1[0];
    assert_eq!(cargo.subject_kind, "tool_target");
    assert_eq!(
        (cargo.value, cargo.threshold, cargo.target),
        (90_000, 60_000, 30_000)
    );
    assert_eq!(cargo.detector.unit, "ms");
    assert_eq!((cargo.impact_bytes, cargo.impact_eqv_estimate), (0, None));
    let Evidence::Wall(wall) = &cargo.evidence else {
        panic!("{cargo:#?}");
    };
    assert_eq!(
        *wall,
        WallEvidence {
            calls: 12,
            skipped: 1,
            total_ms: 12 * 90_000,
            union_ms: 12 * 90_000 - 30_000,
            parallel_calls: 2,
            // No closed item: per run session.
            total_per: 12 * 90_000 / 10,
            per: "session",
        }
    );
    let lines = cargo.evidence.lines();
    assert!(lines.iter().any(|l| l == "parallel_calls: 2"), "{lines:?}");
    let Evidence::Wall(fetch) = &t1[1].evidence else {
        panic!("{:#?}", t1[1]);
    };
    assert_eq!((t1[1].value, fetch.total_per), (70_000, 70_000));
    assert!(fetch.total_per < wall.total_per);
}

#[test]
fn a_session_heavy_with_images_fires_the_media_share() {
    let mut scene = Scene::ten();
    media_scene(&mut scene, 200_000);
    let detection = scene.detect();

    let m1 = of(&detection, MEDIA_SHARE);
    assert_eq!(m1.len(), 1, "{detection:#?}");
    let f = m1[0];
    assert_eq!((f.subject_kind, f.subject.as_str()), ("repo", "."));
    assert_eq!(
        (f.value, f.threshold, f.target),
        (500_000, 300_000, 100_000)
    );
    assert_eq!(f.detector.remedy, "limit_media");
    assert_eq!((f.impact_bytes, f.impact_eqv_estimate), (2_000_000, None));
    assert_eq!(
        f.evidence.lines(),
        [
            "2000000 media bytes of 4000000 result bytes",
            "tool Read: 2000000 media bytes",
        ]
    );

    // 200_000 media bytes of 4_000_000: not a pattern.
    let mut scene = Scene::ten();
    media_scene(&mut scene, 20_000);
    let detection = scene.detect();
    assert_eq!(of(&detection, MEDIA_SHARE), Vec::<&Finding>::new());
}

/// T1 leaves out `Task` and `Agent`: their wall time is a whole subagent run
/// (`mutant_heat_t1_times_subagents` times them).
#[test]
fn t1_leaves_out_the_subagent_tools() {
    let mut scene = Scene::ten();
    for n in 0..10 {
        let from = 1_000_000 * i64::from(n + 1);
        for tool in ["Task", "Agent"] {
            scene.timed(&session(n + 1), (tool, None), from, 600_000);
        }
    }
    let detection = scene.detect();
    assert_eq!(of(&detection, TOOL_WALL_TIME), Vec::<&Finding>::new());
    // A long tool beside them still fires.
    wall_scene(&mut scene);
    let detection = scene.detect();
    let subjects: Vec<&str> = of(&detection, TOOL_WALL_TIME)
        .iter()
        .map(|f| f.subject.as_str())
        .collect();
    assert_eq!(subjects, ["Bash cargo", "WebFetch"], "{detection:#?}");
}

/// Only `claude`'s parser stores the tool figures: a group of another provider is
/// `insufficient` for X1, T1 and M1 with [`NO_TOOL_FIGURES`], even at parser version 4, and not
/// with the reingest reason (`mutant_heat_tool_gate_any_provider` compares the version only).
#[test]
fn a_provider_without_tool_figures_is_insufficient_with_its_own_reason() {
    let mut scene = Scene {
        provider: Some("codex"),
        ..Scene::default()
    };
    for n in 1..=10 {
        scene.message(&session(n), "run");
    }
    commands_scene(&mut scene);
    wall_scene(&mut scene);
    media_scene(&mut scene, 200_000);
    let detection = scene.detect();
    assert_eq!(detection.findings, Vec::<Finding>::new(), "{detection:#?}");
    let silent: Vec<(&str, &str, Option<&str>)> = detection
        .insufficient
        .iter()
        .filter(|i| [REPEATED_COMMAND.id, TOOL_WALL_TIME.id, MEDIA_SHARE.id].contains(&i.detector))
        .map(|i| (i.detector, i.provider.as_str(), i.reason))
        .collect();
    let none = Some(NO_TOOL_FIGURES);
    assert_eq!(
        silent,
        [
            (REPEATED_COMMAND.id, "codex", none),
            (TOOL_WALL_TIME.id, "codex", none),
            (MEDIA_SHARE.id, "codex", none),
        ],
        "{detection:#?}"
    );
    assert!(!NO_TOOL_FIGURES.contains("reingest"));
}
