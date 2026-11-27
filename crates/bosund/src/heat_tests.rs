//! `heat.evaluate` and `heat.list` (#11 D) over the served state of two identities
//! (`notify_tests::World`): `a` with repository `alpha`, `b` with `beta`, each its own store.
//! Synthetic Claude transcripts of run sessions are ingested into `b` only.

use bosun_core::{Ctx, Event, Item, ItemId, SpawnMode, Stepped, canonical_path, step, style_of};
use bosun_store::{NewItem, RunStart, Write};
use serde_json::{Value, json};

use crate::notify_tests::World;

const DAY: i64 = 24 * 60 * 60 * 1000;

/// The run sessions of `b`; the first five read `src/big.rs`.
fn session(n: usize) -> String {
    format!("00000000-0000-4000-8000-2260000000{n:02}")
}

/// `ms` since the epoch as an RFC 3339 UTC timestamp.
fn timestamp(ms: i64) -> String {
    let days = ms.div_euclid(DAY);
    let in_day = ms.rem_euclid(DAY);
    // H. Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// One transcript line of `session` in `cwd`.
fn line(session: &str, cwd: &str, n: usize, at_ms: i64, body: &Value) -> String {
    let mut record = json!({
        "parentUuid": null,
        "isSidechain": false,
        "userType": "external",
        "cwd": cwd,
        "sessionId": session,
        "version": "0.0.0",
        "gitBranch": "main",
        "uuid": format!("00000000-0000-4000-8000-3260000{n:05}"),
        "timestamp": timestamp(at_ms),
        "entrypoint": "cli",
    });
    for (k, v) in body.as_object().unwrap() {
        record[k] = v.clone();
    }
    let mut text = record.to_string();
    text.push('\n');
    text
}

fn assistant(id: &str, content: &Value) -> Value {
    json!({
        "type": "assistant",
        "requestId": format!("req_{id}"),
        "message": {
            "id": format!("msg_{id}"),
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "content": content,
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 2,
                "cache_creation_input_tokens": 30,
                "cache_read_input_tokens": 500,
                "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 0},
                "output_tokens": 20,
                "service_tier": "standard"
            }
        }
    })
}

/// Ten run sessions of one item of `beta` in `b`'s store and its projects directory, inside the
/// 14 days before now; five of them read `src/big.rs`.
async fn seed_b(world: &World) {
    let served = &world.state.identities["b"];
    let (name, repo) = &served.repos[0];
    let cwd = world.dir.path().join("beta").display().to_string();
    let big = world
        .dir
        .path()
        .join("beta")
        .join("src")
        .join("big.rs")
        .display()
        .to_string();
    let projects = world.dir.path().join("b").join("claude").join("projects");
    let dir = projects.join("-synthetic-beta");
    std::fs::create_dir_all(&dir).unwrap();
    let start = crate::overview::now_ms() - DAY;
    let mut n = 0;
    for s in 0..10 {
        let id = session(s);
        let at = start + i64::try_from(s).unwrap() * 60_000;
        let mut text = String::new();
        n += 1;
        let first = assistant(
            &format!("2260000000000000{n:05}"),
            &json!([{"type": "text", "text": "Synthetic."}]),
        );
        text.push_str(&line(&id, &cwd, n, at, &first));
        if s < 5 {
            n += 1;
            let tool = format!("toolu_2260000000000000{n:05}");
            let read = assistant(
                &format!("2260000000000000{n:05}"),
                &json!([{"type": "tool_use", "id": tool, "name": "Read",
                         "input": {"file_path": big}, "caller": {"type": "direct"}}]),
            );
            text.push_str(&line(&id, &cwd, n, at + 1000, &read));
            n += 1;
            let result = json!({
                "type": "user",
                "message": {"role": "user", "content": [
                    {"tool_use_id": tool, "type": "tool_result",
                     "content": "fn synthetic_big() {}\n".repeat(40)}
                ]}
            });
            text.push_str(&line(&id, &cwd, n, at + 2000, &result));
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
    }

    let item = ItemId::new("beta#1");
    let go =
        |item: &Item, event: &Event| -> Stepped { step(item, event, &Ctx::at(start)).unwrap() };
    let claimed = go(&Item::new(item.clone()), &Event::Claim);
    let running = go(&claimed.item, &Event::ClaimConfirmed);
    let mut writes = vec![
        Write::Item(NewItem {
            item: item.clone(),
            name: name.clone(),
            repo: repo.key.clone(),
            card_key: Some("1".to_owned()),
            at_ms: start,
        }),
        Write::Transition {
            stepped: claimed,
            at_ms: start,
        },
        Write::Transition {
            stepped: running,
            at_ms: start + 1,
        },
    ];
    for s in 0..10 {
        writes.push(Write::Run(RunStart {
            item: item.clone(),
            attempt: 1,
            spawn: u32::try_from(s + 1).unwrap(),
            mode: SpawnMode::Fresh,
            provider: "claude".to_owned(),
            session_key: session(s),
            started_ms: start + i64::try_from(s).unwrap() * 60_000,
            start_sha: None,
            effort: None,
        }));
    }
    served.store.writer().write(writes).await.unwrap();
    world.ok("ingest.reingest", json!({"identity": "b"})).await;
}

async fn evaluate(world: &World, identity: &str) -> Value {
    let result = world
        .ok("heat.evaluate", json!({"identity": identity}))
        .await;
    assert_eq!(result["identity"], identity, "{result}");
    let repos = result["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1, "{result}");
    assert_eq!(repos[0]["error"], Value::Null, "{result}");
    repos[0].clone()
}

async fn proposals(world: &World, identity: &str) -> Vec<Value> {
    let list = world
        .ok("heat.list", json!({"identity": identity, "all": true}))
        .await;
    assert_eq!(list["identity"], identity, "{list}");
    list["proposals"].as_array().unwrap().clone()
}

/// #11 D: an evaluation asked for one identity reads and writes that identity's store only; a
/// finding becomes a proposal of the identity that owns the repository at its second
/// evaluation, and a third adds none (`mutant_heat_evaluate_other_identity` evaluates `b` on
/// the calls for `a`).
#[tokio::test]
async fn an_evaluation_stores_proposals_only_in_the_identity_that_owns_the_repository() {
    let world = World::new().await;
    seed_b(&world).await;

    for _ in 0..2 {
        let a = evaluate(&world, "a").await;
        assert_eq!(a["repo"]["name"], "alpha", "{a}");
        assert_eq!(a["findings"], 0, "{a}");
        assert_eq!(a["sightings"], 0, "{a}");
    }
    assert_eq!(proposals(&world, "b").await, Vec::<Value>::new());
    assert_eq!(proposals(&world, "a").await, Vec::<Value>::new());

    let first = evaluate(&world, "b").await;
    assert_eq!(first["repo"]["name"], "beta", "{first}");
    assert_eq!(first["findings"], 1, "{first}");
    assert_eq!(first["sightings"], 1, "{first}");
    assert_eq!(first["promoted"], 0, "{first}");
    assert_eq!(proposals(&world, "b").await, Vec::<Value>::new());

    let second = evaluate(&world, "b").await;
    assert_eq!(second["promoted"], 1, "{second}");
    assert_eq!(second["sightings"], 0, "{second}");
    let listed = proposals(&world, "b").await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let p = &listed[0];
    assert_eq!(p["repo"]["name"], "beta", "{p}");
    assert_eq!(p["detector"], "file_read_across_runs", "{p}");
    assert_eq!(p["provider"], "claude", "{p}");
    assert_eq!(p["subject_kind"], "file", "{p}");
    let repo_key = &world.state.identities["b"].repos[0].1.key;
    let style = style_of(repo_key);
    assert_eq!(
        canonical_path(p["subject"].as_str().unwrap(), style),
        canonical_path("src/big.rs", style),
        "{p}"
    );
    assert_eq!(p["metric"], "file_read_sessions", "{p}");
    assert_eq!(p["unit"], "sessions", "{p}");
    assert_eq!(p["baseline_value"], 5, "{p}");
    assert_eq!(p["threshold"], 5, "{p}");
    assert!(p["target_value"].as_i64().unwrap() < 5, "{p}");
    let (from, to) = (
        p["window_from_ms"].as_i64().unwrap(),
        p["window_to_ms"].as_i64().unwrap(),
    );
    assert_eq!(to - from, 14 * DAY, "{p}");
    assert_eq!(p["estimate"], false, "{p}");
    assert_eq!(p["state"], "open", "{p}");
    assert_eq!(p["seen_count"], 2, "{p}");
    let evidence: Vec<&str> = p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    for s in 0..5 {
        let named = format!("session {}", session(s));
        assert!(evidence.contains(&named.as_str()), "{evidence:?}");
    }

    let third = evaluate(&world, "b").await;
    assert_eq!(third["seen"], 1, "{third}");
    assert_eq!(third["sightings"], 0, "{third}");
    let listed = proposals(&world, "b").await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["seen_count"], 3, "{listed:?}");

    let list_a = world
        .ok("heat.list", json!({"identity": "a", "all": true}))
        .await;
    assert_eq!(list_a["proposals"], json!([]), "{list_a}");
    let text = list_a.to_string();
    assert!(!text.contains("beta") && !text.contains("big.rs"), "{text}");
    world.close().await;
}

/// `b`'s state of its one proposal, from `heat.list` with `all`.
async fn only(world: &World) -> Value {
    let listed = proposals(world, "b").await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    listed[0].clone()
}

/// #11 D: `heat.dismiss` takes the operator token; a dismissed proposal leaves the active list,
/// stays under `all` with its reason, and its subject is in cooldown at the next evaluation.
/// Another identity cannot dismiss it, and it cannot be dismissed twice
/// (`mutant_heat_dismiss_without_token` skips the token).
#[tokio::test]
async fn a_dismissed_proposal_leaves_the_list_and_needs_the_operator_token() {
    use bosun_api::rpc::code;

    let world = World::new().await;
    seed_b(&world).await;
    evaluate(&world, "b").await;
    let second = evaluate(&world, "b").await;
    assert_eq!(second["promoted"], 1, "{second}");
    let proposal = only(&world).await;
    assert_eq!(proposal["state"], "open", "{proposal}");
    let key = proposal["key"].as_str().unwrap().to_owned();
    assert_eq!(key.len(), 16, "{key}");

    let wrong = json!({"operator": "not-the-token", "identity": "b", "key": key,
                       "reason": "synthetic"});
    assert_eq!(
        world.refused("heat.dismiss", wrong).await,
        i64::from(code::UNAUTHORIZED)
    );
    assert_eq!(only(&world).await["state"], "open");

    let params = json!({"operator": crate::notify_tests::TOKEN, "identity": "b", "key": key,
                        "reason": "not worth it"});
    let dismissed = world.ok("heat.dismiss", params.clone()).await;
    assert_eq!(dismissed["key"], key.as_str(), "{dismissed}");
    assert_eq!(dismissed["state"], "dismissed", "{dismissed}");
    assert_eq!(dismissed["dismiss_reason"], "not worth it", "{dismissed}");

    let active = world.ok("heat.list", json!({"identity": "b"})).await;
    assert_eq!(active["proposals"], json!([]), "{active}");
    let all = only(&world).await;
    assert_eq!(all["state"], "dismissed", "{all}");
    assert_eq!(all["dismiss_reason"], "not worth it", "{all}");

    let third = evaluate(&world, "b").await;
    assert_eq!(third["findings"], 1, "{third}");
    assert_eq!(third["cooled_down"], 1, "{third}");
    assert_eq!(third["sightings"], 0, "{third}");
    assert_eq!(only(&world).await["state"], "dismissed");

    let mut for_a = params.clone();
    for_a["identity"] = json!("a");
    assert_eq!(
        world.refused("heat.dismiss", for_a).await,
        i64::from(code::INVALID_PARAMS)
    );
    assert_eq!(only(&world).await, all);

    let again = world.call("heat.dismiss", params).await.unwrap_err();
    assert_eq!(again["code"], code::INVALID_PARAMS, "{again}");
    assert!(
        again["message"].as_str().unwrap().contains("dismissed"),
        "{again}"
    );
    assert_eq!(only(&world).await, all);
    world.close().await;
}

/// #11 D: `heat.land` takes the operator token and a commit id of 7 to 40 hex digits; the
/// landed proposal lists as `landed` by hand with its commit and its follow-up due one window
/// later, and one annotation names it in the store of the identity that owns it, none in the
/// other's. A further evaluation leaves it alone; it cannot land twice, nor for another
/// identity (`mutant_heat_land_no_annotation` writes no annotation).
#[tokio::test]
async fn a_manual_landing_is_recorded_with_an_annotation() {
    use bosun_api::rpc::code;

    let world = World::new().await;
    seed_b(&world).await;
    evaluate(&world, "b").await;
    let second = evaluate(&world, "b").await;
    assert_eq!(second["promoted"], 1, "{second}");
    let proposal = only(&world).await;
    assert_eq!(proposal["state"], "open", "{proposal}");
    let key = proposal["key"].as_str().unwrap().to_owned();
    let sha = "0123456789abcdef0123456789abcdef01234567";

    let wrong = json!({"operator": "not-the-token", "identity": "b", "key": key, "sha": sha});
    assert_eq!(
        world.refused("heat.land", wrong).await,
        i64::from(code::UNAUTHORIZED)
    );
    assert_eq!(only(&world).await, proposal);
    let bad_sha =
        json!({"operator": crate::notify_tests::TOKEN, "identity": "b", "key": key, "sha": "xyz"});
    assert_eq!(
        world.refused("heat.land", bad_sha).await,
        i64::from(code::INVALID_PARAMS)
    );
    assert_eq!(only(&world).await, proposal);

    let params =
        json!({"operator": crate::notify_tests::TOKEN, "identity": "b", "key": key, "sha": sha});
    let before = crate::overview::now_ms();
    let landed = world.ok("heat.land", params.clone()).await;
    let after = crate::overview::now_ms();
    assert_eq!(landed["key"], key.as_str(), "{landed}");
    assert_eq!(landed["state"], "landed", "{landed}");

    let listed = only(&world).await;
    assert_eq!(listed, landed);
    assert_eq!(listed["state"], "landed", "{listed}");
    assert_eq!(listed["landed_how"], "manual", "{listed}");
    assert_eq!(listed["landed_sha"], sha, "{listed}");
    assert_eq!(listed["item"], Value::Null, "{listed}");
    let landed_ms = listed["landed_ms"].as_i64().unwrap();
    assert!((before..=after).contains(&landed_ms), "{listed}");
    let window =
        listed["window_to_ms"].as_i64().unwrap() - listed["window_from_ms"].as_i64().unwrap();
    assert_eq!(
        listed["followup_due_ms"].as_i64().unwrap(),
        landed_ms + window,
        "{listed}"
    );
    let active = world.ok("heat.list", json!({"identity": "b"})).await;
    assert_eq!(active["proposals"], json!([listed]), "{active}");

    let annotations = |identity: &'static str| {
        let reader = world.state.identities[identity].store.reader().clone();
        async move { reader.annotations(0, i64::MAX).await.unwrap() }
    };
    let b = annotations("b").await;
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].ts_ms, landed_ms, "{b:?}");
    assert!(b[0].text.contains(&key) && b[0].text.contains(sha), "{b:?}");
    assert_eq!(
        b[0].repo.as_deref(),
        Some(world.state.identities["b"].repos[0].1.key.as_str()),
        "{b:?}"
    );
    assert_eq!(annotations("a").await, []);

    let mut for_a = params.clone();
    for_a["identity"] = json!("a");
    assert_eq!(
        world.refused("heat.land", for_a).await,
        i64::from(code::INVALID_PARAMS)
    );
    assert_eq!(annotations("a").await, []);

    let third = evaluate(&world, "b").await;
    assert_eq!(third["findings"], 1, "{third}");
    assert_eq!(third["seen"], 0, "{third}");
    assert_eq!(third["sightings"], 0, "{third}");
    assert_eq!(third["expired"], 0, "{third}");
    assert_eq!(third["landed"], 0, "{third}");
    assert_eq!(only(&world).await, listed);

    let again = world.call("heat.land", params).await.unwrap_err();
    assert_eq!(again["code"], code::INVALID_PARAMS, "{again}");
    assert!(
        again["message"].as_str().unwrap().contains("landed"),
        "{again}"
    );
    assert_eq!(only(&world).await, listed);
    assert_eq!(annotations("b").await.len(), 1);
    world.close().await;
}
