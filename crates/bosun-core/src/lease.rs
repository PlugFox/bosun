//! The lease model of #8: which holder owns which resource, and who waits for what.
//!
//! A resource has a name, a capacity (1 is exclusive) and a scope that the caller maps to
//! machine, identity or repository; the model does not interpret the scope. A holder is an
//! opaque key (later the shim's process key: pid and start time); the model only compares and
//! orders it.
//!
//! This module performs no I/O and reads no clock: the caller passes the time in, and tells the
//! model which holders are alive. Acquisition is all-or-nothing: a holder takes every resource
//! it needs in one step, or none of them. Resources are taken in one fixed global order (by
//! name), whatever order the caller lists them in. A holder never acquires more while it holds
//! something.
//!
//! A request that cannot be served now waits in one FIFO queue, which is served in arrival
//! order: nothing behind a waiting request is granted ahead of it. A request needing two
//! resources is therefore not starved by a stream of requests for one of them. The model
//! records each request's wait, from enqueue to grant, per resource.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What a resource guards. The model treats every scope alike; the caller chooses the meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The whole machine.
    Machine,
    /// One identity (#2).
    Identity,
    /// One repository.
    Repository,
}

/// A resource that holders lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// The name; resources are ordered by it, and it is how requests refer to them.
    pub name: String,
    /// How many holders it serves at once. 1 is exclusive.
    pub capacity: u32,
    /// What it guards.
    pub scope: Scope,
}

impl Resource {
    /// A resource named `name` with `capacity` units.
    ///
    /// # Panics
    ///
    /// If `capacity` is 0: a resource no holder can lease is a configuration error.
    #[must_use]
    pub fn new(name: impl Into<String>, capacity: u32, scope: Scope) -> Self {
        assert!(capacity >= 1, "a resource needs at least one unit");
        Self {
            name: name.into(),
            capacity,
            scope,
        }
    }
}

/// The answer to an accepted [`Leases::acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// The holder holds every resource it needs.
    Granted,
    /// The request is queued (or, under a mutant, partly taken); call again or wait for a grant.
    Waiting,
}

/// Why [`Leases::acquire`] refused a request outright: it neither grants nor queues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The request names no resource.
    NoResources,
    /// The request names a resource the model does not know.
    UnknownResource(String),
    /// The holder holds a different set of resources. A holder never acquires more while it
    /// holds something.
    Holding,
    /// The holder is already queued for a different set of resources.
    AlreadyQueued,
}

/// One wait, recorded when a holder is granted a resource: how long it waited, in the caller's
/// time units, from the enqueue of its request to this grant. A request served at once waits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait<K> {
    /// The holder.
    pub holder: K,
    /// The resource granted.
    pub resource: String,
    /// The wait, in the caller's time units.
    pub wait: u64,
}

/// A request that could not be served at once. `needs` is deduplicated, in the order given.
#[derive(Debug, Clone)]
struct Request<K> {
    holder: K,
    needs: Vec<String>,
    enqueued: u64,
}

/// The leases of a set of resources, and the queue of requests waiting for them.
#[derive(Debug, Clone)]
pub struct Leases<K> {
    resources: BTreeMap<String, Resource>,
    /// The holders of each resource, by resource name.
    users: BTreeMap<String, BTreeSet<K>>,
    /// The resources each holder holds.
    held: BTreeMap<K, BTreeSet<String>>,
    queue: VecDeque<Request<K>>,
    waits: Vec<Wait<K>>,
}

impl<K: Clone + Ord> Leases<K> {
    /// A model of `resources`, all free.
    ///
    /// # Panics
    ///
    /// If two resources share a name.
    pub fn new(resources: impl IntoIterator<Item = Resource>) -> Self {
        let mut by_name = BTreeMap::new();
        for resource in resources {
            let name = resource.name.clone();
            assert!(
                by_name.insert(name.clone(), resource).is_none(),
                "resource {name} is declared twice"
            );
        }
        let users = by_name
            .keys()
            .map(|n| (n.clone(), BTreeSet::new()))
            .collect();
        Self {
            resources: by_name,
            users,
            held: BTreeMap::new(),
            queue: VecDeque::new(),
            waits: Vec::new(),
        }
    }

    /// Asks for every resource in `needs` at time `now`.
    ///
    /// The request is granted at once if every resource has a free unit and nothing is queued
    /// ahead of it; otherwise it is queued, and the answer is [`Acquire::Waiting`]. Asking again
    /// for the same set while queued is harmless. Asking for the set the holder already holds
    /// returns [`Acquire::Granted`] and changes nothing.
    ///
    /// # Errors
    ///
    /// [`Refused`] if `needs` is empty or names an unknown resource, if the holder holds a
    /// different set, or if it is queued for a different one.
    pub fn acquire(&mut self, holder: K, needs: &[&str], now: u64) -> Result<Acquire, Refused> {
        let wanted = self.validate(needs)?;
        if cfg!(mutant_lease_one_by_one) {
            // Mutant (`mutant_lease_one_by_one`): resources are taken one at a time, in the order
            // `needs` lists them, and a holder keeps what it has while it waits for the rest. Two
            // holders wanting the same pair in opposite orders each take one and block on the other.
            return Ok(self.take_one_by_one(&holder, &wanted, now));
        }
        if self.held.get(&holder).is_some_and(|h| !h.is_empty()) {
            return if self.holds_exactly(&holder, &wanted) {
                Ok(Acquire::Granted)
            } else {
                Err(Refused::Holding)
            };
        }
        if let Some(queued) = self.queue.iter().find(|q| q.holder == holder) {
            return if same_set(&queued.needs, &wanted) {
                Ok(Acquire::Waiting)
            } else {
                Err(Refused::AlreadyQueued)
            };
        }
        if self.queue.is_empty() && wanted.iter().all(|r| self.fits(r)) {
            for r in ordered(&wanted) {
                self.take(&holder, r, now, now);
            }
            return Ok(Acquire::Granted);
        }
        self.queue.push_back(Request {
            holder,
            needs: wanted,
            enqueued: now,
        });
        Ok(Acquire::Waiting)
    }

    /// Releases every lease `holder` has and withdraws its queued request, if any, then serves
    /// the queue at time `now`.
    pub fn release(&mut self, holder: &K, now: u64) {
        self.drop_holder(holder);
        self.serve(now);
    }

    /// Releases the leases of every holder that `alive` says is gone, withdrawing their queued
    /// requests too, then serves the queue at time `now`.
    pub fn release_stale(&mut self, alive: impl Fn(&K) -> bool, now: u64) {
        if cfg!(mutant_lease_keep_dead_holder) {
            // Mutant (`mutant_lease_keep_dead_holder`): a dead holder keeps its leases and its
            // waiting requests keep blocking the queue.
            return;
        }
        let gone: BTreeSet<K> = self
            .held
            .keys()
            .cloned()
            .chain(self.queue.iter().map(|q| q.holder.clone()))
            .filter(|h| !alive(h))
            .collect();
        for holder in &gone {
            self.drop_holder(holder);
        }
        self.serve(now);
    }

    /// The resources `holder` holds.
    #[must_use]
    pub fn holds(&self, holder: &K) -> BTreeSet<String> {
        self.held.get(holder).cloned().unwrap_or_default()
    }

    /// Whether `holder` has a request in the queue.
    #[must_use]
    pub fn is_waiting(&self, holder: &K) -> bool {
        self.queue.iter().any(|q| &q.holder == holder)
    }

    /// How many holders hold `resource` now.
    #[must_use]
    pub fn users(&self, resource: &str) -> usize {
        self.users.get(resource).map_or(0, BTreeSet::len)
    }

    /// Every wait recorded so far, in grant order.
    #[must_use]
    pub fn waits(&self) -> &[Wait<K>] {
        &self.waits
    }

    /// Checks `needs` and returns it deduplicated, in the order given.
    fn validate(&self, needs: &[&str]) -> Result<Vec<String>, Refused> {
        if needs.is_empty() {
            return Err(Refused::NoResources);
        }
        let mut wanted: Vec<String> = Vec::with_capacity(needs.len());
        for &name in needs {
            if !self.resources.contains_key(name) {
                return Err(Refused::UnknownResource(name.to_owned()));
            }
            if !wanted.iter().any(|w| w == name) {
                wanted.push(name.to_owned());
            }
        }
        Ok(wanted)
    }

    fn holds_exactly(&self, holder: &K, wanted: &[String]) -> bool {
        self.held
            .get(holder)
            .is_some_and(|h| h.len() == wanted.len() && wanted.iter().all(|r| h.contains(r)))
    }

    /// Whether `resource` has a free unit.
    fn fits(&self, resource: &str) -> bool {
        let capacity = self.resources[resource].capacity as usize;
        self.users.get(resource).map_or(0, BTreeSet::len) < capacity
    }

    /// Gives `resource` to `holder`, recording its wait. Returns false if the holder already
    /// held it, in which case nothing is recorded.
    fn take(&mut self, holder: &K, resource: &str, enqueued: u64, now: u64) -> bool {
        let newly = self
            .held
            .entry(holder.clone())
            .or_default()
            .insert(resource.to_owned());
        if newly {
            self.users
                .entry(resource.to_owned())
                .or_default()
                .insert(holder.clone());
            self.waits.push(Wait {
                holder: holder.clone(),
                resource: resource.to_owned(),
                wait: now.saturating_sub(enqueued),
            });
        }
        newly
    }

    /// Removes every lease `holder` has and its queued request.
    fn drop_holder(&mut self, holder: &K) {
        if let Some(resources) = self.held.remove(holder) {
            for resource in resources {
                if let Some(users) = self.users.get_mut(&resource) {
                    users.remove(holder);
                }
            }
        }
        self.queue.retain(|q| &q.holder != holder);
    }

    /// Grants queued requests in arrival order, stopping at the first that does not fit.
    fn serve(&mut self, now: u64) {
        while let Some(head) = self.queue.front() {
            if !head.needs.iter().all(|r| self.fits(r)) {
                break;
            }
            let Some(request) = self.queue.pop_front() else {
                break;
            };
            for resource in ordered(&request.needs) {
                self.take(&request.holder, resource, request.enqueued, now);
            }
        }
    }

    /// The mutant's acquisition: one resource per call, in the order given, keeping what it has.
    fn take_one_by_one(&mut self, holder: &K, wanted: &[String], now: u64) -> Acquire {
        let enqueued = if let Some(queued) = self.queue.iter().find(|q| q.holder == *holder) {
            queued.enqueued
        } else {
            self.queue.push_back(Request {
                holder: holder.clone(),
                needs: wanted.to_vec(),
                enqueued: now,
            });
            now
        };
        for resource in wanted {
            let already = self.held.get(holder).is_some_and(|h| h.contains(resource));
            if already {
                continue;
            }
            if self.fits(resource) {
                self.take(holder, resource, enqueued, now);
            }
            break;
        }
        if self.holds_exactly(holder, wanted) {
            self.queue.retain(|q| q.holder != *holder);
            Acquire::Granted
        } else {
            Acquire::Waiting
        }
    }
}

/// The resources of `needs` in the global order: by name.
fn ordered(needs: &[String]) -> BTreeSet<&str> {
    needs.iter().map(String::as_str).collect()
}

fn same_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Holder = &'static str;

    fn pool(names: &[&str]) -> Leases<Holder> {
        Leases::new(names.iter().map(|n| Resource::new(*n, 1, Scope::Machine)))
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stage {
        Want,
        Hold(u8),
        Done,
    }

    #[test]
    fn two_items_wanting_the_same_pair_never_deadlock() {
        // The step bound is the deadlock detector: a run that cannot finish within it is
        // stuck, whatever the reason, and the bound ends it.
        const BOUND: usize = 16;
        let names: [Holder; 2] = ["first", "second"];
        let needs: [[&str; 2]; 2] = [["a", "b"], ["b", "a"]];
        let mut leases = pool(&["a", "b"]);
        let mut stage = [Stage::Want, Stage::Want];
        let mut step = 0;
        let mut clock = 0;

        while stage.iter().any(|s| *s != Stage::Done) {
            assert!(step < BOUND, "no progress within {BOUND} steps: {stage:?}");
            // Who moves first alternates, so neither item always wins the race for `a`.
            let order = if step % 2 == 0 { [0, 1] } else { [1, 0] };
            for i in order {
                stage[i] = match stage[i] {
                    Stage::Want => match leases.acquire(names[i], &needs[i], clock) {
                        Ok(Acquire::Granted) => Stage::Hold(1),
                        Ok(Acquire::Waiting) => Stage::Want,
                        Err(refused) => panic!("{}: {refused:?}", names[i]),
                    },
                    Stage::Hold(0) => {
                        leases.release(&names[i], clock);
                        Stage::Done
                    }
                    Stage::Hold(n) => Stage::Hold(n - 1),
                    Stage::Done => Stage::Done,
                };
            }
            // Never two items at once: at most one of them holds any part of the pair.
            let holding: Vec<Holder> = names
                .iter()
                .copied()
                .filter(|n| !leases.holds(n).is_empty())
                .collect();
            assert!(
                holding.len() <= 1,
                "step {step}: both hold part of the pair: {holding:?}"
            );
            for r in ["a", "b"] {
                assert!(leases.users(r) <= 1, "step {step}: {r} has two holders");
            }
            step += 1;
            clock += 1;
        }
    }

    #[test]
    fn a_lease_whose_holder_is_gone_is_released() {
        let mut leases = pool(&["a", "b"]);
        assert_eq!(leases.acquire("dead", &["a"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("live", &["b"], 1), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("waiter", &["a"], 3), Ok(Acquire::Waiting));

        leases.release_stale(|h| *h != "dead", 10);

        assert_eq!(leases.holds(&"dead"), set(&[]));
        assert_eq!(leases.holds(&"waiter"), set(&["a"]));
        assert!(!leases.is_waiting(&"waiter"));
        assert_eq!(leases.holds(&"live"), set(&["b"]), "a live lease is kept");
        assert!(leases.waits().contains(&Wait {
            holder: "waiter",
            resource: "a".to_owned(),
            wait: 7
        }));
    }

    #[test]
    fn a_dead_holders_queued_request_is_withdrawn() {
        let mut leases = pool(&["a"]);
        assert_eq!(leases.acquire("holder", &["a"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("ghost", &["a"], 1), Ok(Acquire::Waiting));
        assert_eq!(leases.acquire("next", &["a"], 2), Ok(Acquire::Waiting));

        leases.release_stale(|h| *h != "ghost", 5);
        assert!(!leases.is_waiting(&"ghost"));
        leases.release(&"holder", 6);

        assert_eq!(
            leases.holds(&"next"),
            set(&["a"]),
            "the ghost does not get the grant"
        );
    }

    #[test]
    fn a_request_takes_all_its_resources_or_none() {
        let mut leases = pool(&["a", "b"]);
        assert_eq!(leases.acquire("x", &["b"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("y", &["b", "a"], 1), Ok(Acquire::Waiting));

        assert_eq!(
            leases.holds(&"y"),
            set(&[]),
            "y takes nothing while b is busy"
        );
        assert_eq!(leases.users("a"), 0, "a stays free for the queue");
    }

    #[test]
    fn a_request_for_two_resources_is_not_starved_by_one_resource_requests() {
        let mut leases = pool(&["a", "b"]);
        assert_eq!(leases.acquire("holder", &["a"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("pair", &["a", "b"], 1), Ok(Acquire::Waiting));
        // b is free, but the pair is ahead of this request in the queue.
        assert_eq!(leases.acquire("single", &["b"], 2), Ok(Acquire::Waiting));

        leases.release(&"holder", 3);
        assert_eq!(leases.holds(&"pair"), set(&["a", "b"]));
        assert!(leases.is_waiting(&"single"));

        leases.release(&"pair", 5);
        assert_eq!(leases.holds(&"single"), set(&["b"]));
        assert!(leases.waits().contains(&Wait {
            holder: "pair",
            resource: "b".to_owned(),
            wait: 2
        }));
    }

    #[test]
    fn a_resource_with_capacity_serves_that_many_holders_at_once() {
        let mut leases = Leases::new([Resource::new("slots", 2, Scope::Identity)]);
        assert_eq!(leases.acquire("a", &["slots"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("b", &["slots"], 0), Ok(Acquire::Granted));
        assert_eq!(leases.acquire("c", &["slots"], 1), Ok(Acquire::Waiting));
        assert_eq!(leases.users("slots"), 2);

        leases.release(&"a", 4);

        assert_eq!(
            leases.holds(&"c"),
            set(&["slots"]),
            "c is granted the freed unit"
        );
        assert_eq!(leases.users("slots"), 2);
        assert_eq!(
            leases.waits(),
            [
                Wait {
                    holder: "a",
                    resource: "slots".to_owned(),
                    wait: 0
                },
                Wait {
                    holder: "b",
                    resource: "slots".to_owned(),
                    wait: 0
                },
                Wait {
                    holder: "c",
                    resource: "slots".to_owned(),
                    wait: 3
                },
            ]
        );
    }

    #[test]
    fn a_holder_never_acquires_more_while_it_holds_something() {
        let mut leases = pool(&["a", "b"]);
        assert_eq!(leases.acquire("h", &["a"], 0), Ok(Acquire::Granted));
        assert_eq!(
            leases.acquire("h", &["a"], 1),
            Ok(Acquire::Granted),
            "the same set is a no-op"
        );
        assert_eq!(leases.acquire("h", &["b"], 1), Err(Refused::Holding));
        assert_eq!(leases.acquire("h", &["a", "b"], 1), Err(Refused::Holding));
        assert_eq!(leases.holds(&"h"), set(&["a"]));
    }

    #[test]
    fn a_request_naming_nothing_or_an_unknown_resource_is_refused() {
        let mut leases = pool(&["a"]);
        assert_eq!(leases.acquire("h", &[], 0), Err(Refused::NoResources));
        assert_eq!(
            leases.acquire("h", &["a", "zz"], 0),
            Err(Refused::UnknownResource("zz".to_owned()))
        );
        assert!(
            leases.holds(&"h").is_empty(),
            "a refused request takes nothing"
        );
    }
}
