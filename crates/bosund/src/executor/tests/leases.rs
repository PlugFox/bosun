//! Leases in the loop (#8, "Leases"; E1, E15, E20): in worktree mode an item holds what it
//! `needs` from before its first session until it leaves the loop, so two items of two
//! repositories that need the same exclusive resource never overlap, and the wait is written to
//! the store of the one that waited. Under a shared build directory every process the daemon
//! starts in a worktree gets the directory, and the worktree's tracked files are newer than
//! its last build when the process starts.

use crate::leases::{Holder, Leases, Request};

use super::*;

/// How long one item of these scenes may take.
const LIMIT: Duration = Duration::from_secs(180);

/// A card with `verdict` (a TOML literal string) that needs `needs`.
fn card_needing(id: u64, verdict: &str, needs: &[&str]) -> Card {
    let needs: Vec<String> = needs.iter().map(|n| format!("\"{n}\"")).collect();
    let needs = if needs.is_empty() {
        String::new()
    } else {
        format!("needs = [{}]\n", needs.join(", "))
    };
    Card {
        body: format!("Item {id}.\n\n```bosun\nverdict = '''{verdict}'''\n{needs}```\n"),
        ..card(id, "Needs", "true", "")
    }
}

/// This process as a `bosun with` holder: alive while the test runs.
fn this_process() -> Holder {
    let pid = std::process::id();
    Holder::Command(bosun_proc::ProcessKey {
        pid,
        start: bosun_proc::start_time(pid).unwrap().unwrap(),
    })
}

/// The modification time of `path`.
fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// The leases the journal at `path` lists.
fn journalled(path: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| "{\"leases\": []}".to_owned());
    let journal: serde_json::Value = serde_json::from_str(&text).unwrap();
    journal["leases"].as_array().cloned().unwrap_or_default()
}

/// Rewrites the scene's configuration after its fields changed.
fn configure(scene: &Scene) {
    std::fs::write(&scene.config_path, scene.config_text("'docs/frozen'")).unwrap();
}

/// Two repositories (each its own store and loop) share one manager over `[resource.rig]`,
/// exclusive, machine-wide; each has one card that needs it, whose session marks its start,
/// sleeps 1.5 s and marks its end. The test holds `rig` itself until both items wait for it,
/// so the second really waits. Both end Done, their sessions never overlap, the second's store
/// has its wait for `rig` (at least the first's session long) and its release, and the journal
/// is empty at the end. (The claim takes the leases and keeps them for the first session, so
/// `mutant_loop_skip_leases`, which only lets a session skip its own request, does not reach
/// this scene; `mutant_loop_lease_after_setup` and the parked scene below cover the waits.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_items_that_need_the_same_resource_never_overlap_and_the_wait_is_recorded() {
    const RIG: &str = "[resource.rig]\nkind = \"device\"\ncapacity = 1\nscope = \"machine\"\n";
    let mut one = Scene::with_worktrees(None).await;
    let mut two = Scene::with_worktrees(None).await;
    two.name = "other".to_owned();
    for scene in [&mut one, &mut two] {
        scene.config_more = RIG.to_owned();
        configure(scene);
    }
    let leases = Arc::new(
        Leases::new(
            &one.config(),
            &crate::leases::journal_path(&one.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    one.leases = Some(leases.clone());
    two.leases = Some(leases.clone());

    let gate = this_process();
    leases
        .acquire(Request {
            holder: gate.clone(),
            names: vec!["rig".to_owned()],
            repo: "demo".to_owned(),
            identity: "test".to_owned(),
            writer: None,
        })
        .await
        .unwrap();

    for scene in [&one, &two] {
        scene.write_board(&[card_needing(1, "test -f done.txt", &["rig"])]);
        scene.script(
            1,
            &[
                format!("write {} x", scene.path("start").display()),
                "sleep 1500".to_owned(),
                format!("write {} x", scene.path("end").display()),
                "commit done.txt done".to_owned(),
            ],
        );
    }
    let (task_one, watcher_one) = one.start(None);
    let (task_two, watcher_two) = two.start(None);

    let (item_one, item_two) = (
        Holder::Item(ItemId::new("demo#1")),
        Holder::Item(ItemId::new("other#1")),
    );
    // Both wait for `rig` (a loop that does not wait starts its session instead).
    wait_for("both items to wait for rig", LIMIT, async || {
        (leases.is_waiting(&item_one) && leases.is_waiting(&item_two))
            || one.path("start").exists()
            || two.path("start").exists()
    })
    .await;
    leases.release(&gate).await;

    let state_one = ended(&one, 1, LIMIT).await;
    let state_two = ended(&two, 1, LIMIT).await;
    assert!(matches!(state_one, State::Done(_)), "{state_one:?}");
    assert!(matches!(state_two, State::Done(_)), "{state_two:?}");

    let interval = |scene: &Scene| (mtime(&scene.path("start")), mtime(&scene.path("end")));
    let (a, b) = (interval(&one), interval(&two));
    assert!(
        a.1 <= b.0 || b.1 <= a.0,
        "the sessions overlap: {a:?} and {b:?}"
    );

    let (second, holder) = if b.0 > a.0 {
        (&two, "item other#1")
    } else {
        (&one, "item demo#1")
    };
    let mut row = None;
    wait_for(
        "the second item's lease to be released",
        LIMIT,
        async || {
            let rows = second
                .store
                .reader()
                .lease_waits(0, i64::MAX)
                .await
                .unwrap();
            row = rows
                .into_iter()
                .find(|r| r.resource == "rig" && r.holder == holder);
            row.as_ref().is_some_and(|r| r.released_ms.is_some())
        },
    )
    .await;
    let row = row.unwrap();
    let granted = row.granted_ms.expect("the wait was granted");
    assert!(
        granted - row.filed_ms >= 1000,
        "the second waited only {} ms: {row:?}",
        granted - row.filed_ms
    );

    wait_for("the journal to empty", LIMIT, async || {
        journalled(leases.journal()).is_empty()
    })
    .await;
    for task in [task_one, watcher_one, task_two, watcher_two] {
        task.abort();
    }
}

/// `[resource.rig]`: exclusive, machine-wide.
const RIG: &str = "[resource.rig]\nkind = \"device\"\ncapacity = 1\nscope = \"machine\"\n";

/// The daemon's API over the scene's configuration with `leases` as its manager: what
/// `bosun with` calls. Its identities are opened apart from the scene's own store connection.
async fn lease_api(scene: &Scene, leases: &Arc<Leases>) -> crate::state::State {
    std::fs::create_dir_all(scene.path("claude").join("projects")).unwrap();
    let identities = crate::state::open(&scene.config(), "lease-epoch", EventOptions::default())
        .await
        .map_err(|(_, e)| e)
        .unwrap();
    crate::state::State {
        started_ms: 0,
        identities,
        credentials: scene.credentials.clone(),
        control: scene.control.clone(),
        leases: leases.clone(),
    }
}

/// A child process of this test that runs until `release` exists: a `fake-agent` whose script
/// waits for the file.
fn waiting_child(scene: &Scene, release: &Path) -> std::process::Child {
    let script = scene.path("command.script");
    std::fs::write(&script, format!("wait {}\n", release.display())).unwrap();
    Command::new(fake_agent())
        .arg("--fake-script")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

/// A command of the test's own (a child that waits for a file) takes `rig` through
/// `lease.acquire` with its pid and is granted; then a card that needs `rig` goes to Ready. For
/// 2 s after it asks for `rig` the item has no run. The child exits: its lease goes with its process, the item runs and
/// ends Done, and its `lease_wait` row for `rig` shows the wait (at least 1.5 s).
/// `mutant_api_lease_acquire_always_grants` answers granted without leasing, so the item runs
/// at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_holding_a_resource_blocks_an_item_until_it_exits() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = RIG.to_owned();
    configure(&scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    let api = lease_api(&scene, &leases).await;
    let mut conn = crate::api::Connection::default();

    let release = scene.path("release");
    let mut child = waiting_child(&scene, &release);
    let acquired = rpc(
        &api,
        &mut conn,
        "lease.acquire",
        serde_json::json!({
            "operator": control::TOKEN,
            "identity": "test",
            "resources": ["rig"],
            "pid": child.id(),
        }),
    )
    .await;
    assert_eq!(acquired["result"]["granted"], true, "{acquired}");

    scene.write_board(&[card_needing(1, "test -f done.txt", &["rig"])]);
    scene.script(1, &["commit done.txt done".to_owned()]);
    let (task, watcher) = scene.start(None);
    // From when the item asks for `rig` (a loop that does not ask runs it instead).
    let item = Holder::Item(ItemId::new("demo#1"));
    wait_for("the item to wait for rig", LIMIT, async || {
        leases.is_waiting(&item) || !scene.runs(1).await.is_empty()
    })
    .await;
    let held_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < held_until {
        assert!(
            scene.runs(1).await.is_empty(),
            "the item ran while the command held rig"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    std::fs::write(&release, "x").unwrap();
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let state = ended(&scene, 1, LIMIT).await;
    assert!(matches!(state, State::Done(_)), "{state:?}");

    let rows = scene.store.reader().lease_waits(0, i64::MAX).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.resource == "rig" && r.holder == "item demo#1")
        .unwrap_or_else(|| panic!("no wait of demo#1 for rig: {rows:?}"));
    let granted = row.granted_ms.expect("the wait was granted");
    assert!(
        granted - row.filed_ms >= 1500,
        "the item waited only {} ms: {row:?}",
        granted - row.filed_ms
    );
    task.abort();
    watcher.abort();
    crate::state::shut_down(api.identities).await;
}

/// `lease.acquire` and `lease.release` without the operator token, and with a wrong one, are
/// refused as unauthorized, and nothing is leased; with the token the same request is granted
/// (`mutant_api_lease_without_operator` leases without it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_call_without_the_operator_token_is_refused() {
    use bosun_api::rpc::code;

    let mut scene = Scene::new().await;
    scene.config_more = RIG.to_owned();
    configure(&scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    let api = lease_api(&scene, &leases).await;
    let mut conn = crate::api::Connection::default();
    let me = this_process();
    let pid = std::process::id();
    let acquire = |operator: Option<&str>| {
        let mut params = serde_json::json!({"identity": "test", "resources": ["rig"], "pid": pid});
        if let Some(operator) = operator {
            params["operator"] = operator.into();
        }
        params
    };
    let release = |operator: Option<&str>| {
        let mut params = serde_json::json!({ "pid": pid });
        if let Some(operator) = operator {
            params["operator"] = operator.into();
        }
        params
    };

    for operator in [None, Some(""), Some("bosun-operator-forged")] {
        let refused = rpc(&api, &mut conn, "lease.acquire", acquire(operator)).await;
        assert_eq!(
            error_code(&refused),
            Some(code::UNAUTHORIZED.into()),
            "{operator:?}: {refused}"
        );
        assert_eq!(leases.holds(&me), Vec::<String>::new(), "{operator:?}");
        assert!(!leases.is_waiting(&me), "{operator:?}");
    }

    let granted = rpc(
        &api,
        &mut conn,
        "lease.acquire",
        acquire(Some(control::TOKEN)),
    )
    .await;
    assert_eq!(granted["result"]["granted"], true, "{granted}");
    assert_eq!(leases.holds(&me), ["rig"]);
    for operator in [None, Some(""), Some("bosun-operator-forged")] {
        let refused = rpc(&api, &mut conn, "lease.release", release(operator)).await;
        assert_eq!(
            error_code(&refused),
            Some(code::UNAUTHORIZED.into()),
            "{operator:?}: {refused}"
        );
        assert_eq!(leases.holds(&me), ["rig"], "{operator:?}");
    }
    let released = rpc(
        &api,
        &mut conn,
        "lease.release",
        release(Some(control::TOKEN)),
    )
    .await;
    assert_eq!(released["result"]["released"], true, "{released}");
    assert_eq!(leases.holds(&me), Vec::<String>::new());
    crate::state::shut_down(api.identities).await;
}

/// One repository with `[resource.target]` as its shared build directory under the variable
/// `BUILD_DIR_FOR_TEST`; its setup sleeps a second, stamps the directory and sleeps another
/// second. The session commits `a.txt`; the verdict passes only when the directory is in its
/// environment and `a.txt` (in the fresh verdict checkout, older than the stamp of its setup)
/// was touched after the setup and before the verdict. The item ends Done
/// (`mutant_loop_no_build_env` and `mutant_loop_skip_mtime_touch` fail it).
///
/// Both sleeps matter because the verdict shell may compare whole seconds: macOS `/bin/sh` is
/// bash 3.2, whose `test -nt` compares `st_mtime` in whole seconds (dash, bash 5 and Git for
/// Windows' bash compare nanoseconds). At whole-second resolution `x + 1 s` always falls in a
/// later second than `x`. With the touch, `a.txt` is touched at least a second after the stamp,
/// so it is newer even at one-second resolution; without the second sleep it would share the
/// stamp's second and every verdict would fail. Without the touch
/// (`mutant_loop_skip_mtime_touch`), `a.txt` keeps its checkout time, at least a second before
/// the stamp, so it stays older and the mutant still goes red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verdict_under_the_shared_build_gets_its_directory_and_fresh_file_times() {
    let mut scene = Scene::with_worktrees(Some(
        "sleep 1; touch \"$BUILD_DIR_FOR_TEST/stamp\"; sleep 1",
    ))
    .await;
    let build = scene.path("build");
    std::fs::create_dir_all(&build).unwrap();
    scene.worktree_more = format!(
        ", shared_build = {{ dir = '{}', resource = \"target\", env = \"BUILD_DIR_FOR_TEST\" }}",
        sh_path(&build)
    );
    scene.config_more = "[resource.target]\nkind = \"build-dir\"\ncapacity = 1\n".to_owned();
    configure(&scene);
    scene.write_board(&[card_needing(
        1,
        "test -n \"$BUILD_DIR_FOR_TEST\" && test a.txt -nt \"$BUILD_DIR_FOR_TEST/stamp\"",
        &[],
    )]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let (task, watcher) = scene.start(None);
    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert!(build.join("stamp").is_file());
}

/// E19: a card that needs a configured resource is claimable in worktree mode, and refused by
/// lint, as in stage 1, in a repository without `repo.<name>.worktree`.
#[tokio::test]
async fn a_card_that_needs_a_resource_is_refused_without_worktree_mode() {
    const RIG: &str = "[resource.rig]\nkind = \"device\"\n";
    let mut stage1 = Scene::new().await;
    let mut worktrees = Scene::with_worktrees(None).await;
    for scene in [&mut stage1, &mut worktrees] {
        scene.config_more = RIG.to_owned();
        configure(scene);
    }
    let card = card_needing(1, "test -f done.txt", &["rig"]);
    let on_board = std::collections::BTreeSet::from([1]);
    let lint = |scene: &Scene| {
        let config = scene.config();
        let repo = &config.repos["demo"];
        let leases = Leases::new(
            &config,
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap();
        let resources = lint_resources(repo, &leases.names());
        claim::static_lint(&card, repo, &on_board, &resources)
    };
    let refused = lint(&stage1);
    assert!(!refused.claimable(), "{:?}", refused.findings);
    let admitted = lint(&worktrees);
    assert!(admitted.claimable(), "{:?}", admitted.findings);
}

/// `[resource.target]`: the shared build directory's resource, exclusive, machine-wide.
const TARGET: &str = "[resource.target]\nkind = \"build-dir\"\ncapacity = 1\n";

/// Gives `scene` (in worktree mode) a shared build directory at `<scene>/build`, leased as
/// `target`, with `more` appended to its `worktree` table, and its own lease manager.
fn share_the_build(scene: &mut Scene, more: &str) -> Arc<Leases> {
    let build = scene.path("build");
    std::fs::create_dir_all(&build).unwrap();
    scene.worktree_more = format!(
        ", shared_build = {{ dir = '{}', resource = \"target\", env = \"BUILD_DIR_FOR_TEST\" }}{more}",
        sh_path(&build)
    );
    scene.config_more = TARGET.to_owned();
    configure(scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    leases
}

/// Takes `names` for this process, as a `bosun with` command would.
async fn hold_as_this_process(leases: &Leases, names: &[&str]) {
    leases
        .acquire(Request {
            holder: this_process(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            repo: "demo".to_owned(),
            identity: "test".to_owned(),
            writer: None,
        })
        .await
        .unwrap();
}

/// The last component of `path`, whichever separator it is spelled with.
fn last_component(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
}

/// E1: the claim's worktree setup and its verdict on the base commit run under the shared
/// build, so they wait for its lease. The test holds `target` itself, then a card goes to
/// Ready: for 2 s after the item asks for `target` its setup has not run and it has no run.
/// The test lets go: the setup runs and the item ends Done (`mutant_loop_lease_after_setup`
/// runs the setup and the base verdict at once, and the first session takes the lease).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_setup_and_the_base_verdict_wait_for_the_shared_build() {
    let mut scene = Scene::with_worktrees(None).await;
    let ran = scene.path("setup-ran");
    scene.setup = Some(format!("echo x >> \"{}\"", sh_path(&ran)));
    let leases = share_the_build(&mut scene, "");
    hold_as_this_process(&leases, &["target"]).await;

    scene.write_board(&[card_needing(1, "test -f done.txt", &[])]);
    scene.script(1, &["commit done.txt done".to_owned()]);
    let (task, watcher) = scene.start(None);
    // From when the item asks for `target` (a claim that does not ask runs the setup instead).
    let item = Holder::Item(ItemId::new("demo#1"));
    wait_for("the item to wait for target", LIMIT, async || {
        leases.is_waiting(&item) || ran.exists()
    })
    .await;
    let held_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < held_until {
        assert!(!ran.exists(), "the setup ran while the test held target");
        assert!(
            scene.runs(1).await.is_empty(),
            "the item ran while the test held target"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    leases.release(&this_process()).await;
    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert!(ran.exists(), "the setup never ran");
}

/// E1: `teardown` runs under the shared build, so the item still holds its leases while the
/// teardown of its own worktree runs; it lets go of them after. The teardown writes where it
/// runs and sleeps 2 s; when it names the item's worktree, the item holds `target`. After the
/// item ends Done it holds nothing and the journal is empty
/// (`mutant_loop_release_before_teardown` releases before the teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_runs_while_the_item_still_holds_its_leases() {
    let mut scene = Scene::with_worktrees(None).await;
    let log = scene.path("teardown-ran");
    let leases = share_the_build(
        &mut scene,
        &format!(", teardown = \"pwd >> '{}'; sleep 2\"", sh_path(&log)),
    );
    scene.write_board(&[card_needing(1, "test -f done.txt", &[])]);
    scene.script(1, &["commit done.txt done".to_owned()]);
    let (task, watcher) = scene.start(None);

    let item = ItemId::new("demo#1");
    let holder = Holder::Item(item.clone());
    let deadline = Instant::now() + LIMIT;
    let held = loop {
        assert!(
            Instant::now() < deadline,
            "the item's worktree was never torn down"
        );
        let rows = scene.store.reader().worktrees(&item).await.unwrap();
        let own = rows
            .iter()
            .find(|r| r.kind == bosun_store::WorktreeKind::Item)
            .map(|r| last_component(&r.path).to_owned());
        let lines = std::fs::read_to_string(&log).unwrap_or_default();
        if let Some(own) = own
            && lines.lines().any(|line| last_component(line) == own)
        {
            break leases.holds(&holder);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        held,
        ["target"],
        "the item's teardown ran without its lease"
    );

    let state = ended(&scene, 1, LIMIT).await;
    assert!(matches!(state, State::Done(_)), "{state:?}");
    wait_for("the item to let go of target", LIMIT, async || {
        leases.holds(&holder).is_empty() && journalled(leases.journal()).is_empty()
    })
    .await;
    task.abort();
    watcher.abort();
}

/// A loop the store halts (it refuses the transition that runs the verdict) runs nothing more
/// for its item, so it frees the item's leases: once the loop task has ended the item holds
/// nothing, the journal is empty, and another holder is granted `rig` at once
/// (`mutant_loop_halt_keeps_leases` keeps them while the daemon runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_that_halts_frees_its_items_leases() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = RIG.to_owned();
    configure(&scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    scene.write_board(&[card_needing(1, "test -f done.txt", &["rig"])]);
    scene.script(1, &["commit done.txt done".to_owned()]);
    let failpoint: Failpoint = Arc::new(|t: &bosun_core::Transition| {
        t.effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::RunVerdict { .. }))
    });
    let (task, watcher) = scene.start(Some(failpoint));
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("the loop stops after a refused transition")
        .unwrap();
    watcher.abort();

    let item = Holder::Item(ItemId::new("demo#1"));
    assert_eq!(leases.holds(&item), Vec::<String>::new());
    assert!(!leases.is_waiting(&item));
    assert_eq!(
        journalled(leases.journal()),
        Vec::<serde_json::Value>::new()
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        hold_as_this_process(&leases, &["rig"]),
    )
    .await
    .expect("rig is free once the loop halted");
    assert_eq!(leases.holds(&this_process()), ["rig"]);
}

/// One worktree-mode scene with `[resource.rig]` and its own lease manager.
async fn rig_scene() -> (Scene, Arc<Leases>) {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = RIG.to_owned();
    configure(&scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    (scene, leases)
}

/// Whether the journal at `path` lists a lease of `item`.
fn journal_lists(path: &Path, item: &str) -> bool {
    journalled(path)
        .iter()
        .any(|e| e["holder"]["item"].as_str() == Some(item))
}

/// E24: a card that needs `rig` parks once on a rate limit, then commits. While it is Parked it
/// holds nothing, the journal does not list it, and this process is granted `rig` at once. The
/// test moves the clock past the park while it holds `rig`: for 2 s the item runs no session.
/// The test lets go: the item resumes and ends Done, and its store has two grants of `rig`
/// (the claim's and the resume's), each released (`mutant_loop_hold_leases_while_parked` keeps
/// the lease while parked; `mutant_loop_skip_leases` resumes without waiting for `rig`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_item_holds_no_lease_and_takes_it_again_to_resume() {
    let (scene, leases) = rig_scene().await;
    let m = |name: &str| scene.path(name).display().to_string();
    scene.write_board(&[card_needing(1, "test -f done.txt", &["rig"])]);
    scene.script(
        1,
        &[
            format!(
                "once {} replay {}",
                m("m1"),
                Scene::stream("limit-no-reset.jsonl")
            ),
            format!("once {} exit 1", m("m2")),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let item = Holder::Item(ItemId::new("demo#1"));

    wait_for("the limit to park the item", LIMIT, async || {
        let state = scene.state(1).await;
        matches!(state, Some(State::Parked(_))) || terminal(state)
    })
    .await;
    let state = scene.state(1).await;
    assert!(matches!(state, Some(State::Parked(_))), "{state:?}");
    assert_eq!(
        leases.holds(&item),
        Vec::<String>::new(),
        "held while parked"
    );
    assert!(
        !journal_lists(leases.journal(), "demo#1"),
        "journalled while parked: {:?}",
        journalled(leases.journal())
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        hold_as_this_process(&leases, &["rig"]),
    )
    .await
    .expect("rig is free while the item is parked");
    assert_eq!(scene.runs(1).await.len(), 1);

    // Past the park: the item asks for `rig` again and waits.
    scene.clock.advance(Duration::from_mins(16));
    let held_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < held_until {
        assert_eq!(
            scene.runs(1).await.len(),
            1,
            "the item resumed while the test held rig"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    leases.release(&this_process()).await;
    let state = ended(&scene, 1, LIMIT).await;
    assert!(matches!(state, State::Done(_)), "{state:?}");
    let mut rows = Vec::new();
    wait_for("both grants of rig to be released", LIMIT, async || {
        rows = scene
            .store
            .reader()
            .lease_waits(0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.resource == "rig" && r.holder == "item demo#1")
            .collect();
        rows.len() >= 2 && rows.iter().all(|r| r.released_ms.is_some())
    })
    .await;
    task.abort();
    watcher.abort();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.granted_ms.is_some()), "{rows:?}");
}

/// E24: as `a_dirty_checkout_of_the_target_defers_the_landing_and_holds_the_queue`, with a card
/// that needs `rig`: once the fast-forward is reported deferred, the item holds nothing and
/// this process is granted `rig` at once. The test lets go and deletes the file: the item ends
/// Done with its commit on the user's target (`mutant_loop_hold_leases_while_deferred` keeps
/// the lease while deferred).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_landing_holds_no_lease() {
    let (mut scene, leases) = rig_scene().await;
    let scratch = scene.repo.join("scratch.txt");
    let then = format!("echo mine > '{}'", sh_path(&scratch));
    super::landing::setup_once(&mut scene, &then);
    let mut sub = super::worktrees::subscribe_trouble(&scene);
    scene.write_board(&[card_needing(1, "test -f a.txt", &["rig"])]);
    scene.script(1, &["commit a.txt a".to_owned()]);
    let start = scene.git(&["rev-parse", "HEAD"]);
    let (task, watcher) = scene.start(None);
    let item = Holder::Item(ItemId::new("demo#1"));

    let mut seen = Vec::new();
    wait_for("the deferred fast-forward", LIMIT, async || {
        seen.extend(super::worktrees::trouble(&mut sub));
        seen.iter().any(|t| t.contains("is deferred")) || terminal(scene.state(1).await)
    })
    .await;
    assert!(seen.iter().any(|t| t.contains("is deferred")), "{seen:#?}");
    assert_eq!(
        leases.holds(&item),
        Vec::<String>::new(),
        "held while deferred"
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        hold_as_this_process(&leases, &["rig"]),
    )
    .await
    .expect("rig is free while the landing is deferred");
    assert_eq!(scene.state(1).await, Some(State::Landing));

    leases.release(&this_process()).await;
    std::fs::remove_file(&scratch).unwrap();
    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert!(matches!(state, State::Done(_)), "{state:?}");
    assert_eq!(scene.git(&["rev-parse", "HEAD~1"]), start);
    assert!(super::landing::files_at(&scene, "HEAD").contains("a.txt"));
}

/// A lease refusal is a configuration change, not a failed run: with `rig` and `other`, a card
/// that needs `rig` starts its session; the test then swaps the item's lease for `other`, so
/// the verdict's request for `rig` is refused (the holder holds another set). The item ends
/// Needs you (`config_changed`) with no verdict run and one attempt, none failed
/// (`mutant_loop_lease_refusal_fails_verdict` takes the refusal for a failed run).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_refusal_before_the_verdict_needs_the_user_and_counts_no_attempt() {
    let mut scene = Scene::with_worktrees(None).await;
    scene.config_more = format!("{RIG}[resource.other]\nkind = \"device\"\ncapacity = 1\n");
    configure(&scene);
    let leases = Arc::new(
        Leases::new(
            &scene.config(),
            &crate::leases::journal_path(&scene.path("state").join("bosun.token")),
        )
        .unwrap(),
    );
    scene.leases = Some(leases.clone());
    let (started, go) = (scene.path("started"), scene.path("go"));
    scene.write_board(&[card_needing(1, "test -f done.txt", &["rig"])]);
    scene.script(
        1,
        &[
            format!("write {} x", started.display()),
            format!("wait {}", go.display()),
            "commit done.txt done".to_owned(),
        ],
    );
    let (task, watcher) = scene.start(None);
    let id = ItemId::new("demo#1");
    let item = Holder::Item(id.clone());

    wait_for("the session to start holding rig", LIMIT, async || {
        (started.exists() && leases.holds(&item) == ["rig"]) || terminal(scene.state(1).await)
    })
    .await;
    assert_eq!(leases.holds(&item), ["rig"]);
    leases.release(&item).await;
    leases
        .acquire(Request {
            holder: item.clone(),
            names: vec!["other".to_owned()],
            repo: "demo".to_owned(),
            identity: "test".to_owned(),
            writer: None,
        })
        .await
        .unwrap();
    std::fs::write(&go, "x").unwrap();

    let state = ended(&scene, 1, LIMIT).await;
    task.abort();
    watcher.abort();
    assert_eq!(state, State::NeedsYou(Reason::ConfigChanged));
    let verdicts = scene.store.reader().verdict_runs(&id).await.unwrap();
    assert!(verdicts.is_empty(), "{verdicts:?}");
    let row = scene.item(1).await.unwrap();
    assert_eq!((row.attempts, row.failed), (1, 0), "{row:?}");
}
