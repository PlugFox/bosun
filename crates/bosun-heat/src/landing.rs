//! The landing of heat's proposals (#11, part D, "Remedy landed"): which proposal's remedy an
//! item finished, the writes that record it, and the plan of one evaluation that links the
//! landings before it reconciles the findings. Pure: no I/O, no clock.
//!
//! - Only a **proposal** (`seen_count >= 2`) in state `open` or `published` lands; a sighting,
//!   a landed row and every ended row never do. An improvement nobody landed is `expired`,
//!   never `landed`.
//! - An item counts only when it is done with a pass and reached done at or after the
//!   proposal's creation: an item finished before the proposal existed is not its remedy.
//! - The ways, most precise first: (1) the item's card is the proposal's card
//!   ([`LandedHow::Item`]); (2) the card's text at claim has a line `Proposal: <key>`
//!   ([`LandedHow::Marker`], [`marker`]). Among the items of one way, the earliest landing
//!   wins, then the item id. (3) The user says so (`heat.land`, [`LandedHow::Manual`]).
//! - The landing commit and its time: the landing row's `landed_sha` and `delivered_ms`; for
//!   a pull request merged later, its `landed_sha` and the time the row last changed; without
//!   a landing row (the repository lands in its own checkout), no commit and the time the item
//!   reached done. A landing row of an unmerged pull request has not landed yet.
//! - A landing is the proposal's `Landed` step, its follow-up due one baseline window later,
//!   and one annotation of the repository ([`landed`]).

use std::collections::BTreeMap;

use bosun_core::proposal::{LandedHow, ProposalState};
use bosun_core::{ItemId, Outcome, State};
use bosun_store::{LandingRow, PrState, ProposalRow, ProposalStep, StoredItem, Write};

use crate::detect::Finding;
use crate::reconcile::{Reconciled, reconcile};

/// One item of the repository, with what the linking reads of it.
#[derive(Debug, Clone, Copy)]
pub struct LinkItem<'a> {
    pub item: &'a StoredItem,
    /// The time of its last transition into done.
    pub done_ms: Option<i64>,
    /// The body of its snapshot: the card's text at claim.
    pub body: Option<&'a str>,
    pub landing: Option<&'a LandingRow>,
}

/// The key a line `Proposal: <key>` names, when `line` is one: an optional list marker (`-`,
/// `*`, `+`), optional `**` around the name or the whole prefix, `proposal` in any case, `:`,
/// then the key as the first word, 16 hex digits. Anything else is no marker.
#[must_use]
pub(crate) fn marker(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line
        .strip_prefix(['-', '*', '+'])
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map_or(line, str::trim_start);
    let line = line.strip_prefix("**").unwrap_or(line);
    let name = "proposal";
    if !line
        .get(..name.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let rest = &line[name.len()..];
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    let rest = rest.strip_prefix(':')?;
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    let key = rest.split_whitespace().next()?;
    (key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit())).then_some(key)
}

/// Whether `body` has a marker line naming proposal `key`.
fn names(body: &str, key: &str) -> bool {
    body.lines()
        .filter_map(marker)
        .any(|named| named.eq_ignore_ascii_case(key))
}

/// The landing commit and its time, for an item that reached done at `done_ms`; `None` while
/// its landing has not landed (a pull request not merged yet).
fn commit(landing: Option<&LandingRow>, done_ms: i64) -> Option<(Option<String>, i64)> {
    let Some(row) = landing else {
        return Some((None, done_ms));
    };
    match (&row.landed_sha, row.delivered_ms, row.pr_state) {
        (Some(sha), Some(at), _) => Some((Some(sha.clone()), at)),
        (Some(sha), None, Some(PrState::Merged)) => Some((Some(sha.clone()), row.updated_ms)),
        _ => None,
    }
}

/// The writes of the landing of proposal `row` at `landed_ms`: its `Landed` step, the
/// follow-up due one baseline window later, and one annotation of its repository.
#[must_use]
pub fn landed(
    row: &ProposalRow,
    item: Option<ItemId>,
    sha: Option<String>,
    how: LandedHow,
    landed_ms: i64,
) -> Vec<Write> {
    let text = format!(
        "heat proposal {} landed ({}) at {}",
        row.key,
        how.as_str(),
        sha.as_deref().unwrap_or("no commit")
    );
    let mut writes = vec![Write::ProposalStep {
        key: row.key.clone(),
        step: ProposalStep::Landed {
            item,
            sha,
            how,
            landed_ms,
            followup_due_ms: landed_ms + (row.window_to_ms - row.window_from_ms),
        },
        at_ms: landed_ms,
    }];
    // Mutant (`mutant_heat_land_no_annotation`): a landing writes no annotation.
    if !cfg!(mutant_heat_land_no_annotation) {
        writes.push(Write::Annotation {
            ts_ms: landed_ms,
            repo: row.repo.clone(),
            text,
        });
    }
    writes
}

/// Whether proposal `row` can land: a proposal (not a sighting), open or published.
fn can_land(row: &ProposalRow) -> bool {
    row.seen_count >= 2 && matches!(row.state, ProposalState::Open | ProposalState::Published)
}

/// An item that lands a proposal: how, at what time, by which item and commit.
struct Landing<'a> {
    how: LandedHow,
    at: i64,
    item: &'a ItemId,
    sha: Option<String>,
}

impl Landing<'_> {
    /// The order of preference: the way (most precise first), the time, the item id.
    fn order(&self) -> (bool, i64, &ItemId) {
        (self.how != LandedHow::Item, self.at, self.item)
    }
}

/// The landings of `proposals`, the rows of one repository, by `items`, the items of that
/// repository: for each proposal that lands, its [`landed`] writes. Deterministic for equal
/// input.
#[must_use]
pub fn link(proposals: &[ProposalRow], items: &[LinkItem<'_>]) -> Vec<Write> {
    let mut writes = Vec::new();
    for row in proposals.iter().filter(|row| can_land(row)) {
        let mut best: Option<Landing<'_>> = None;
        for candidate in items {
            let stored = candidate.item;
            // Mutant (`mutant_heat_land_without_item_done`): an item lands its proposal
            // whatever its state and outcome.
            if !cfg!(mutant_heat_land_without_item_done)
                && stored.item.state != State::Done(Outcome::Pass)
            {
                continue;
            }
            let how = if row.card_key.is_some() && row.card_key == stored.card_key {
                LandedHow::Item
            } else if candidate.body.is_some_and(|body| names(body, &row.key)) {
                LandedHow::Marker
            } else {
                continue;
            };
            let Some(done_ms) = candidate.done_ms.filter(|done| *done >= row.created_ms) else {
                continue;
            };
            let Some((sha, at)) = commit(candidate.landing, done_ms) else {
                continue;
            };
            let landing = Landing {
                how,
                at,
                item: &stored.item.id,
                sha,
            };
            if best.as_ref().is_none_or(|b| landing.order() < b.order()) {
                best = Some(landing);
            }
        }
        if let Some(l) = best {
            writes.extend(landed(row, Some(l.item.clone()), l.sha, l.how, l.at));
        }
    }
    writes
}

/// The batch of one evaluation of one repository: the landings of `existing` by `items`
/// ([`link`]), then the reconciliation of `findings` ([`reconcile`]) with the rows as they are
/// after those landings, so a proposal that lands gets no other write in the batch (the
/// machine refuses `Seen` and `Expired` on a landed row, and a refused write fails the batch).
#[must_use]
pub fn plan(
    findings: &[Finding],
    existing: &[ProposalRow],
    items: &[LinkItem<'_>],
    now_ms: i64,
) -> Reconciled {
    let landings = link(existing, items);
    let mut after: BTreeMap<&str, &ProposalStep> = BTreeMap::new();
    for write in &landings {
        if let Write::ProposalStep { key, step, .. } = write {
            after.insert(key, step);
        }
    }
    let landed_rows: Vec<ProposalRow> = existing
        .iter()
        .map(|row| match after.get(row.key.as_str()) {
            Some(ProposalStep::Landed {
                item,
                sha,
                how,
                landed_ms,
                followup_due_ms,
            }) => ProposalRow {
                state: ProposalState::Landed,
                item: item.clone(),
                landed_sha: sha.clone(),
                landed_how: Some(*how),
                landed_ms: Some(*landed_ms),
                followup_due_ms: Some(*followup_due_ms),
                ..row.clone()
            },
            _ => row.clone(),
        })
        .collect();
    // Mutant (`mutant_heat_seen_after_landing`): the findings are reconciled with the rows as
    // stored, before the landings.
    let rows = if cfg!(mutant_heat_seen_after_landing) {
        existing
    } else {
        &landed_rows
    };
    let reconciled = reconcile(findings, rows, now_ms);
    let mut summary = reconciled.summary;
    summary.landed = u64::try_from(after.len()).unwrap_or(u64::MAX);
    let mut writes = landings;
    writes.extend(reconciled.writes);
    Reconciled { writes, summary }
}

#[cfg(test)]
mod tests;
