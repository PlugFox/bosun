//! How often to poll (#5): every 30 s while anything is active, backing off to several minutes
//! when idle, and slower still when the rate limit runs low.

use std::time::Duration;

use super::transport::RateLimit;

/// The interval while an item is claimed or running, and the first idle interval.
pub const ACTIVE: Duration = Duration::from_secs(30);

/// The longest idle interval.
pub const IDLE_MAX: Duration = Duration::from_mins(5);

/// The share of the hourly quota kept in reserve for writes: below it, polling waits for the
/// window to reset (capped at [`IDLE_MAX`] beyond the normal interval, so a wrong clock cannot
/// stop polling).
pub const RESERVE_PERCENT: u64 = 10;

/// The poll interval: [`Pacer::next`] after every poll.
#[derive(Debug, Clone, Default)]
pub struct Pacer {
    /// Polls in a row that found nothing new while nothing was active.
    quiet: u32,
}

impl Pacer {
    /// A pacer that starts at [`ACTIVE`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long to wait before the next poll. `active`: an item is claimed or running.
    /// `changed`: the poll found changes. `rate`: the last rate limit seen; `now`: Unix seconds.
    pub fn next(
        &mut self,
        active: bool,
        changed: bool,
        rate: Option<&RateLimit>,
        now: u64,
    ) -> Duration {
        let interval = if active || changed {
            self.quiet = 0;
            ACTIVE
        } else {
            self.quiet = self.quiet.saturating_add(1);
            // 30 s, 60 s, 120 s, 240 s, then the cap.
            ACTIVE.saturating_mul(1 << self.quiet.min(8)).min(IDLE_MAX)
        };
        match rate {
            Some(rate) if rate.remaining.saturating_mul(100) < rate.limit * RESERVE_PERCENT => {
                let until_reset = Duration::from_secs(rate.reset.saturating_sub(now));
                interval.max(until_reset.min(interval + IDLE_MAX))
            }
            _ => interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(remaining: u64, reset: u64) -> RateLimit {
        RateLimit {
            limit: 5000,
            remaining,
            used: 5000 - remaining,
            reset,
        }
    }

    #[test]
    fn active_polls_every_thirty_seconds_and_idle_backs_off() {
        let mut pacer = Pacer::new();
        assert_eq!(pacer.next(true, false, None, 0), ACTIVE);
        assert_eq!(pacer.next(true, false, None, 0), ACTIVE);
        let idle: Vec<u64> = (0..6)
            .map(|_| pacer.next(false, false, None, 0).as_secs())
            .collect();
        assert_eq!(idle, [60, 120, 240, 300, 300, 300]);
        assert_eq!(pacer.next(false, true, None, 0), ACTIVE, "a change resets");
        assert_eq!(pacer.next(false, false, None, 0).as_secs(), 60);
    }

    #[test]
    fn a_low_quota_waits_for_the_reset() {
        let mut pacer = Pacer::new();
        let now = 1_000;
        assert_eq!(
            pacer.next(true, false, Some(&rate(4000, now + 900)), now),
            ACTIVE
        );
        assert_eq!(
            pacer.next(true, false, Some(&rate(100, now + 200)), now),
            Duration::from_secs(200)
        );
        assert_eq!(
            pacer.next(true, false, Some(&rate(100, now + 3600)), now),
            ACTIVE + IDLE_MAX,
            "capped"
        );
        assert_eq!(
            pacer.next(true, false, Some(&rate(100, now - 5)), now),
            ACTIVE,
            "a past reset"
        );
    }
}
