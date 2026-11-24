//! Resource leases of #8: a pure model of who holds what and who waits for what.
//!
//! A [`Resource`] has a name, a capacity (1 is exclusive) and a [`Scope`]. A holder is an
//! opaque key (the shim's process key later); the model compares holders and never interprets
//! them. The model performs no I/O and reads no clock: the caller passes the time (in its own
//! units) and, to [`Leases::release_stale`], the liveness of holders.
//!
//! - [`Leases::acquire`] files a request for a set of resources. A holder files at most one
//!   request and never acquires more while it holds something.
//! - [`Leases::step`] advances one holder's request by one resource, the unit of the real
//!   implementation (one row, one transaction). Resources are taken in one fixed global order,
//!   by name, whatever order the request listed them in; two requests for the same pair can
//!   therefore never each hold one of it, and never deadlock.
//! - The request is granted when it has taken every resource it needs: all or nothing. Until
//!   then it holds no usable lease, and releasing or reaping its holder returns what it took.
//! - A resource without a free slot queues the requests trying it, first come first served, so
//!   a request needing two resources is not starved by a stream of one-resource requests: they
//!   queue behind it. [`Leases::serve`] steps every request in arrival order.
//! - Each grant records the request's wait, from its filing to its grant, for every resource it
//!   needed ([`Leases::waits`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// What a resource is shared within.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The whole machine (a build cache, the CPU).
    Machine,
    /// One identity (a provider account and its quota).
    Identity,
    /// One repository (its working tree, its target branch).
    Repository,
}

/// A leasable resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// The name, unique in a [`Leases`]; it fixes the global order resources are taken in.
    pub name: String,
    /// How many holders may hold it at once; 1 is exclusive.
    pub capacity: usize,
    /// What it is shared within.
    pub scope: Scope,
}

impl Resource {
    /// A resource named `name`.
    #[must_use]
    pub fn new(name: impl Into<String>, capacity: usize, scope: Scope) -> Self {
        Self {
            name: name.into(),
            capacity,
            scope,
        }
    }
}

/// A request or a table the model refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Two resources share a name.
    DuplicateResource(String),
    /// A resource has capacity 0.
    ZeroCapacity(String),
    /// A request names a resource the table does not have.
    UnknownResource(String),
    /// A request needs nothing.
    NoNeeds,
    /// The holder already holds a lease or has a request pending.
    AlreadyHolding,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateResource(name) => write!(f, "resource `{name}` is declared twice"),
            Self::ZeroCapacity(name) => write!(f, "resource `{name}` has capacity 0"),
            Self::UnknownResource(name) => write!(f, "unknown resource `{name}`"),
            Self::NoNeeds => f.write_str("a request must need at least one resource"),
            Self::AlreadyHolding => {
                f.write_str("the holder already holds a lease or has a request pending")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

/// What one [`Leases::step`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The holder has no pending request (none filed, or already granted).
    Idle,
    /// The holder took this resource; the request needs more.
    Took(String),
    /// The holder took its last resource: the request is granted.
    Granted,
    /// This resource has no free slot for the holder yet; the holder queues for it.
    Blocked(String),
}

/// A granted request's wait for one of the resources it needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait<H> {
    /// The holder.
    pub holder: H,
    /// One resource the request needed.
    pub resource: String,
    /// From the request's filing to its grant, in the caller's time units.
    pub waited: i64,
}

#[derive(Debug, Clone)]
struct Slot<H> {
    resource: Resource,
    holders: Vec<H>,
    /// Holders trying this resource and not yet given it, first come first served.
    queue: VecDeque<H>,
}

#[derive(Debug, Clone)]
struct Request<H> {
    holder: H,
    /// The resources in the order they are taken.
    order: Vec<String>,
    /// How many of `order` are taken.
    taken: usize,
    filed: i64,
}

/// The lease table: resources, their holders and the pending requests.
#[derive(Debug, Clone)]
pub struct Leases<H> {
    slots: BTreeMap<String, Slot<H>>,
    /// Pending requests, in arrival order.
    pending: Vec<Request<H>>,
    waits: Vec<Wait<H>>,
}

impl<H: Clone + Eq> Leases<H> {
    /// A table of `resources`, none held.
    ///
    /// # Errors
    /// A duplicate name or a capacity of 0.
    pub fn new(resources: impl IntoIterator<Item = Resource>) -> Result<Self, LeaseError> {
        let mut slots = BTreeMap::new();
        for resource in resources {
            if resource.capacity == 0 {
                return Err(LeaseError::ZeroCapacity(resource.name));
            }
            if slots.contains_key(&resource.name) {
                return Err(LeaseError::DuplicateResource(resource.name));
            }
            slots.insert(
                resource.name.clone(),
                Slot {
                    resource,
                    holders: Vec::new(),
                    queue: VecDeque::new(),
                },
            );
        }
        Ok(Self {
            slots,
            pending: Vec::new(),
            waits: Vec::new(),
        })
    }

    /// The resource named `name`.
    #[must_use]
    pub fn resource(&self, name: &str) -> Option<&Resource> {
        self.slots.get(name).map(|s| &s.resource)
    }

    /// Files `holder`'s request for every resource in `needs` (duplicates count once) at `now`.
    /// Nothing is taken yet: [`Leases::step`] or [`Leases::serve`] takes it.
    ///
    /// # Errors
    /// An unknown resource, an empty `needs`, or a holder that already holds a lease or has a
    /// request pending.
    pub fn acquire<S: AsRef<str>>(
        &mut self,
        holder: H,
        needs: &[S],
        now: i64,
    ) -> Result<(), LeaseError> {
        if needs.is_empty() {
            return Err(LeaseError::NoNeeds);
        }
        if let Some(unknown) = needs.iter().find(|n| !self.slots.contains_key(n.as_ref())) {
            return Err(LeaseError::UnknownResource(unknown.as_ref().to_owned()));
        }
        if self.is_pending(&holder) || !self.holds(&holder).is_empty() {
            return Err(LeaseError::AlreadyHolding);
        }
        let order: Vec<String> = if cfg!(mutant_lease_one_by_one) {
            // Mutant (`mutant_lease_one_by_one`): resources are taken one by one in the order
            // the request lists them, not in the global order.
            let mut seen = BTreeSet::new();
            needs
                .iter()
                .map(|n| n.as_ref().to_owned())
                .filter(|n| seen.insert(n.clone()))
                .collect()
        } else {
            needs
                .iter()
                .map(|n| n.as_ref().to_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        self.pending.push(Request {
            holder,
            order,
            taken: 0,
            filed: now,
        });
        Ok(())
    }

    /// Advances `holder`'s pending request by one resource at `now`.
    ///
    /// # Panics
    /// Never: [`Leases::acquire`] files only requests for resources the table has.
    pub fn step(&mut self, holder: &H, now: i64) -> Step {
        let Some(index) = self.pending.iter().position(|r| &r.holder == holder) else {
            return Step::Idle;
        };
        let request = &self.pending[index];
        let name = request.order[request.taken].clone();
        let slot = self
            .slots
            .get_mut(&name)
            .expect("acquire checked every resource exists");
        if !slot.queue.contains(holder) {
            slot.queue.push_back(holder.clone());
        }
        // The first `free` holders in the queue may take a slot; the rest wait their turn.
        let free = slot.resource.capacity.saturating_sub(slot.holders.len());
        let turn = slot.queue.iter().position(|h| h == holder);
        if turn.is_none_or(|t| t >= free) {
            return Step::Blocked(name);
        }
        slot.queue.retain(|h| h != holder);
        slot.holders.push(holder.clone());
        let request = &mut self.pending[index];
        request.taken += 1;
        if request.taken < request.order.len() {
            return Step::Took(name);
        }
        let request = self.pending.remove(index);
        let waited = now - request.filed;
        self.waits
            .extend(request.order.into_iter().map(|resource| Wait {
                holder: request.holder.clone(),
                resource,
                waited,
            }));
        Step::Granted
    }

    /// Steps every pending request in arrival order until none moves; returns the holders
    /// granted, in grant order.
    pub fn serve(&mut self, now: i64) -> Vec<H> {
        let mut granted = Vec::new();
        loop {
            let mut moved = false;
            let holders: Vec<H> = self.pending.iter().map(|r| r.holder.clone()).collect();
            for holder in holders {
                match self.step(&holder, now) {
                    Step::Took(_) => moved = true,
                    Step::Granted => {
                        moved = true;
                        granted.push(holder);
                    }
                    Step::Idle | Step::Blocked(_) => {}
                }
            }
            if !moved {
                return granted;
            }
        }
    }

    /// Releases everything `holder` holds and withdraws its pending request. Returns whether
    /// there was anything to release.
    pub fn release(&mut self, holder: &H) -> bool {
        let mut released = false;
        let before = self.pending.len();
        self.pending.retain(|r| &r.holder != holder);
        released |= self.pending.len() != before;
        for slot in self.slots.values_mut() {
            let (holders, queue) = (slot.holders.len(), slot.queue.len());
            slot.holders.retain(|h| h != holder);
            slot.queue.retain(|h| h != holder);
            released |= slot.holders.len() != holders || slot.queue.len() != queue;
        }
        released
    }

    /// Releases every lease and request whose holder `alive` says is gone; returns those
    /// holders. A live holder's leases are kept.
    pub fn release_stale(&mut self, alive: impl Fn(&H) -> bool) -> Vec<H> {
        if cfg!(mutant_lease_keep_dead_holder) {
            // Mutant (`mutant_lease_keep_dead_holder`): a dead holder's leases are kept.
            return Vec::new();
        }
        let mut dead: Vec<H> = Vec::new();
        let known = self
            .slots
            .values()
            .flat_map(|s| s.holders.iter().chain(s.queue.iter()))
            .chain(self.pending.iter().map(|r| &r.holder));
        for holder in known {
            if !dead.contains(holder) && !alive(holder) {
                dead.push(holder.clone());
            }
        }
        for holder in &dead {
            self.release(holder);
        }
        dead
    }

    /// The resources `holder` holds (granted or taken by its pending request), by name.
    #[must_use]
    pub fn holds(&self, holder: &H) -> Vec<&str> {
        self.slots
            .iter()
            .filter(|(_, s)| s.holders.contains(holder))
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// `holder` has a request pending (filed and not yet granted).
    #[must_use]
    pub fn is_pending(&self, holder: &H) -> bool {
        self.pending.iter().any(|r| &r.holder == holder)
    }

    /// `holder` holds a granted lease: it holds something and has no request pending.
    #[must_use]
    pub fn is_granted(&self, holder: &H) -> bool {
        !self.is_pending(holder) && !self.holds(holder).is_empty()
    }

    /// The holders of the resource named `name`.
    #[must_use]
    pub fn holders(&self, name: &str) -> &[H] {
        self.slots.get(name).map_or(&[], |s| s.holders.as_slice())
    }

    /// Every granted request's wait per resource, in grant order.
    #[must_use]
    pub fn waits(&self) -> &[Wait<H>] {
        &self.waits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [&str; 0] = [];

    fn table(resources: &[(&str, usize)]) -> Leases<&'static str> {
        Leases::new(
            resources
                .iter()
                .map(|&(name, capacity)| Resource::new(name, capacity, Scope::Machine)),
        )
        .unwrap()
    }

    #[test]
    fn two_items_wanting_the_same_pair_never_deadlock() {
        const BOUND: usize = 100;
        let mut leases = table(&[("a", 1), ("b", 1)]);
        leases.acquire("one", &["a", "b"], 0).unwrap();
        leases.acquire("two", &["b", "a"], 0).unwrap();
        let mut done: Vec<&str> = Vec::new();
        let mut steps = 0;
        // The first step that found each holder holding one of the pair.
        let mut split: Option<usize> = None;
        // The holders take turns; a granted holder does its work and releases on its next turn.
        while done.len() < 2 {
            assert!(
                steps < BOUND,
                "no grant for both within {BOUND} steps: deadlock"
            );
            let holder = ["one", "two"][steps % 2];
            let now = i64::try_from(steps).unwrap();
            steps += 1;
            if done.contains(&holder) {
                continue;
            }
            if leases.is_granted(&holder) {
                leases.release(&holder);
                done.push(holder);
                continue;
            }
            leases.step(&holder, now);
            assert!(
                !(leases.is_granted(&"one") && leases.is_granted(&"two")),
                "both granted at once"
            );
            let (one, two) = (leases.holds(&"one"), leases.holds(&"two"));
            if one.len() == 1 && two.len() == 1 {
                split.get_or_insert(steps);
            }
        }
        assert_eq!(split, None, "a step found each holding one of the pair");
        assert_eq!(done, ["one", "two"]);
    }

    #[test]
    fn a_lease_whose_holder_is_gone_is_released() {
        let mut leases = table(&[("a", 1), ("b", 1)]);
        leases.acquire("dead", &["a"], 0).unwrap();
        leases.acquire("live", &["b"], 0).unwrap();
        assert_eq!(leases.serve(0), ["dead", "live"]);
        leases.acquire("waiting", &["a"], 1).unwrap();
        assert_eq!(leases.serve(1), NONE);

        let released = leases.release_stale(|h| *h != "dead");
        assert_eq!(released, ["dead"]);
        assert_eq!(leases.holds(&"dead"), NONE);
        assert_eq!(leases.holds(&"live"), ["b"]);
        assert_eq!(leases.serve(5), ["waiting"]);
        assert_eq!(leases.holders("a"), ["waiting"]);
        assert!(leases.is_granted(&"live"));
    }

    #[test]
    fn a_resource_of_capacity_two_admits_two_holders_and_queues_the_third() {
        let mut leases = table(&[("cpu", 2)]);
        for holder in ["x", "y", "z"] {
            leases.acquire(holder, &["cpu"], 0).unwrap();
        }
        assert_eq!(leases.serve(0), ["x", "y"]);
        assert!(leases.is_pending(&"z"));
        leases.release(&"x");
        assert_eq!(leases.serve(1), ["z"]);
        assert_eq!(leases.holders("cpu"), ["y", "z"]);
    }

    #[test]
    fn each_grant_records_its_wait_per_resource() {
        let mut leases = table(&[("a", 1), ("b", 1)]);
        leases.acquire("first", &["a"], 10).unwrap();
        assert_eq!(leases.serve(10), ["first"]);
        leases.acquire("second", &["b", "a"], 12).unwrap();
        assert_eq!(leases.serve(12), NONE);
        leases.release(&"first");
        assert_eq!(leases.serve(19), ["second"]);
        let waits: Vec<(&str, &str, i64)> = leases
            .waits()
            .iter()
            .map(|w| (w.holder, w.resource.as_str(), w.waited))
            .collect();
        assert_eq!(
            waits,
            [("first", "a", 0), ("second", "a", 7), ("second", "b", 7)]
        );
    }

    #[test]
    fn a_request_for_two_is_not_starved_by_requests_for_one() {
        let mut leases = table(&[("a", 1), ("b", 1)]);
        leases.acquire("blocker", &["b"], 0).unwrap();
        assert_eq!(leases.serve(0), ["blocker"]);
        leases.acquire("pair", &["a", "b"], 1).unwrap();
        assert_eq!(leases.serve(1), NONE);
        // A stream of one-resource requests for `b` arrives while `pair` waits for it.
        leases.acquire("single", &["b"], 2).unwrap();
        assert_eq!(leases.serve(2), NONE);
        leases.release(&"blocker");
        assert_eq!(leases.serve(3), ["pair"]);
        assert!(leases.is_pending(&"single"));
    }

    #[test]
    fn a_holder_never_acquires_more_while_it_holds_something() {
        let mut leases = table(&[("a", 1), ("b", 1)]);
        leases.acquire("h", &["a"], 0).unwrap();
        assert_eq!(
            leases.acquire("h", &["b"], 0),
            Err(LeaseError::AlreadyHolding)
        );
        leases.serve(0);
        assert_eq!(
            leases.acquire("h", &["b"], 0),
            Err(LeaseError::AlreadyHolding)
        );
        assert_eq!(
            leases.acquire("g", &["c"], 0),
            Err(LeaseError::UnknownResource("c".to_owned()))
        );
        assert_eq!(leases.acquire("g", &NONE, 0), Err(LeaseError::NoNeeds));
    }

    #[test]
    fn a_table_refuses_duplicate_names_and_zero_capacity() {
        let dup = Leases::<u32>::new([
            Resource::new("a", 1, Scope::Identity),
            Resource::new("a", 1, Scope::Repository),
        ]);
        assert_eq!(
            dup.unwrap_err(),
            LeaseError::DuplicateResource("a".to_owned())
        );
        let zero = Leases::<u32>::new([Resource::new("a", 0, Scope::Machine)]);
        assert_eq!(zero.unwrap_err(), LeaseError::ZeroCapacity("a".to_owned()));
    }
}
