//! The Ready cards that wait on an open blocker (#5, #7), per repository.
//!
//! Each loop sets its repository's list after every plan (`executor::Loop::schedule`); the
//! list stands until the next plan, also while an item is in flight. `status.overview`,
//! `item.get`, `queue.get` and the blocker's Needs-you comment read it. A repository whose
//! loop never planned has no entry.
//!
//! Beside it, per repository, the Ready cards that wait on a failing resource probe (#8): the
//! loop sets that list after every plan that reached a claim, empty when none waits.
//! `status.overview` and `item.get` show it (#205) to the repository's own identity only, each
//! card with its own identity's probe result (#2).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use bosun_api::rpc::ErrorObject;
use bosun_api::{OpenBlocker, UnhealthyResource, WaitingCard};
use bosun_core::{Blocked, ItemId};
use bosun_store::StoredItem;

use crate::state::{Served, State};

/// The waiting cards of every repository of the daemon.
#[derive(Debug, Default)]
pub(crate) struct Blockers {
    inner: Mutex<BTreeMap<String, Vec<Blocked>>>,
    /// The cards that wait on a failing probe, by repository.
    probing: Mutex<BTreeMap<String, Vec<ProbeWait>>>,
}

/// A Ready card that waits on a failing resource probe (#8): not claimed, no session spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeWait {
    pub(crate) item: ItemId,
    /// The first failing instance of those it would lease.
    pub(crate) resource: String,
    /// That instance's failing streak, as the card's identity probed it.
    pub(crate) streak: crate::health::Streak,
}

impl Blockers {
    /// Replaces the list of `repo` (its configured name).
    pub(crate) fn set(&self, repo: &str, list: Vec<Blocked>) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.insert(repo.to_owned(), list);
    }

    /// Replaces the list of `repo`'s cards that wait on a failing probe.
    pub(crate) fn set_probing(&self, repo: &str, list: Vec<ProbeWait>) {
        let mut probing = self.probing.lock().unwrap_or_else(PoisonError::into_inner);
        probing.insert(repo.to_owned(), list);
    }

    /// The cards of `repo` that wait on a failing probe, in board order.
    pub(crate) fn probing_of(&self, repo: &str) -> Vec<ProbeWait> {
        let probing = self.probing.lock().unwrap_or_else(PoisonError::into_inner);
        probing.get(repo).cloned().unwrap_or_default()
    }

    /// The waiting cards of `repo`, in board order.
    pub(crate) fn of(&self, repo: &str) -> Vec<Blocked> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.get(repo).cloned().unwrap_or_default()
    }

    /// Every repository's list, by name.
    pub(crate) fn all(&self) -> BTreeMap<String, Vec<Blocked>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The open blockers of `item` in `repo`; empty when it does not wait.
    pub(crate) fn blockers_of(&self, repo: &str, item: &ItemId) -> Vec<ItemId> {
        self.of(repo)
            .into_iter()
            .find(|b| b.item == *item)
            .map_or_else(Vec::new, |b| b.open)
    }

    /// The cards of `repo` that wait on `blocker`, in board order.
    pub(crate) fn dependants_of(&self, repo: &str, blocker: &ItemId) -> Vec<ItemId> {
        self.of(repo)
            .into_iter()
            .filter(|b| b.open.contains(blocker))
            .map(|b| b.item)
            .collect()
    }
}

/// The open blockers `open`, each with its state in `items` (the identity's store).
pub(crate) fn open_blockers(open: &[ItemId], items: &[StoredItem]) -> Vec<OpenBlocker> {
    open.iter()
        .map(|b| OpenBlocker {
            item: b.to_string(),
            state: items
                .iter()
                .find(|i| i.item.id == *b)
                .map(|i| i.item.state.as_str().to_owned()),
        })
        .collect()
}

/// What the API shows of a card's failing probe: the instance, since when, what it said.
fn unhealthy(wait: &ProbeWait) -> UnhealthyResource {
    UnhealthyResource {
        resource: wait.resource.clone(),
        since_ms: wait.streak.since_ms,
        detail: wait.streak.detail.clone(),
    }
}

/// The waiting cards of `repo`, one of `served`'s repositories, with their blockers' states:
/// the blocked cards, then the cards that wait on a failing probe (#205). A card in both lists
/// is listed once, with both.
pub(crate) fn waiting_in(state: &State, repo: &str, items: &[StoredItem]) -> Vec<WaitingCard> {
    let lists = if cfg!(mutant_api_waiting_across_identities) {
        // Mutant (`mutant_api_waiting_across_identities`): every repository's waiting cards,
        // not only the identity's.
        state.blockers.all().into_values().flatten().collect()
    } else {
        state.blockers.of(repo)
    };
    let mut cards: Vec<(ItemId, WaitingCard)> = lists
        .into_iter()
        .map(|b| {
            let card = WaitingCard {
                item: b.item.to_string(),
                blockers: open_blockers(&b.open, items),
                unhealthy: Vec::new(),
            };
            (b.item, card)
        })
        .collect();
    // Mutant (`mutant_api_probe_wait_unnamed`): the cards that wait on a probe are left out.
    if !cfg!(mutant_api_probe_wait_unnamed) {
        for wait in state.blockers.probing_of(repo) {
            if let Some((_, card)) = cards.iter_mut().find(|(id, _)| *id == wait.item) {
                card.unhealthy.push(unhealthy(&wait));
            } else {
                let card = WaitingCard {
                    item: wait.item.to_string(),
                    blockers: Vec::new(),
                    unhealthy: vec![unhealthy(&wait)],
                };
                cards.push((wait.item, card));
            }
        }
    }
    cards.into_iter().map(|(_, card)| card).collect()
}

/// The failing probes `item`, stored in `served`'s store, waits on in Ready (#205); empty when
/// it waits on none. Only `served`'s own repositories and probes are read (#2).
pub(crate) fn unhealthy_of(
    state: &State,
    served: &Served,
    item: &StoredItem,
) -> Vec<UnhealthyResource> {
    // Mutant (`mutant_api_probe_wait_unnamed`): `item.get` leaves the failing probes out.
    if cfg!(mutant_api_probe_wait_unnamed) {
        return Vec::new();
    }
    let Some((name, _)) = served
        .repos
        .iter()
        .find(|(name, repo)| crate::executor::of_repo(item, name, repo))
    else {
        return Vec::new();
    };
    state
        .blockers
        .probing_of(name)
        .iter()
        .filter(|w| w.item == item.item.id)
        .map(unhealthy)
        .collect()
}

/// The open blockers of `item`, stored in `served`'s store, while it waits on them.
pub(crate) async fn blocked_by(
    state: &State,
    served: &Served,
    item: &StoredItem,
) -> Result<Vec<OpenBlocker>, ErrorObject> {
    let Some((name, _)) = served
        .repos
        .iter()
        .find(|(name, repo)| crate::executor::of_repo(item, name, repo))
    else {
        return Ok(Vec::new());
    };
    let open = state.blockers.blockers_of(name, &item.item.id);
    if open.is_empty() {
        return Ok(Vec::new());
    }
    let reader = served.store.reader();
    let mut items = Vec::new();
    for b in &open {
        if let Some(found) = reader.item(b).await.map_err(crate::api::failed)? {
            items.push(found);
        }
    }
    Ok(open_blockers(&open, &items))
}
