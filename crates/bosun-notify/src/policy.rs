//! When a channel's pending notices go out (#9, "Delivery through the outbox"; #208, "a
//! minimum gap between messages").
//!
//! [`decide`] is pure: given a channel's pending rows, when it last sent, its minimum gap and
//! the time, it says which rows to drop, which to send now and when to look again.
//!
//! - **Maximum age.** A row older than [`MAX_AGE`] is dropped, never sent: a day-old "needs
//!   you" is noise, and the board says it anyway.
//! - **Minimum gap.** Nothing is sent before `last_sent_ms + min_gap_ms`.
//! - **Coalescing.** When a send is allowed, every due row (its retry time passed, or none)
//!   goes in one message, so a burst inside the gap is one message after it.
//! - **Backoff.** A failed send is retried after [`backoff`]: the service's own wait when it
//!   gives one, else `2^tries` seconds, at most [`MAX_BACKOFF`].

use std::time::Duration;

/// The oldest a notice may be and still be sent.
pub const MAX_AGE: Duration = Duration::from_hours(24);

/// The longest wait before a failed notice is tried again.
pub const MAX_BACKOFF: Duration = Duration::from_mins(10);

/// One pending row, as the policy needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub id: i64,
    pub created_ms: i64,
    /// When a failed row may be tried again; `None` for a row never tried.
    pub retry_at_ms: Option<i64>,
}

/// What to do with a channel's pending rows now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// Too old: dropped, never sent.
    pub drop: Vec<i64>,
    /// Sent now, in one message, in the order given.
    pub send: Vec<i64>,
    /// When the rows left could next be sent; `None` when none is left.
    pub next_at_ms: Option<i64>,
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Decides what to do with `pending` (oldest first) of a channel that last sent at
/// `last_sent_ms` and keeps `min_gap_ms` between two messages, at `now_ms`.
#[must_use]
pub fn decide(
    pending: &[Pending],
    last_sent_ms: Option<i64>,
    min_gap_ms: i64,
    now_ms: i64,
) -> Decision {
    let mut decision = Decision::default();
    let mut kept = Vec::new();
    for p in pending {
        // Mutant (`mutant_notify_keep_stale`): no row is too old.
        if !cfg!(mutant_notify_keep_stale) && now_ms.saturating_sub(p.created_ms) > ms(MAX_AGE) {
            decision.drop.push(p.id);
        } else {
            kept.push(*p);
        }
    }
    // Mutant (`mutant_notify_no_gap`): the minimum gap is ignored.
    let gap = if cfg!(mutant_notify_no_gap) {
        0
    } else {
        min_gap_ms
    };
    let mut open_at = last_sent_ms.map_or(i64::MIN, |t| t.saturating_add(gap));
    let due = |p: &Pending| p.retry_at_ms.is_none_or(|r| r <= now_ms);
    if open_at <= now_ms {
        decision.send = kept.iter().filter(|p| due(p)).map(|p| p.id).collect();
        if !decision.send.is_empty() {
            open_at = now_ms.saturating_add(gap);
        }
    }
    decision.next_at_ms = kept
        .iter()
        .filter(|p| !decision.send.contains(&p.id))
        .map(|p| open_at.max(p.retry_at_ms.unwrap_or(i64::MIN)).max(now_ms))
        .min();
    decision
}

/// How long to wait after a failed send that was the `tries`-th: the service's `after` when it
/// gave one, else `2^tries` seconds, at most [`MAX_BACKOFF`].
#[must_use]
pub fn backoff(tries: u32, after: Option<Duration>) -> Duration {
    after.unwrap_or_else(|| Duration::from_secs(1 << tries.min(10)).min(MAX_BACKOFF))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_800_000_000_000;
    const S: i64 = 1_000;

    fn row(id: i64, created_ms: i64) -> Pending {
        Pending {
            id,
            created_ms,
            retry_at_ms: None,
        }
    }

    #[test]
    fn a_burst_within_the_gap_is_one_message() {
        let burst = [row(1, T + S), row(2, T + 2 * S), row(3, T + 3 * S)];
        let early = decide(&burst, Some(T), 60 * S, T + 30 * S);
        assert_eq!(early.send, Vec::<i64>::new());
        assert_eq!(early.drop, Vec::<i64>::new());
        assert_eq!(early.next_at_ms, Some(T + 60 * S));

        let open = decide(&burst, Some(T), 60 * S, T + 60 * S);
        assert_eq!(open.send, [1, 2, 3]);
        assert_eq!(open.next_at_ms, None);

        // A row waiting for its retry stays behind, and the gap starts again after the send.
        let mut retried = burst;
        retried[2].retry_at_ms = Some(T + 90 * S);
        let open = decide(&retried, Some(T), 60 * S, T + 60 * S);
        assert_eq!(open.send, [1, 2]);
        assert_eq!(open.next_at_ms, Some(T + 120 * S));
    }

    #[test]
    fn a_notification_older_than_the_maximum_age_is_dropped_not_sent() {
        let now = T + ms(Duration::from_hours(30));
        let old = row(1, now - ms(Duration::from_hours(25)));
        let new = row(2, now - 60 * S);
        let decision = decide(&[old, new], None, 60 * S, now);
        assert_eq!(decision.drop, [1]);
        assert_eq!(decision.send, [2]);
    }

    #[test]
    fn a_failed_send_backs_off_by_the_service_or_doubling() {
        assert_eq!(backoff(1, None), Duration::from_secs(2));
        assert_eq!(backoff(3, None), Duration::from_secs(8));
        assert_eq!(backoff(30, None), MAX_BACKOFF);
        assert_eq!(
            backoff(1, Some(Duration::from_secs(42))),
            Duration::from_secs(42)
        );
    }
}
