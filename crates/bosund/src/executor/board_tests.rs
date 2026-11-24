//! The scenarios of #106 and #111 against the loop on the fake Projects v2 board: the daemon's
//! own markers past the author allowlist, a card that leaves the board or whose issue is closed
//! mid-run, and the verdict tails by the repository's visibility.

use super::*;

/// #106: card 1 carries the claim marker of another daemon id of the same identity, and the
/// identity's login is not on its own author allowlist. The marker is still seen: the
/// repository is refused and nothing is claimed (`mutant_queue_markers_through_allowlist`
/// filters the marker out, and card 1 is claimed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_is_refused_with_the_login_off_the_allowlist() {
    let mut scene = Scene::on_projects().await;
    scene.unlist_login();
    let fake = scene.projects.clone().unwrap();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    fake.issue(BOARD_REPO, 1, |issue| {
        issue.comments.push(FakeComment::by(
            LOGIN,
            &bosun_queue::marker::embed(
                &bosun_queue::claim::claim_key("other-host/other-user"),
                "bosun: claimed by the daemon `other-host/other-user`.",
            ),
        ));
    });
    scene.script(1, &["commit one.txt one".to_owned()]);
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    tokio::time::sleep(Duration::from_secs(3)).await;
    task.abort();
    watcher.abort();

    assert_eq!(fake.count("UpdateStatus"), 0, "{:?}", fake.requests());
    assert_eq!(fake.count("AddComment"), 0, "{:?}", fake.requests());
    for n in [1, 2] {
        let state = scene.state(n).await;
        assert!(
            matches!(state, None | Some(State::Ready)),
            "card {n}: {state:?}"
        );
        assert_eq!(fake.status_of(0, BOARD_REPO, n).as_deref(), Some("Ready"));
    }
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 0);
}

/// #106: a comment the identity's login wrote is never in a prompt, whether the login is on
/// the author allowlist or not: a keyed one (the daemon's) never, a plain one only as the
/// user's when the login is listed. A comment by another allowed author is inlined.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_comment_by_the_identitys_login_is_never_in_a_prompt() {
    for listed in [true, false] {
        let mut scene = Scene::on_projects().await;
        if !listed {
            scene.unlist_login();
        }
        let fake = scene.projects.clone().unwrap();
        scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
        fake.issue(BOARD_REPO, 1, |issue| {
            issue.comments.extend([
                FakeComment::by(
                    LOGIN,
                    &bosun_queue::marker::embed("note:1", "MARKED-BY-THE-DAEMON"),
                ),
                FakeComment::by(LOGIN, "PLAIN-BY-THE-LOGIN"),
                FakeComment::by("local", "FROM-AN-ALLOWED-AUTHOR"),
            ]);
        });
        let prompt = scene.path("prompt1");
        scene.script(
            1,
            &[
                format!("stdin {}", prompt.display()),
                "commit one.txt one".to_owned(),
            ],
        );
        let (task, watcher) = scene.start(None);
        let state = ended(&scene, 1, Duration::from_secs(60)).await;
        task.abort();
        watcher.abort();
        assert!(
            matches!(state, State::Done(_)),
            "listed {listed}: {state:?}"
        );
        let text = std::fs::read_to_string(&prompt).unwrap();
        assert!(text.contains("FROM-AN-ALLOWED-AUTHOR"), "{text}");
        assert!(!text.contains("MARKED-BY-THE-DAEMON"), "{text}");
        assert!(!text.contains("claimed by the daemon"), "{text}");
        assert_eq!(
            text.contains("PLAIN-BY-THE-LOGIN"),
            listed,
            "listed {listed}: {text}"
        );
    }
}

/// #106: the issue of the item in flight is deleted during its session. The loop keeps polling
/// the board, so the item is cancelled and its session killed (`mutant_loop_no_midrun_poll`:
/// the board is not read mid-run, and the session runs on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_issue_deleted_mid_session_cancels_the_item_and_kills_the_session() {
    let scene = Scene::on_projects().await;
    let fake = scene.projects.clone().unwrap();
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card(1, "One", "test -f one.txt", "Item one.")]);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            // Waits (at most a minute) for a file nobody writes.
            format!("wait {}", p("never")),
            "commit one.txt one".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    let mut children = Vec::new();
    wait_for(
        "the session's child to be recorded",
        Duration::from_secs(30),
        async || {
            children = scene.store.reader().children().await.unwrap();
            !children.is_empty()
        },
    )
    .await;
    let child = &children[0];
    let key = bosun_proc::ProcessKey {
        pid: child.pid,
        start: bosun_proc::start_time(child.pid)
            .unwrap()
            .expect("the session runs"),
    };
    assert!(bosun_proc::is_running(key));

    fake.delete_issue(BOARD_REPO, 1);
    let state = ended_or_cancelled(&scene, 1, Duration::from_secs(10)).await;
    assert_eq!(state, State::Cancelled);
    wait_for("no child", Duration::from_secs(5), async || {
        !bosun_proc::is_running(key)
    })
    .await;
    task.abort();
    watcher.abort();
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
    assert!(!scene.repo.join("one.txt").exists());
}

/// The child of the session of item 1, once it is recorded.
async fn session_child(scene: &Scene) -> bosun_proc::ProcessKey {
    let mut children = Vec::new();
    wait_for(
        "the session's child to be recorded",
        Duration::from_secs(30),
        async || {
            children = scene.store.reader().children().await.unwrap();
            !children.is_empty()
        },
    )
    .await;
    let child = &children[0];
    bosun_proc::ProcessKey {
        pid: child.pid,
        start: bosun_proc::start_time(child.pid)
            .unwrap()
            .expect("the session runs"),
    }
}

/// #111: the user closes the issue of the item in flight during its session; the card stays
/// in its column. The close is a cancel: the item is cancelled and its session killed
/// (`mutant_queue_closed_not_cancel`: the card is read by its column, and the session runs on).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_issue_closed_mid_session_cancels_the_item_and_kills_the_session() {
    let scene = Scene::on_projects().await;
    let fake = scene.projects.clone().unwrap();
    let p = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[
        card(1, "One", "test -f one.txt", "Item one."),
        card(2, "Two", "test -f two.txt", "Item two."),
    ]);
    // Closed while in Ready: never claimed.
    fake.close_issue(BOARD_REPO, 2);
    scene.script(
        1,
        &[
            format!("write {} x", p("flag1")),
            // Waits (at most a minute) for a file nobody writes.
            format!("wait {}", p("never")),
            "commit one.txt one".to_owned(),
        ],
    );
    scene.script(2, &["commit two.txt two".to_owned()]);
    let (task, watcher) = scene.start(None);
    wait_file(&scene.path("flag1")).await;
    let key = session_child(&scene).await;
    assert!(bosun_proc::is_running(key));

    fake.close_issue(BOARD_REPO, 1);
    let state = ended_or_cancelled(&scene, 1, Duration::from_secs(10)).await;
    assert_eq!(state, State::Cancelled);
    wait_for("no child", Duration::from_secs(5), async || {
        !bosun_proc::is_running(key)
    })
    .await;
    // Nothing else is claimed: card 2's issue is closed.
    tokio::time::sleep(Duration::from_secs(1)).await;
    task.abort();
    watcher.abort();
    assert_eq!(scene.spawner.spawns.load(Ordering::SeqCst), 1);
    assert!(!scene.repo.join("one.txt").exists());
    assert!(
        matches!(scene.state(2).await, None | Some(State::Ready)),
        "{:?}",
        scene.state(2).await
    );
}

/// The body of the daemon's sticky report on issue `n` of the fake, once it says `wanted`.
async fn report_saying(fake: &FakeGitHub, n: u64, wanted: &str) -> String {
    let key = bosun_core::publish::report_key(&ItemId::new(format!("demo#{n}")));
    let mut body = String::new();
    wait_for(
        &format!("the report of {n} to say {wanted:?}"),
        Duration::from_secs(30),
        async || {
            body = fake
                .issue_now(BOARD_REPO, n)
                .comments
                .iter()
                .find(|c| bosun_queue::marker::extract(&c.body).is_some_and(|(k, _)| k == key))
                .map_or_else(String::new, |c| c.body.clone());
            body.contains(wanted)
        },
    )
    .await;
    body
}

/// #111: the sticky report of a Done item carries its verdict output tail on a private
/// repository and leaves it out on a public one, as GitHub reports the repository's visibility
/// (`mutant_report_tails_on_public`: the visibility is not asked, and the public repository's
/// report carries the tail).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verdict_tail_is_published_on_a_private_repository_only() {
    for public in [false, true] {
        let scene = Scene::on_projects().await;
        let fake = scene.projects.clone().unwrap();
        if public {
            fake.set_visibility(BOARD_REPO, "PUBLIC");
        }
        scene.write_board(&[card(
            1,
            "One",
            "echo SYNTHETIC-TAIL-LINE && test -f one.txt",
            "Item one.",
        )]);
        scene.script(1, &["commit one.txt one".to_owned()]);
        let (task, watcher) = scene.start(None);
        let state = ended(&scene, 1, Duration::from_secs(60)).await;
        assert_eq!(state, State::Done(Outcome::Pass), "public {public}");
        drained(&scene).await;
        let report = report_saying(&fake, 1, "Done:").await;
        task.abort();
        watcher.abort();
        let tail = report.contains("<details><summary>Attempt 1");
        let withheld = report.contains("Verdict output is not published");
        assert_eq!(
            (tail, withheld),
            (!public, public),
            "public {public}: {report}"
        );
    }
}

/// Waits for item `card` to leave the daemon's states; at most `limit`.
async fn ended_or_cancelled(scene: &Scene, card: u64, limit: Duration) -> State {
    let mut state = None;
    wait_for(&format!("item {card} to end"), limit, async || {
        state = scene.state(card).await;
        terminal(state) || state == Some(State::Cancelled)
    })
    .await;
    state.unwrap()
}
