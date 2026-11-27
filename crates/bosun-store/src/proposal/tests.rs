use super::*;
use crate::{Options, Repo, Store, Write};

const ALPHA: &str = "c:\\demo\\alpha";
const BETA: &str = "c:\\demo\\beta";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const T0: i64 = 1_700_000_000_000;

fn repo(name: Option<&str>, path: &str) -> Write {
    Write::Repo(Repo {
        name: name.map(str::to_owned),
        path: path.to_owned(),
        remote: None,
        default_branch: None,
    })
}

/// A complete finding of `d1` on `claude` for file `subject` of `repo`.
fn finding(key: &str, repo: &str, subject: &str, at_ms: i64) -> NewProposal {
    NewProposal {
        key: key.to_owned(),
        repo: repo.to_owned(),
        provider: "claude".to_owned(),
        detector: "d1".to_owned(),
        detector_version: 1,
        remedy: "split the file".to_owned(),
        subject_kind: SubjectKind::File,
        subject: subject.to_owned(),
        metric: "reread_bytes".to_owned(),
        unit: "bytes".to_owned(),
        estimate: false,
        baseline_value: Some(40_000),
        threshold: 20_000,
        target_value: Some(10_000),
        window: Some((at_ms - 7 * DAY_MS, at_ms)),
        sample: 12,
        guard: None,
        impact_eqv_estimate: Some(1_500),
        params_json: r#"{"parser_version":3}"#.to_owned(),
        evidence_json: r#"[{"session":"s1"}]"#.to_owned(),
        at_ms,
    }
}

async fn open_store(dir: &tempfile::TempDir) -> Store {
    let store = Store::open(dir.path().join("bosun.db"), Options::default())
        .await
        .unwrap();
    store
        .writer()
        .write(vec![repo(None, ALPHA), repo(None, BETA)])
        .await
        .unwrap();
    store
}

async fn opened(store: &Store, new: NewProposal) -> Result<(), Error> {
    store.writer().write(vec![Write::ProposalOpened(new)]).await
}

async fn stepped(store: &Store, key: &str, step: ProposalStep, at_ms: i64) -> Result<(), Error> {
    store
        .writer()
        .write(vec![Write::ProposalStep {
            key: key.to_owned(),
            step,
            at_ms,
        }])
        .await
}

fn landed(at_ms: i64) -> ProposalStep {
    ProposalStep::Landed {
        item: None,
        sha: Some("abc123".to_owned()),
        how: LandedHow::Marker,
        landed_ms: at_ms,
        followup_due_ms: at_ms + 14 * DAY_MS,
    }
}

fn measured(outcome: FollowupOutcome) -> ProposalStep {
    ProposalStep::Measured {
        after_value: Some(30_000),
        after_sample: 9,
        guard: None,
        params_json: None,
        outcome,
    }
}

fn refused(result: &Result<(), Error>) -> &str {
    match result {
        Err(Error::Refused(why)) => why,
        other => panic!("not refused: {other:?}"),
    }
}

/// #11 D: "A detector does not fire twice for the same file while its card is open"
/// (`mutant_proposal_no_dedup` stores a second active row).
#[tokio::test]
async fn a_second_active_proposal_for_the_same_subject_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir).await;
    let reader = store.reader();
    opened(&store, finding("p1", ALPHA, "docs/guide.md", T0))
        .await
        .unwrap();
    let p1 = reader.proposal("p1").await.unwrap().unwrap();
    assert_eq!(p1.state, ProposalState::Open);
    assert_eq!(p1.seen_count, 1);
    assert_eq!((p1.created_ms, p1.last_seen_ms), (T0, T0));
    assert_eq!(p1.repo, ALPHA);
    assert_eq!(p1.subject_kind, SubjectKind::File);
    assert_eq!(p1.target_value, 10_000);

    let again = opened(&store, finding("p2", ALPHA, "docs\\Guide.md", T0 + 1)).await;
    assert!(refused(&again).contains("p1"), "{again:?}");
    assert_eq!(reader.proposals(Some(ALPHA)).await.unwrap().len(), 1);

    let codex = NewProposal {
        provider: "codex".to_owned(),
        ..finding("p3", ALPHA, "docs/guide.md", T0 + 1)
    };
    opened(&store, codex).await.unwrap();
    opened(&store, finding("p4", BETA, "docs/guide.md", T0 + 1))
        .await
        .unwrap();
    assert_eq!(reader.proposals(Some(ALPHA)).await.unwrap().len(), 2);
    assert_eq!(reader.proposals(Some(BETA)).await.unwrap().len(), 1);
    assert_eq!(reader.proposals(None).await.unwrap().len(), 3);

    store
        .writer()
        .write(vec![Write::ProposalSeen {
            key: "p1".to_owned(),
            at_ms: T0 + 2,
        }])
        .await
        .unwrap();
    let p1 = reader.proposal("p1").await.unwrap().unwrap();
    assert_eq!((p1.seen_count, p1.last_seen_ms), (2, T0 + 2));

    let published = ProposalStep::Published {
        card_key: "card-1".to_owned(),
        issue_number: Some(7),
    };
    stepped(&store, "p1", published, T0 + 3).await.unwrap();
    let again = opened(&store, finding("p5", ALPHA, "docs/guide.md", T0 + 4)).await;
    assert!(refused(&again).contains("p1"), "{again:?}");
    stepped(&store, "p1", landed(T0 + 5), T0 + 5).await.unwrap();
    let again = opened(&store, finding("p6", ALPHA, "docs/guide.md", T0 + 6)).await;
    assert!(refused(&again).contains("p1"), "{again:?}");
    let p1 = reader.proposal("p1").await.unwrap().unwrap();
    assert_eq!(p1.state, ProposalState::Landed);
    assert_eq!(p1.card_key.as_deref(), Some("card-1"));
    assert_eq!((p1.issue_number, p1.published_ms), (Some(7), Some(T0 + 3)));
    assert_eq!(p1.landed_how, Some(LandedHow::Marker));

    stepped(&store, "p1", measured(FollowupOutcome::Moved), T0 + 7)
        .await
        .unwrap();
    opened(&store, finding("p7", ALPHA, "docs/guide.md", T0 + 8))
        .await
        .unwrap();
    let p1 = reader.proposal("p1").await.unwrap().unwrap();
    assert_eq!(p1.state, ProposalState::Measured);
    assert_eq!(p1.outcome, Some(FollowupOutcome::Moved));
    assert_eq!(reader.proposals(Some(ALPHA)).await.unwrap().len(), 3);

    let unknown = stepped(&store, "nope", ProposalStep::Expired, T0 + 9).await;
    assert!(matches!(unknown, Err(Error::UnknownRow(_))), "{unknown:?}");
    store.close().await.unwrap();
}

/// #11 D: "Every proposal card contains a metric, a current value, a target and a window; a
/// card without them is not created" (`mutant_proposal_allow_incomplete` lets the schema refuse
/// it with an error that names nothing).
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_proposal_without_a_target_or_a_window_is_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir).await;
    let base = || finding("p1", ALPHA, "docs/guide.md", T0);
    let cases = [
        (
            NewProposal {
                target_value: None,
                ..base()
            },
            "target",
        ),
        (
            NewProposal {
                window: None,
                ..base()
            },
            "window",
        ),
        (
            NewProposal {
                window: Some((T0, T0)),
                ..base()
            },
            "window",
        ),
        (
            NewProposal {
                baseline_value: None,
                ..base()
            },
            "value",
        ),
        (
            NewProposal {
                metric: String::new(),
                ..base()
            },
            "metric",
        ),
        (
            NewProposal {
                sample: 0,
                ..base()
            },
            "sample",
        ),
    ];
    for (new, missing) in cases {
        let result = opened(&store, new).await;
        let why = refused(&result);
        assert!(why.contains(missing), "{missing}: {why}");
    }
    let unit = opened(
        &store,
        NewProposal {
            unit: String::new(),
            ..base()
        },
    )
    .await;
    assert!(refused(&unit).contains("unit"), "{unit:?}");
    let evidence = opened(
        &store,
        NewProposal {
            evidence_json: format!("[\"{}\"]", "x".repeat(EVIDENCE_CAP)),
            ..base()
        },
    )
    .await;
    assert!(refused(&evidence).contains("evidence"), "{evidence:?}");
    let nowhere = opened(&store, finding("p1", "c:\\demo\\gamma", "a.md", T0)).await;
    assert!(refused(&nowhere).contains("repository"), "{nowhere:?}");
    assert_eq!(store.reader().proposals(None).await.unwrap(), []);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proposal")
        .fetch_one(store.reader().pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // The schema refuses them too. The reader's pool is read-only, so the raw rows go through a
    // connection of their own, in a transaction that is rolled back; a complete row of the same
    // shape is accepted first, so a refusal is the schema's and not the statement's.
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repo WHERE path = ?")
        .bind(ALPHA)
        .fetch_one(store.reader().pool())
        .await
        .unwrap();
    let mut conn = <SqliteConnection as sqlx::Connection>::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("bosun.db")),
    )
    .await
    .unwrap();
    let mut tx = sqlx::Connection::begin(&mut conn).await.unwrap();
    let insert = |key: &str, target: Option<i64>, to_ms: i64| {
        sqlx::query(
            "INSERT INTO proposal (
                 proposal_key, dedup_key, repo_id, provider, detector, detector_version, remedy,
                 subject_kind, subject, metric, unit, exactness, baseline_value, threshold,
                 target_value, window_from_ms, window_to_ms, sample, state, created_ms,
                 last_seen_ms, seen_count)
             VALUES (?, ?, ?, 'claude', 'd1', 1, 'split', 'file', 'a.md', 'reread_bytes',
                     'bytes', 'exact', 40000, 20000, ?, 1000, ?, 12, 'open', 1000, 1000, 1)",
        )
        .bind(key.to_owned())
        .bind(format!("dedup-{key}"))
        .bind(repo_id)
        .bind(target)
        .bind(to_ms)
    };
    insert("raw-ok", Some(10_000), 2_000)
        .execute(&mut *tx)
        .await
        .unwrap();
    for (key, target, to_ms) in [("raw-1", None, 2_000), ("raw-2", Some(10_000), 1_000)] {
        let error = insert(key, target, to_ms)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("constraint failed"), "{key}: {text}");
    }
    tx.rollback().await.unwrap();
    drop(conn);
    store.close().await.unwrap();
}

/// #11 D: a merge of two repository rows keeps one active proposal per subject and loses no row;
/// a measured step writes the guard and the snapshot's parameters.
#[tokio::test]
async fn merged_repository_rows_keep_one_active_proposal_per_subject() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("bosun.db"), Options::default())
        .await
        .unwrap();
    let reader = store.reader();
    // A named repository at alpha, and an unnamed row registered by path at beta: when the
    // named one moves to beta, the unnamed row is merged into it.
    store
        .writer()
        .write(vec![repo(Some("demo"), ALPHA), repo(None, BETA)])
        .await
        .unwrap();
    opened(&store, finding("kept", ALPHA, "docs/guide.md", T0))
        .await
        .unwrap();
    opened(&store, finding("absorbed", BETA, "docs\\Guide.md", T0))
        .await
        .unwrap();
    opened(&store, finding("other", BETA, "src/main.rs", T0))
        .await
        .unwrap();
    assert_eq!(reader.repos().await.unwrap().len(), 2);

    store
        .writer()
        .write(vec![repo(Some("demo"), BETA)])
        .await
        .unwrap();
    assert_eq!(
        reader.repos().await.unwrap(),
        [crate::RepoRow {
            name: Some("demo".to_owned()),
            path: BETA.to_owned(),
        }]
    );
    let rows = reader.proposals(Some(BETA)).await.unwrap();
    let state_of = |key: &str| {
        let row = rows.iter().find(|r| r.key == key).unwrap();
        (row.repo.as_str(), row.state)
    };
    assert_eq!(rows.len(), 3, "{rows:#?}");
    assert_eq!(state_of("kept"), (BETA, ProposalState::Open));
    assert_eq!(state_of("absorbed"), (BETA, ProposalState::Expired));
    assert_eq!(state_of("other"), (BETA, ProposalState::Open));
    // The old path is an alias of the surviving row.
    assert_eq!(reader.proposals(Some(ALPHA)).await.unwrap(), rows);

    stepped(&store, "kept", landed(T0 + 1), T0 + 1)
        .await
        .unwrap();
    let guarded = ProposalStep::Measured {
        after_value: Some(9_000),
        after_sample: 11,
        guard: Some(Guard {
            metric: "test_failures".to_owned(),
            baseline: 3,
            after: 2,
        }),
        params_json: Some(r#"{"parser_version":3,"drift":"none"}"#.to_owned()),
        outcome: FollowupOutcome::Reached,
    };
    stepped(&store, "kept", guarded, T0 + 2).await.unwrap();
    let kept = reader.proposal("kept").await.unwrap().unwrap();
    assert_eq!(kept.state, ProposalState::Measured);
    assert_eq!(kept.guard_metric.as_deref(), Some("test_failures"));
    assert_eq!((kept.guard_baseline, kept.guard_after), (Some(3), Some(2)));
    assert_eq!(kept.params_json, r#"{"parser_version":3,"drift":"none"}"#);
    assert_eq!(
        (kept.after_value, kept.after_sample, kept.measured_ms),
        (Some(9_000), Some(11), Some(T0 + 2))
    );
    store.close().await.unwrap();
}

/// #11 D: "After dismissed or `not_moved`: a 90-day cooldown" (`mutant_proposal_no_cooldown`
/// stores a repeat right after a dismissal).
#[tokio::test]
async fn a_dismissed_proposal_suppresses_its_key_for_the_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir).await;
    let t1 = T0 + DAY_MS;
    opened(&store, finding("p1", ALPHA, "docs/guide.md", T0))
        .await
        .unwrap();
    let dismissed = ProposalStep::Dismissed {
        reason: "not worth it".to_owned(),
    };
    stepped(&store, "p1", dismissed.clone(), t1).await.unwrap();
    let p1 = store.reader().proposal("p1").await.unwrap().unwrap();
    assert_eq!(p1.state, ProposalState::Dismissed);
    assert_eq!(p1.dismissed_ms, Some(t1));
    assert_eq!(p1.dismiss_reason.as_deref(), Some("not worth it"));

    let early = opened(
        &store,
        finding("p1b", ALPHA, "docs/guide.md", t1 + 89 * DAY_MS),
    )
    .await;
    assert!(refused(&early).contains("cooldown"), "{early:?}");
    opened(
        &store,
        finding("p1c", ALPHA, "docs/guide.md", t1 + 90 * DAY_MS + 1),
    )
    .await
    .unwrap();

    let t2 = T0 + 2 * DAY_MS;
    opened(&store, finding("p2", ALPHA, "src/lib.rs", T0))
        .await
        .unwrap();
    stepped(&store, "p2", landed(T0 + 1), T0 + 1).await.unwrap();
    stepped(&store, "p2", measured(FollowupOutcome::NotMoved), t2)
        .await
        .unwrap();
    let early = opened(
        &store,
        finding("p2b", ALPHA, "src/lib.rs", t2 + 30 * DAY_MS),
    )
    .await;
    assert!(refused(&early).contains("cooldown"), "{early:?}");

    opened(&store, finding("p3", ALPHA, "src/main.rs", T0))
        .await
        .unwrap();
    stepped(&store, "p3", landed(T0 + 1), T0 + 1).await.unwrap();
    stepped(&store, "p3", measured(FollowupOutcome::Insufficient), t2)
        .await
        .unwrap();
    opened(&store, finding("p3b", ALPHA, "src/main.rs", t2 + 1))
        .await
        .unwrap();

    for step in [
        dismissed,
        ProposalStep::Expired,
        landed(t2),
        ProposalStep::Published {
            card_key: "card".to_owned(),
            issue_number: None,
        },
    ] {
        let result = stepped(&store, "p1", step, t2).await;
        assert!(refused(&result).contains("no further step"), "{result:?}");
    }
    let seen = store
        .writer()
        .write(vec![Write::ProposalSeen {
            key: "p1".to_owned(),
            at_ms: t2,
        }])
        .await;
    refused(&seen);
    store.close().await.unwrap();
}

/// #11 D: an annotation is stored on its repository and read back by time, then by write
/// order, within `[from, to)`; one of an unknown repository is refused and fails its batch.
#[tokio::test]
async fn annotations_are_stored_per_repository_and_read_back_by_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(&dir).await;
    let note = |ts_ms: i64, repo: &str, text: &str| Write::Annotation {
        ts_ms,
        repo: repo.to_owned(),
        text: text.to_owned(),
    };
    store
        .writer()
        .write(vec![
            note(T0 + 2, BETA, "second"),
            note(T0 + 1, ALPHA, "first"),
            note(T0 + 2, ALPHA, "third"),
            note(T0 + 9, ALPHA, "outside"),
        ])
        .await
        .unwrap();
    let unknown = store
        .writer()
        .write(vec![
            note(T0 + 3, ALPHA, "rolled back"),
            note(T0 + 3, "c:\\demo\\nowhere", "unknown"),
        ])
        .await;
    assert!(refused(&unknown).contains("unknown repository"), "{unknown:?}");

    let read = store.reader().annotations(T0, T0 + 9).await.unwrap();
    let got: Vec<(i64, Option<&str>, &str)> = read
        .iter()
        .map(|a| (a.ts_ms, a.repo.as_deref(), a.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (T0 + 1, Some(ALPHA), "first"),
            (T0 + 2, Some(BETA), "second"),
            (T0 + 2, Some(ALPHA), "third"),
        ]
    );
    store.close().await.unwrap();
}
