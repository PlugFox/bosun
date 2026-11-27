//! The fast-death breaker (#7): a repository whose sessions keep dying at once is paused.
//!
//! A broken environment (an agent CLI that cannot get going, a repository hook that kills every
//! session) would otherwise send every Ready card to Needs you, one after the other: a crash is
//! retried once per attempt, and nothing looks across items. The loop of each repository keeps
//! a [`Streak`] across its items and observes how every session it spawned or re-attached
//! ended ([`SessionEnd`], reported by the session task with its event):
//!
//! - A session counts when it ended by itself (not killed by the daemon, not for a missed MCP
//!   handshake, not capped by its shim), its exit was classified a crash (a failed resume is
//!   one), and it lived less than `daemon.fast_death_within` (default 60 s) from its spawn.
//! - Any other end resets the streak: a normal exit, a limit, an expired authentication, a crash
//!   after the window, a session the daemon killed (a blocking question's included). A session
//!   re-attached after a daemon restart has outlived a daemon and never counts.
//! - At `daemon.fast_death_sessions` (default 3) in a row the breaker trips: the loop replaces
//!   the session's event with [`bosun_core::Event::FastDeath`], so the tripping session charges
//!   its item nothing and the item waits parked (`paused`); it pauses the repository in
//!   [`crate::control::Control`] ([`crate::control::Control::trip`]) until `bosun resume`, and
//!   says why in `status`, the `fast_deaths` event, the daemon's log and a `repo_paused`
//!   notice in the identity's notification outbox (#9). The crashes before the
//!   tripping one were charged as any crash.
//!
//! The streak and the trip are in memory only: a daemon restart starts the streak at zero and
//! lifts the trip, as it lifts a pause by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use bosun_api::EventBody;
use bosun_core::{Event, ItemId};

use super::{Done, Loop};

/// How a session the loop spawned or re-attached ended, for the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionEnd {
    /// Its exit was classified a crash (`Exit::Crash`), a failed resume included.
    pub(crate) crashed: bool,
    /// It ended by itself: the daemon did not kill it, its handshake was not missed and its
    /// shim did not cap it.
    pub(crate) by_itself: bool,
    /// From right after its spawn returned to its end.
    pub(crate) lived: Duration,
}

/// The sessions in a row of one repository that died by themselves soon after their spawn.
#[derive(Debug, Default)]
pub(super) struct Streak {
    count: u32,
}

impl Streak {
    /// Observes `end` under the rule `(sessions, within)`; `true` when it trips the breaker:
    /// the streak then starts again from zero.
    pub(super) fn observe(&mut self, end: SessionEnd, (sessions, within): (u32, Duration)) -> bool {
        // Mutant (`mutant_loop_fast_death_counts_kills`): a session the daemon killed counts.
        let by_itself = end.by_itself || cfg!(mutant_loop_fast_death_counts_kills);
        if end.crashed && by_itself && end.lived < within {
            self.count += 1;
        } else if !cfg!(mutant_loop_fast_death_streak_never_resets) {
            // Mutant (`mutant_loop_fast_death_streak_never_resets`): an end that should reset
            // the streak leaves it.
            self.count = 0;
        }
        if self.count >= sessions.max(1) {
            self.count = 0;
            return true;
        }
        false
    }

    /// The sessions counted so far.
    #[cfg(test)]
    fn count(&self) -> u32 {
        self.count
    }
}

impl Loop {
    /// Observes the session a [`Done::Event`] reports the end of, if any, and returns what the
    /// loop applies. Every session is observed, its item current or not: a blocking question's
    /// kill ends a session after its item has left the loop, and resets the streak.
    pub(super) async fn observe_session(&mut self, done: Done) -> Done {
        let Done::Event {
            item,
            event,
            failing,
            session: Some(end),
        } = done
        else {
            return done;
        };
        let event = self.fast_death(&item, event, end).await;
        Done::Event {
            item,
            event,
            failing,
            session: None,
        }
    }

    /// Observes how a session of the repository ended, before its `event` is applied; returns
    /// the event to apply: `event` itself, or [`Event::FastDeath`] when this session tripped the
    /// breaker, which then pauses the repository and says so.
    async fn fast_death(&mut self, item: &ItemId, event: Event, end: SessionEnd) -> Event {
        // Mutant (`mutant_loop_no_fast_death_breaker`): nothing is observed.
        if cfg!(mutant_loop_no_fast_death_breaker) {
            return event;
        }
        let daemon = &self.config().daemon;
        let (sessions, within) = (daemon.fast_death_sessions, daemon.fast_death_within);
        if !self.streak.observe(end, (sessions, within)) {
            return event;
        }
        let (attempt, spawn) = match event {
            Event::Exited { attempt, spawn, .. } | Event::ResumeFailed { attempt, spawn } => {
                (attempt, spawn)
            }
            // Only an exit or a failed resume can be a crash by itself.
            _ => (0, 0),
        };
        let repo = self.shared.options.repo.clone();
        let now = self.shared.deps.clock.wall_ms();
        let identity = self.identity_name().unwrap_or_default();
        let why = reason(sessions, within, &item.to_string());
        self.shared.deps.control.trip(&identity, &repo, why, now);
        crate::log::log!(
            "bosund: repository {repo}: {sessions} sessions in a row ended within {} s of their \
             spawn (the last: {item}, attempt {attempt}, session {spawn}); paused until bosun \
             resume",
            within.as_secs()
        );
        self.queue_paused(&repo, item, sessions, within, now).await;
        self.shared.publish(EventBody::FastDeaths {
            repo: repo.clone(),
            item: item.to_string(),
            sessions,
            within_ms: u64::try_from(within.as_millis()).unwrap_or(u64::MAX),
        });
        self.shared.publish(EventBody::ControlChanged {
            state: "paused".to_owned(),
            repo: Some(repo),
        });
        if spawn == 0 {
            return event;
        }
        Event::FastDeath { attempt, spawn }
    }
}

impl Loop {
    /// Queues the `repo_paused` notice of a trip at `at_ms` (#9): keyed by the repository and
    /// the pause's time, with the breaker's figures.
    async fn queue_paused(
        &self,
        repo: &str,
        item: &ItemId,
        sessions: u32,
        within: Duration,
        at_ms: i64,
    ) {
        // Mutant (`mutant_loop_no_breaker_notice`): the breaker's pause queues nothing.
        if cfg!(mutant_loop_no_breaker_notice) {
            return;
        }
        let notice = bosun_store::NewNotice {
            key: format!("repo_paused:{repo}/{at_ms}"),
            event: "repo_paused".to_owned(),
            repo: repo.to_owned(),
            item: Some(item.to_string()),
            facts: BTreeMap::from([
                ("sessions".to_owned(), sessions.to_string()),
                ("within_s".to_owned(), within.as_secs().to_string()),
                ("item".to_owned(), item.to_string()),
            ]),
            at_ms,
        };
        if let Err(e) = self
            .shared
            .deps
            .writer
            .write(vec![bosun_store::Write::NoticeQueued(notice)])
            .await
        {
            self.shared.trouble(format!(
                "the notice of the breaker's pause cannot be queued: {e}"
            ));
        }
    }
}

/// The reason a trip records, shown by `status`.
pub(super) fn reason(sessions: u32, within: Duration, item: &str) -> String {
    format!(
        "{sessions} sessions in a row ended within {} s of their spawn (the last: {item}); bosun \
         resume lifts it",
        within.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: (u32, Duration) = (3, Duration::from_secs(60));

    fn crash(secs: u64) -> SessionEnd {
        SessionEnd {
            crashed: true,
            by_itself: true,
            lived: Duration::from_secs(secs),
        }
    }

    #[test]
    fn only_a_crash_that_ended_by_itself_within_the_window_counts() {
        let mut streak = Streak::default();
        assert!(!streak.observe(crash(1), RULE));
        assert!(!streak.observe(crash(59), RULE));
        assert_eq!(streak.count(), 2);
        assert!(!streak.observe(crash(60), RULE), "at the window: no count");
        assert_eq!(streak.count(), 0, "and the streak is reset");

        assert!(!streak.observe(crash(1), RULE));
        assert!(!streak.observe(crash(1), RULE));
        assert!(streak.observe(crash(1), RULE), "the third trips");
        assert_eq!(streak.count(), 0, "zero after the trip");

        assert!(!streak.observe(crash(1), RULE));
        assert!(!streak.observe(crash(1), RULE));
        let killed = SessionEnd {
            by_itself: false,
            ..crash(1)
        };
        assert!(!streak.observe(killed, RULE), "a kill by the daemon");
        assert_eq!(streak.count(), 0, "resets");

        assert!(!streak.observe(crash(1), RULE));
        assert!(!streak.observe(crash(1), RULE));
        let normal = SessionEnd {
            crashed: false,
            ..crash(1)
        };
        assert!(!streak.observe(normal, RULE), "a normal end");
        assert_eq!(streak.count(), 0, "resets");
        assert!(!streak.observe(crash(1), RULE));
        assert_eq!(streak.count(), 1);
    }

    #[test]
    fn the_reason_names_the_rule_and_the_item() {
        assert_eq!(
            reason(3, Duration::from_secs(60), "demo#1"),
            "3 sessions in a row ended within 60 s of their spawn (the last: demo#1); bosun \
             resume lifts it"
        );
    }
}
