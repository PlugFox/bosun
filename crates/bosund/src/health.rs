//! The health of probed resources (#8, "Leases": a lease says who holds a resource, not whether
//! it answers), the pure part: no I/O, every time passed in.
//!
//! - The state is keyed by resource instance and identity ([`Key`]): each identity runs a
//!   resource's probe in its own agent environment and sees only its own result, even of a
//!   machine-scope instance; another identity's failing streak changes nothing for it (#2).
//! - A key is healthy (or not probed yet) or in a failing [`Streak`]. A failing result starts or
//!   extends the streak; the next probe is due [`PROBE_BACKOFF`] `* 2^(failures-1)` after it,
//!   at most [`PROBE_BACKOFF_MAX`]. A passing result ends the streak.
//! - [`Health::begin`] says whether a probe is to run: while a streak's next probe is not due,
//!   the key counts as failing without running it. When it is due, its next time is moved to
//!   the next backoff first, so that another loop of the same identity counts the key failing
//!   meanwhile instead of probing it at once. (Two loops may still probe a healthy key at the
//!   same moment.)
//! - [`Health::tell`] is true once per streak: the failure is told once.
//! - The state lives in the daemon's memory, in the lease manager, and is not journalled: a
//!   restarted daemon probes again before its first claim, so an outage across a restart starts
//!   a new streak and is told once more.

use std::collections::BTreeMap;
use std::time::Duration;

/// The wait after a streak's first failure before the probe runs again.
pub(crate) const PROBE_BACKOFF: Duration = Duration::from_secs(30);

/// The longest wait between two probes of a failing key.
pub(crate) const PROBE_BACKOFF_MAX: Duration = Duration::from_mins(10);

/// What the health state is kept by: a resource instance, as probed by one identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    /// `<name>`, `<name>/<identity>` or `<name>/<repo>`.
    pub(crate) instance: String,
    /// The identity that probes it.
    pub(crate) identity: String,
}

impl Key {
    pub(crate) fn new(instance: &str, identity: &str) -> Self {
        Self {
            instance: instance.to_owned(),
            identity: identity.to_owned(),
        }
    }
}

/// A failing streak of one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Streak {
    /// When its first failing probe ended.
    pub(crate) since_ms: i64,
    /// How many probes in a row failed.
    pub(crate) failures: u32,
    /// When the probe is due again.
    pub(crate) next_ms: i64,
    /// What the last failing probe said.
    pub(crate) detail: String,
    /// Whether the streak was told ([`Health::tell`]).
    pub(crate) told: bool,
}

/// What [`Health::begin`] says of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Check {
    /// Run the probe, then [`Health::record`] its result.
    Run,
    /// The key is failing and its probe is not due: it counts as failing.
    Failing(Streak),
}

/// What [`Health::record`] made of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// The probe passed; `ended` when it ended a streak.
    Healthy { ended: bool },
    /// The probe failed: the streak as it is now.
    Failing(Streak),
}

/// The health of every probed key of one daemon.
#[derive(Debug, Default)]
pub(crate) struct Health {
    streaks: BTreeMap<Key, Streak>,
}

/// The wait after the `failures`-th failure in a row: [`PROBE_BACKOFF`] doubled for each
/// failure after the first, at most [`PROBE_BACKOFF_MAX`].
pub(crate) fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    PROBE_BACKOFF
        .saturating_mul(1 << doublings)
        .min(PROBE_BACKOFF_MAX)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Health {
    /// Whether the probe of `key` is to run at `now_ms`. A due probe of a failing streak has its
    /// next time moved to the next backoff before it runs.
    pub(crate) fn begin(&mut self, key: &Key, now_ms: i64) -> Check {
        let Some(streak) = self.streaks.get_mut(key) else {
            return Check::Run;
        };
        // Mutant (`mutant_leases_probe_no_backoff`): a failing key is always due.
        if streak.next_ms > now_ms && !cfg!(mutant_leases_probe_no_backoff) {
            return Check::Failing(streak.clone());
        }
        streak.next_ms = now_ms.saturating_add(ms(backoff(streak.failures.saturating_add(1))));
        Check::Run
    }

    /// Records the result of a probe of `key` that ended at `now_ms`: `Err` with what it said
    /// when it failed.
    pub(crate) fn record(
        &mut self,
        key: &Key,
        now_ms: i64,
        result: Result<(), String>,
    ) -> Recorded {
        match result {
            Ok(()) => Recorded::Healthy {
                ended: self.streaks.remove(key).is_some(),
            },
            Err(detail) => {
                let streak = self.streaks.entry(key.clone()).or_insert(Streak {
                    since_ms: now_ms,
                    failures: 0,
                    next_ms: now_ms,
                    detail: String::new(),
                    told: false,
                });
                streak.failures = streak.failures.saturating_add(1);
                streak.next_ms = now_ms.saturating_add(ms(backoff(streak.failures)));
                streak.detail = detail;
                Recorded::Failing(streak.clone())
            }
        }
    }

    /// Whether the failing streak of `key` is to be told now: true once per streak.
    pub(crate) fn tell(&mut self, key: &Key) -> bool {
        let Some(streak) = self.streaks.get_mut(key) else {
            return false;
        };
        !std::mem::replace(&mut streak.told, true)
    }

    /// The failing streaks of `identity`, by instance.
    pub(crate) fn failing_of(&self, identity: &str) -> Vec<(String, Streak)> {
        self.streaks
            .iter()
            .filter(|(key, _)| key.identity == identity)
            .map(|(key, streak)| (key.instance.clone(), streak.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1000;

    /// A failing key is not probed until its backoff ran out; the backoff doubles from 30 s to
    /// at most 10 min; a pass ends the streak; the failure is told once per streak; another
    /// identity's key is its own.
    #[test]
    fn a_failing_key_waits_a_doubling_backoff_and_is_told_once_per_streak() {
        let mut health = Health::default();
        let rig = Key::new("rig", "work");
        let theirs = Key::new("rig", "home");
        assert_eq!(health.begin(&rig, 0), Check::Run);
        let Recorded::Failing(streak) = health.record(&rig, 0, Err("exit 1".to_owned())) else {
            panic!("a failure is a streak");
        };
        assert_eq!(
            (streak.since_ms, streak.failures, streak.next_ms),
            (0, 1, 30 * S)
        );
        assert!(health.tell(&rig));
        assert!(!health.tell(&rig));
        assert_eq!(health.begin(&theirs, 1), Check::Run);
        assert!(!health.tell(&theirs));
        assert_eq!(health.failing_of("home").len(), 0);
        assert!(matches!(health.begin(&rig, 29 * S), Check::Failing(_)));

        // Due: it runs, and meanwhile the key counts as failing for everyone else.
        assert_eq!(health.begin(&rig, 30 * S), Check::Run);
        assert!(matches!(health.begin(&rig, 30 * S), Check::Failing(_)));
        let Recorded::Failing(streak) = health.record(&rig, 31 * S, Err("exit 2".to_owned()))
        else {
            panic!("a failure is a streak");
        };
        assert_eq!((streak.since_ms, streak.failures), (0, 2));
        assert_eq!(streak.next_ms, 91 * S);
        assert_eq!(streak.detail, "exit 2");
        assert!(!health.tell(&rig));

        assert_eq!(backoff(1), PROBE_BACKOFF);
        assert_eq!(backoff(3), Duration::from_mins(2));
        assert_eq!(backoff(6), PROBE_BACKOFF_MAX);
        assert_eq!(backoff(60), PROBE_BACKOFF_MAX);

        assert_eq!(health.begin(&rig, 91 * S), Check::Run);
        assert_eq!(
            health.record(&rig, 92 * S, Ok(())),
            Recorded::Healthy { ended: true }
        );
        assert_eq!(health.begin(&rig, 92 * S), Check::Run);
        assert_eq!(
            health.record(&rig, 92 * S, Ok(())),
            Recorded::Healthy { ended: false }
        );
        // A new streak is told again.
        health.record(&rig, 100 * S, Err("timeout after 10s".to_owned()));
        assert!(health.tell(&rig));
        assert_eq!(health.failing_of("work").len(), 1);
    }
}
