//! The scheduler of #5 and #7: which Ready card to claim next.
//!
//! Board order within Ready, skipping cards with an open `blocked-by`. A dependency cycle
//! sends every Ready card in it to Needs you (a card merely blocked by a cycle waits).
//! Concurrency is one item per repository. A pause (global or per repository), the kill
//! switch and the daily cap hold new claims back.

use std::collections::{BTreeMap, BTreeSet};

use crate::item::{Hold, ItemId};

/// A card on the board, as the scheduler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The item.
    pub item: ItemId,
    /// The repository it belongs to.
    pub repo: String,
    /// It is in Ready and not claimed. Other cards are passed for their `blocked-by` edges.
    pub ready: bool,
    /// Its `blocked-by` list.
    pub blocked_by: Vec<ItemId>,
}

/// The gates the scheduler and the item machine consult.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gates {
    /// The kill switch is thrown.
    pub killed: bool,
    /// A global pause.
    pub paused: bool,
    /// Per-repository pauses.
    pub paused_repos: BTreeSet<String>,
    /// The daily cap holds the queue until this time (ms since the Unix epoch; local midnight).
    pub daily_cap_until_ms: Option<i64>,
    /// Repositories with an item in flight (claimed, running, parked, verifying, landing).
    pub busy_repos: BTreeSet<String>,
}

impl Gates {
    /// The hold active for `repo` at `now_ms`, strongest first: kill, pause, daily cap.
    #[must_use]
    pub fn hold(&self, repo: &str, now_ms: i64) -> Option<Hold> {
        if self.killed {
            Some(Hold::Kill)
        } else if self.paused || self.paused_repos.contains(repo) {
            Some(Hold::Pause)
        } else if self.daily_cap_until_ms.is_some_and(|t| now_ms < t) {
            Some(Hold::DailyCap)
        } else {
            None
        }
    }
}

/// What the scheduler decided, each list in board order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Cards to claim now: at most one per repository.
    pub claim: Vec<ItemId>,
    /// Ready cards in a `blocked-by` cycle: to Needs you.
    pub cycles: Vec<ItemId>,
    /// Ready cards waiting on an open blocker, each with its open blockers.
    pub blocked: Vec<Blocked>,
    /// Claimable cards held back by a hold.
    pub held: Vec<ItemId>,
}

/// A Ready card waiting on open blockers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// The waiting card.
    pub item: ItemId,
    /// Its open blockers, in the card's `blocked-by` order.
    pub open: Vec<ItemId>,
}

/// Plans the next claims over `cards` in board order. `satisfied` holds the blockers whose
/// code is on the target branch (Done for the `local` mode); any other blocker is open,
/// including one not on the board.
#[must_use]
pub fn plan(cards: &[Card], satisfied: &BTreeSet<ItemId>, gates: &Gates, now_ms: i64) -> Plan {
    let mut plan = Plan::default();
    if gates.killed {
        // The kill switch writes nothing further to any board.
        return plan;
    }
    let open: BTreeMap<&ItemId, Vec<&ItemId>> = cards
        .iter()
        .map(|c| {
            let open = c
                .blocked_by
                .iter()
                .filter(|b| !satisfied.contains(*b))
                .collect();
            (&c.item, open)
        })
        .collect();
    let mut busy = gates.busy_repos.clone();
    for card in cards.iter().filter(|c| c.ready) {
        if in_cycle(&card.item, &open) {
            plan.cycles.push(card.item.clone());
        } else if !cfg!(mutant_core_ignore_blockers) && !open[&card.item].is_empty() {
            plan.blocked.push(Blocked {
                item: card.item.clone(),
                open: open[&card.item].iter().map(|b| (*b).clone()).collect(),
            });
        } else if busy.contains(&card.repo) {
            // One item at a time per repository; the card waits its turn.
        } else if gates.hold(&card.repo, now_ms).is_some() {
            plan.held.push(card.item.clone());
        } else {
            busy.insert(card.repo.clone());
            plan.claim.push(card.item.clone());
        }
    }
    plan
}

/// `item` reaches itself through open `blocked-by` edges.
fn in_cycle(item: &ItemId, open: &BTreeMap<&ItemId, Vec<&ItemId>>) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&ItemId> = open.get(item).cloned().unwrap_or_default();
    while let Some(next) = stack.pop() {
        if next == item {
            return true;
        }
        if seen.insert(next)
            && let Some(edges) = open.get(next)
        {
            stack.extend(edges.iter().copied());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ItemId {
        ItemId::new(format!("local:demo#{n}"))
    }

    fn card(n: u32, blocked_by: &[u32]) -> Card {
        Card {
            item: id(n),
            repo: "demo".to_owned(),
            ready: true,
            blocked_by: blocked_by.iter().map(|&b| id(b)).collect(),
        }
    }

    fn ids(ns: &[u32]) -> Vec<ItemId> {
        ns.iter().map(|&n| id(n)).collect()
    }

    fn waiting(p: &Plan) -> Vec<ItemId> {
        p.blocked.iter().map(|b| b.item.clone()).collect()
    }

    #[test]
    fn a_blocked_card_carries_its_open_blockers() {
        // #1 waits on #9 (open) and #8 (satisfied); #2 on #9 and #7 (both open).
        let cards = [card(1, &[9, 8]), card(2, &[9, 7])];
        let p = plan(&cards, &BTreeSet::from([id(8)]), &Gates::default(), 0);
        assert_eq!(
            p.blocked,
            vec![
                Blocked {
                    item: id(1),
                    open: ids(&[9]),
                },
                Blocked {
                    item: id(2),
                    open: ids(&[9, 7]),
                },
            ]
        );
    }

    #[test]
    fn order_is_board_order_skipping_open_blockers() {
        // #1 waits on #9 (not Done); #2 waits on #8 (Done); #3 is free.
        let cards = [card(1, &[9]), card(2, &[8]), card(3, &[])];
        let satisfied = BTreeSet::from([id(8)]);
        let p = plan(&cards, &satisfied, &Gates::default(), 0);
        assert_eq!(p.claim, ids(&[2]), "board order, one per repository");
        assert_eq!(waiting(&p), ids(&[1]));

        let p = plan(&cards[..1], &satisfied, &Gates::default(), 0);
        assert_eq!(p.claim, ids(&[]), "an open blocker is never claimed");
    }

    #[test]
    fn a_cycle_sends_exactly_its_members_to_needs_you() {
        // 1 -> 2 -> 3 -> 1 is a cycle; 4 waits on the cycle; 5 is a self-loop; 6 is free.
        let cards = [
            card(4, &[1]),
            card(1, &[2]),
            card(2, &[3]),
            card(3, &[1]),
            card(5, &[5]),
            card(6, &[]),
        ];
        let p = plan(&cards, &BTreeSet::new(), &Gates::default(), 0);
        assert_eq!(p.cycles, ids(&[1, 2, 3, 5]));
        assert_eq!(waiting(&p), ids(&[4]));
        assert_eq!(p.claim, ids(&[6]));
    }

    #[test]
    fn a_satisfied_edge_breaks_a_cycle() {
        let cards = [card(1, &[2]), card(2, &[1])];
        let p = plan(&cards, &BTreeSet::from([id(1)]), &Gates::default(), 0);
        assert_eq!((p.cycles, p.claim), (ids(&[]), ids(&[2])));
    }

    #[test]
    fn one_item_per_repository() {
        let mut other = card(2, &[]);
        other.repo = "other".to_owned();
        let cards = [card(1, &[]), card(3, &[]), other];
        let p = plan(&cards, &BTreeSet::new(), &Gates::default(), 0);
        assert_eq!(p.claim, ids(&[1, 2]));

        let gates = Gates {
            busy_repos: BTreeSet::from(["demo".to_owned()]),
            ..Gates::default()
        };
        assert_eq!(plan(&cards, &BTreeSet::new(), &gates, 0).claim, ids(&[2]));
    }

    #[test]
    fn holds_keep_claims_back() {
        let mut other = card(2, &[]);
        other.repo = "other".to_owned();
        let cards = [card(1, &[]), other];
        let none = BTreeSet::new();

        let paused = Gates {
            paused_repos: BTreeSet::from(["demo".to_owned()]),
            ..Gates::default()
        };
        let p = plan(&cards, &none, &paused, 0);
        assert_eq!((p.claim, p.held), (ids(&[2]), ids(&[1])));

        let capped = Gates {
            daily_cap_until_ms: Some(100),
            ..Gates::default()
        };
        assert_eq!(plan(&cards, &none, &capped, 99).claim, ids(&[]));
        assert_eq!(plan(&cards, &none, &capped, 100).claim, ids(&[1, 2]));

        let killed = Gates {
            killed: true,
            ..Gates::default()
        };
        assert_eq!(plan(&cards, &none, &killed, 0), Plan::default());
        assert_eq!(killed.hold("demo", 0), Some(Hold::Kill));
    }

    #[test]
    fn non_ready_cards_count_for_edges_only() {
        let mut backlog = card(2, &[1]);
        backlog.ready = false;
        let cards = [card(1, &[2]), backlog];
        let p = plan(&cards, &BTreeSet::new(), &Gates::default(), 0);
        assert_eq!(
            p.cycles,
            ids(&[1]),
            "only the Ready member is the daemon's to move"
        );
    }
}
