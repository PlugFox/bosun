//! `link` and `plan` over synthetic proposals, items and landing rows built here (repository
//! `/fixture/demo`, items `local:demo#N`): the returned writes are inspected, nothing is stored.

use bosun_core::proposal::{LandedHow, ProposalState};
use bosun_core::{Item, ItemId, Outcome, Reason, State};
use bosun_store::{LandingRow, PrState, ProposalRow, ProposalStep, StoredItem, Write};

use super::{LinkItem, link, marker, plan};
use crate::reconcile::tests::{NOW, REPO, WINDOW, finding, row};

const DAY: i64 = 24 * 60 * 60 * 1000;
/// The proposal's creation.
const T0: i64 = NOW - 3 * DAY;
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn proposal(subject: &str, created_ms: i64, state: ProposalState, seen: u64) -> ProposalRow {
    row(&finding(subject, Some(100)), created_ms, state, seen)
}

fn stored(n: u32, state: State, card_key: Option<&str>) -> StoredItem {
    let mut item = Item::new(ItemId::new(format!("local:demo#{n}")));
    item.state = state;
    StoredItem {
        item,
        repo: REPO.to_owned(),
        repo_name: Some("demo".to_owned()),
        card_key: card_key.map(str::to_owned),
        snapshot_hash: None,
        created_ms: T0 - DAY,
        updated_ms: T0,
    }
}

fn landing_row(n: u32, delivered_ms: Option<i64>, pr_state: Option<PrState>) -> LandingRow {
    LandingRow {
        item: ItemId::new(format!("local:demo#{n}")),
        attempt: 1,
        target: "main".to_owned(),
        head_sha: Some(SHA.to_owned()),
        verified_tree: None,
        landed_sha: Some(SHA.to_owned()),
        delivered_ms,
        pr_number: pr_state.map(|_| 7),
        pr_url: None,
        pr_state,
        updated_ms: T0 + 5 * DAY,
    }
}

fn one<'a>(
    item: &'a StoredItem,
    done_ms: Option<i64>,
    body: Option<&'a str>,
    landing: Option<&'a LandingRow>,
) -> LinkItem<'a> {
    LinkItem {
        item,
        done_ms,
        body,
        landing,
    }
}

/// The `Landed` step of `writes[0]` and the annotation of `writes[1]`, the only two writes.
fn landing_of(writes: &[Write]) -> (&str, &ProposalStep, i64, &str, i64, &str) {
    assert_eq!(writes.len(), 2, "{writes:#?}");
    let Write::ProposalStep { key, step, at_ms } = &writes[0] else {
        panic!("{writes:#?}");
    };
    let Write::Annotation { ts_ms, repo, text } = &writes[1] else {
        panic!("{writes:#?}");
    };
    assert_eq!(repo, REPO);
    (key, step, *at_ms, text, *ts_ms, repo)
}

fn landed_fields(step: &ProposalStep) -> (Option<&ItemId>, Option<&str>, LandedHow, i64, i64) {
    let ProposalStep::Landed {
        item,
        sha,
        how,
        landed_ms,
        followup_due_ms,
    } = step
    else {
        panic!("{step:?}");
    };
    (
        item.as_ref(),
        sha.as_deref(),
        *how,
        *landed_ms,
        *followup_due_ms,
    )
}

/// #11 D: a proposal lands by an item of its card or by a `Proposal: <key>` line in the card,
/// only when the item is done with a pass after the proposal was created, and never a sighting
/// or a row already landed or ended (`mutant_heat_land_without_item_done` lands by an item in
/// any state, `mutant_heat_land_no_annotation` writes no annotation).
#[test]
// Long: one case after another, each a few lines.
#[allow(clippy::too_many_lines)]
fn a_proposal_lands_only_when_its_item_is_done_with_a_pass() {
    let open = proposal("src/big.rs", T0, ProposalState::Open, 2);
    let key = open.key.clone();
    assert_eq!(key.len(), 16, "{key}");
    assert_eq!(open.window_to_ms - open.window_from_ms, 14 * DAY);
    let line = format!("Proposal: {key}");
    let body = format!("# Synthetic card\n\nParent: #1\n{line}\n");
    let after = T0 + 2 * DAY;

    // Not done: needs you.
    let needs_you = stored(1, State::NeedsYou(Reason::Question), None);
    assert_eq!(
        link(
            std::slice::from_ref(&open),
            &[one(&needs_you, None, Some(&body), None)]
        ),
        []
    );
    // Done, but closed by the user.
    let closed = stored(2, State::Done(Outcome::UserClosed), None);
    assert_eq!(
        link(
            std::slice::from_ref(&open),
            &[one(&closed, Some(after), Some(&body), None)]
        ),
        []
    );
    // Done with a pass, but before the proposal existed.
    let early = stored(3, State::Done(Outcome::Pass), None);
    assert_eq!(
        link(
            std::slice::from_ref(&open),
            &[one(&early, Some(T0 - 1), Some(&body), None)]
        ),
        []
    );

    // Done with a pass after T0, the line written as a bold list item, delivered.
    let delivered_at = after + 60_000;
    let pass = stored(4, State::Done(Outcome::Pass), None);
    let delivered = landing_row(4, Some(delivered_at), None);
    let bold = format!("# Synthetic card\n\n- **Proposal:** {key}\n");
    let all = [
        one(&needs_you, None, Some(&body), None),
        one(&closed, Some(after), Some(&body), None),
        one(&early, Some(T0 - 1), Some(&body), None),
        one(&pass, Some(after), Some(&bold), Some(&delivered)),
    ];
    let writes = link(std::slice::from_ref(&open), &all);
    let (k, step, at_ms, text, ts_ms, _) = landing_of(&writes);
    assert_eq!(k, key);
    let (item, sha, how, landed_ms, due) = landed_fields(step);
    assert_eq!(item, Some(&pass.item.id));
    assert_eq!(sha, Some(SHA));
    assert_eq!(how, LandedHow::Marker);
    assert_eq!(
        (landed_ms, at_ms, ts_ms),
        (delivered_at, delivered_at, delivered_at)
    );
    assert_eq!(due, delivered_at + 14 * DAY);
    assert!(text.contains(&key) && text.contains(SHA), "{text}");
    assert!(text.contains("marker"), "{text}");

    // A pull request merged later: at the time its row last changed.
    let merged = landing_row(4, None, Some(PrState::Merged));
    let writes = link(
        std::slice::from_ref(&open),
        &[one(&pass, Some(after), Some(&line), Some(&merged))],
    );
    let (_, step, ..) = landing_of(&writes);
    let (_, sha, _, landed_ms, due) = landed_fields(step);
    assert_eq!((sha, landed_ms), (Some(SHA), merged.updated_ms));
    assert_eq!(due, merged.updated_ms + 14 * DAY);
    // Still open: not landed yet.
    let pending = landing_row(4, None, Some(PrState::Open));
    assert_eq!(
        link(
            std::slice::from_ref(&open),
            &[one(&pass, Some(after), Some(&line), Some(&pending))]
        ),
        []
    );

    // By its card: no landing row, at the time the item reached done.
    let mut carded = proposal("src/other.rs", T0, ProposalState::Published, 3);
    carded.card_key = Some("42".to_owned());
    let card = stored(5, State::Done(Outcome::Pass), Some("42"));
    let other_card = stored(6, State::Done(Outcome::Pass), Some("43"));
    let writes = link(
        std::slice::from_ref(&carded),
        &[
            one(&other_card, Some(after), None, None),
            one(&card, Some(after), None, None),
        ],
    );
    let (k, step, _, text, ts_ms, _) = landing_of(&writes);
    assert_eq!(k, carded.key);
    let (item, sha, how, landed_ms, due) = landed_fields(step);
    assert_eq!(item, Some(&card.item.id));
    assert_eq!((sha, how), (None, LandedHow::Item));
    assert_eq!((landed_ms, ts_ms), (after, after));
    assert_eq!(due, after + WINDOW);
    assert!(text.contains(&carded.key), "{text}");
    assert!(text.ends_with("no commit"), "{text}");

    // The same marker for a sighting, a landed proposal and a dismissed one: nothing.
    let marked = [one(&pass, Some(after), Some(&line), None)];
    for (state, seen) in [
        (ProposalState::Open, 1),
        (ProposalState::Landed, 2),
        (ProposalState::Dismissed, 2),
        (ProposalState::Expired, 2),
    ] {
        let mut other = open.clone();
        other.state = state;
        other.seen_count = seen;
        assert_eq!(link(&[other], &marked), [], "{state:?} {seen}");
    }
    // Lines that are no markers.
    for body in [
        "Proposal: not-a-key".to_owned(),
        format!("Proposals: {key}"),
        format!("Proposal {key}"),
        format!("See proposal: {key}"),
    ] {
        assert_eq!(
            link(
                std::slice::from_ref(&open),
                &[one(&pass, Some(after), Some(&body), None)]
            ),
            [],
            "{body}"
        );
    }
    assert_eq!(
        marker(&format!("* proposal: {key} (synthetic)")),
        Some(key.as_str())
    );
    assert_eq!(marker(&format!("**Proposal**: {key}")), Some(key.as_str()));
    assert_eq!(marker(&format!("**Proposal:** {key}")), Some(key.as_str()));
    assert_eq!(marker("Proposal: 0123456789abcdef0"), None);
}

/// #11 D: an evaluation that lands a proposal writes its landing and nothing else for it, even
/// when its finding is here again; another proposal's finding is still seen
/// (`mutant_heat_seen_after_landing` reconciles with the rows as stored, before the landings).
#[test]
fn a_proposal_seen_again_in_the_evaluation_that_lands_it_is_only_landed() {
    let created = NOW - DAY;
    let big = proposal("src/big.rs", created, ProposalState::Open, 2);
    let other = proposal("src/other.rs", created, ProposalState::Open, 2);
    let findings = [
        finding("src/big.rs", Some(100)),
        finding("src/other.rs", Some(50)),
    ];
    let existing = [big.clone(), other.clone()];
    let body = format!("Proposal: {}\n", big.key);
    let item = stored(1, State::Done(Outcome::Pass), None);
    let items = [one(&item, Some(NOW - 60_000), Some(&body), None)];

    let planned = plan(&findings, &existing, &items, NOW);
    let writes = &planned.writes;
    assert!(writes.len() >= 2, "{writes:#?}");
    let (k, step, ..) = landing_of(&writes[..2]);
    assert_eq!(k, big.key);
    assert_eq!(landed_fields(step).2, LandedHow::Marker);
    for w in &writes[2..] {
        let key = match w {
            Write::ProposalSeen { key, .. } | Write::ProposalStep { key, .. } => key,
            Write::ProposalOpened(new) => &new.key,
            other => panic!("{other:?}"),
        };
        assert_ne!(key, &big.key, "{writes:#?}");
    }
    assert!(
        writes.iter().any(
            |w| matches!(w, Write::ProposalSeen { key, at_ms } if *key == other.key && *at_ms == NOW)
        ),
        "{writes:#?}"
    );
    assert_eq!(planned.summary.landed, 1);
    assert_eq!(planned.summary.seen, 1);

    // Without the item, the proposal is seen again.
    let planned = plan(&findings, &existing, &[], NOW);
    assert!(
        planned
            .writes
            .iter()
            .any(|w| matches!(w, Write::ProposalSeen { key, .. } if *key == big.key)),
        "{:#?}",
        planned.writes
    );
    assert_eq!(planned.summary.landed, 0);
    assert_eq!(planned.summary.seen, 2);
}
