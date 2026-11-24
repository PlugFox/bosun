//! Parking (#7, #66): the clock park timers and the wake check read, and the sleep detector.
//!
//! A limit parks the item until its reset time (or the default backoff); the clock's wall time
//! is the machine's `now`, so a test moves time instead of sleeping. A sleep shows as a gap
//! between two ticks of the loop: the wall clock (or, where the monotonic clock counts a
//! suspended machine, both clocks) moved far more than the tick's period. On such a gap the
//! session's child is revalidated by its recorded pid and start time; a dead one is a crash of
//! the attempt, retried once uncounted by the machine.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bosun_proc::ProcessKey;

/// A gap between ticks longer than this, beyond the tick's own period, is taken for a sleep.
/// A false alarm only costs a liveness check of the child.
pub(crate) const SLEEP_GAP: Duration = Duration::from_mins(1);

/// The clocks of the loop.
pub(crate) trait Clock: Send + Sync + fmt::Debug {
    /// The wall clock, ms since the Unix epoch: park times and the machine's `now`.
    fn wall_ms(&self) -> i64;
    /// A monotonic clock, from an arbitrary origin.
    fn mono(&self) -> Duration;
}

/// The system's clocks.
#[derive(Debug)]
pub(crate) struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn wall_ms(&self) -> i64 {
        super::now_ms()
    }

    fn mono(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Whether a recorded child still runs (`bosun_proc::is_running` outside tests).
pub(crate) type Alive = Arc<dyn Fn(ProcessKey) -> bool + Send + Sync>;

/// The running session's child as its task recorded it: set once spawned, cleared once it
/// exited (so a child that ended on its own is never taken for one that died in a sleep).
pub(crate) type ChildSlot = Arc<Mutex<Option<ProcessKey>>>;

/// The sleep detector: compares each tick's clocks with the previous tick's.
#[derive(Debug)]
pub(crate) struct Gaps {
    period: Duration,
    last: Option<(i64, Duration)>,
}

impl Gaps {
    /// A detector for ticks every `period`.
    pub(crate) fn new(period: Duration) -> Self {
        Self { period, last: None }
    }

    /// Records a tick; returns the gap when the time since the last tick, on either clock,
    /// exceeds the period by more than [`SLEEP_GAP`].
    pub(crate) fn observe(&mut self, wall_ms: i64, mono: Duration) -> Option<Duration> {
        let last = self.last.replace((wall_ms, mono));
        let (last_wall, last_mono) = last?;
        let wall = Duration::from_millis(u64::try_from(wall_ms - last_wall).unwrap_or(0));
        let mono = mono.saturating_sub(last_mono);
        let gap = wall.max(mono).saturating_sub(self.period);
        (gap > SLEEP_GAP).then_some(gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_secs(1);

    #[test]
    fn the_first_tick_and_regular_ticks_are_no_gap() {
        let mut gaps = Gaps::new(PERIOD);
        assert_eq!(gaps.observe(1_000, Duration::ZERO), None);
        assert_eq!(gaps.observe(2_000, Duration::from_secs(1)), None);
        // A late tick (a slow poll) below the threshold.
        assert_eq!(gaps.observe(32_000, Duration::from_secs(31)), None);
    }

    #[test]
    fn a_wall_clock_gap_is_a_sleep_where_the_monotonic_clock_stood_still() {
        let mut gaps = Gaps::new(PERIOD);
        gaps.observe(0, Duration::ZERO);
        let gap = gaps.observe(20 * 60_000, Duration::from_secs(1));
        assert_eq!(gap, Some(Duration::from_secs(20 * 60 - 1)));
    }

    #[test]
    fn a_monotonic_gap_is_a_sleep_where_it_counts_the_suspension() {
        let mut gaps = Gaps::new(PERIOD);
        gaps.observe(0, Duration::ZERO);
        assert!(gaps.observe(1_000, Duration::from_mins(20)).is_some());
        // The next regular tick is no gap again.
        assert_eq!(gaps.observe(2_000, Duration::from_mins(20) + PERIOD), None);
    }

    #[test]
    fn a_wall_clock_set_back_is_no_gap() {
        let mut gaps = Gaps::new(PERIOD);
        gaps.observe(10 * 60_000, Duration::ZERO);
        assert_eq!(gaps.observe(0, Duration::from_secs(1)), None);
    }
}
