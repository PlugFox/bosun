//! The attempt machine of #7: `spawned -> exited | killed | limited`, then
//! `verdict_pending -> verdict_running -> pass | fail | flaky | timeout`.
//!
//! An attempt is one try at an item, counted toward the item's attempt limit. It may span
//! several sessions: a resume after a limit, a kill switch or a lost daemon, and one fresh
//! session after a crash, all go back to `spawned` without starting a new attempt. The
//! item machine ([`crate::step`]) drives the attempt and owns the counting; this module owns
//! only the arrows and the verdict rule: a failing run is re-run once with no session in
//! between, fail then pass is `flaky` (treated as pass), and two failing runs are one failed
//! attempt.

/// The state of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttemptState {
    /// A session of the attempt is running.
    Spawned,
    /// The session exited by itself (ok, auth or crash).
    Exited,
    /// The daemon killed the session (the kill switch).
    Killed,
    /// The session exited on a provider limit.
    Limited,
    /// The verdict's preconditions are being checked.
    VerdictPending,
    /// A verdict run is in progress (the first run, or the re-run after a failing one).
    VerdictRunning,
    /// The verdict passed on its first run.
    Pass,
    /// The attempt failed: two failing runs, a failed precondition, or a crash beyond its retry.
    Fail,
    /// The verdict failed once and then passed: recorded, counted, treated as pass, reported.
    Flaky,
    /// The last of two non-passing runs timed out.
    Timeout,
}

impl AttemptState {
    /// The stable name stored in the `attempt` table (#3).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Limited => "limited",
            Self::VerdictPending => "verdict_pending",
            Self::VerdictRunning => "verdict_running",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Flaky => "flaky",
            Self::Timeout => "timeout",
        }
    }

    /// The verdict accepted the attempt: `pass` or `flaky`.
    #[must_use]
    pub fn passed(self) -> bool {
        matches!(self, Self::Pass | Self::Flaky)
    }

    /// The attempt counts as one failed attempt: `fail` or `timeout`.
    #[must_use]
    pub fn failed(self) -> bool {
        matches!(self, Self::Fail | Self::Timeout)
    }
}

/// The result of one verdict run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunResult {
    /// Exit 0.
    Pass,
    /// A non-zero exit.
    Fail,
    /// The run exceeded the verdict timeout and was killed.
    Timeout,
}

/// What happened to an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptEvent {
    /// The session exited by itself; `limited` when its exit was classified as a provider limit.
    Exited {
        /// The exit was a provider limit.
        limited: bool,
    },
    /// The daemon killed the session.
    Killed,
    /// A new session of this attempt started: a resume by id, or a fresh session.
    Respawned,
    /// The daemon started checking the verdict's preconditions.
    Verify,
    /// A verdict run started (the first run, or the re-run).
    VerdictStarted,
    /// A verdict run finished.
    VerdictFinished(RunResult),
    /// The attempt failed without a verdict: a precondition failed, a crash exceeded its retry.
    Failed,
    /// The landing step rebased the attempt's commits: its verdict runs again on the rebased
    /// commit (#8). The runs before the rebase are forgotten: the rebased commit gets the whole
    /// re-run rule (a first pass is `pass`, fail then pass is `flaky`, two non-passing runs fail).
    Reverify,
}

/// One attempt of an item.
///
/// Numbers are item-wide and never reused, even when the user resets the attempt counter, so
/// an event addressed to an attempt, a session or a verdict run that is no longer current is
/// recognised as stale and ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The attempt's number, unique within the item.
    pub number: u32,
    /// The current session's number, unique within the item.
    pub spawn: u32,
    /// The current verdict run's number, unique within the item; 0 before the first run.
    pub run: u32,
    /// Where the attempt is.
    pub state: AttemptState,
    /// The result of the first, non-passing, verdict run, while the re-run is pending.
    pub first_run: Option<RunResult>,
    /// Crash retries used by this attempt.
    pub crash_retries: u32,
}

impl Attempt {
    /// A new attempt whose first session is `spawn`.
    #[must_use]
    pub fn new(number: u32, spawn: u32) -> Self {
        Self {
            number,
            spawn,
            run: 0,
            state: AttemptState::Spawned,
            first_run: None,
            crash_retries: 0,
        }
    }

    /// The attempt after `event`, or `None` when the event has no arrow from the current state.
    #[must_use]
    pub fn step(&self, event: AttemptEvent) -> Option<Self> {
        use AttemptState as S;
        let mut next = self.clone();
        next.state = match (self.state, event) {
            (S::Spawned, AttemptEvent::Exited { limited: false }) => S::Exited,
            (S::Spawned, AttemptEvent::Exited { limited: true }) => S::Limited,
            (S::Spawned, AttemptEvent::Killed) => S::Killed,
            (S::Spawned | S::Exited | S::Limited | S::Killed, AttemptEvent::Respawned) => {
                S::Spawned
            }
            (S::Exited, AttemptEvent::Verify) => S::VerdictPending,
            (S::VerdictPending | S::VerdictRunning, AttemptEvent::VerdictStarted) => {
                S::VerdictRunning
            }
            (S::VerdictRunning, AttemptEvent::VerdictFinished(result)) => {
                return Some(self.finish_run(result));
            }
            (S::Exited | S::Limited | S::Killed | S::VerdictPending, AttemptEvent::Failed) => {
                S::Fail
            }
            (S::Pass | S::Flaky, AttemptEvent::Reverify) => {
                // Mutant (`mutant_core_reverify_keeps_first_run`): a flaky attempt's failed first
                // run counts as the first run on the rebased commit.
                if !cfg!(mutant_core_reverify_keeps_first_run) {
                    next.first_run = None;
                }
                S::VerdictRunning
            }
            _ => return None,
        };
        Some(next)
    }

    /// The verdict rule: re-run once after a non-passing run; fail then pass is flaky.
    fn finish_run(&self, result: RunResult) -> Self {
        let mut next = self.clone();
        match (self.first_run, result) {
            (None, RunResult::Pass) => next.state = AttemptState::Pass,
            (None, first) => next.first_run = Some(first),
            (Some(_), RunResult::Pass) => {
                next.state = if cfg!(mutant_core_flaky_is_fail) {
                    AttemptState::Fail
                } else {
                    AttemptState::Flaky
                };
            }
            (Some(_), RunResult::Fail) => next.state = AttemptState::Fail,
            (Some(_), RunResult::Timeout) => next.state = AttemptState::Timeout,
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict_running() -> Attempt {
        let a = Attempt::new(1, 1);
        let a = a.step(AttemptEvent::Exited { limited: false }).unwrap();
        let a = a.step(AttemptEvent::Verify).unwrap();
        a.step(AttemptEvent::VerdictStarted).unwrap()
    }

    fn runs(results: &[RunResult]) -> AttemptState {
        let mut a = verdict_running();
        for &r in results {
            a = a.step(AttemptEvent::VerdictFinished(r)).unwrap();
        }
        a.state
    }

    #[test]
    fn the_verdict_rule() {
        use RunResult::{Fail, Pass, Timeout};
        assert_eq!(runs(&[Pass]), AttemptState::Pass);
        assert_eq!(runs(&[Fail]), AttemptState::VerdictRunning);
        assert_eq!(runs(&[Fail, Fail]), AttemptState::Fail);
        assert_eq!(runs(&[Timeout, Fail]), AttemptState::Fail);
        assert_eq!(runs(&[Fail, Timeout]), AttemptState::Timeout);
        assert!(AttemptState::Timeout.failed());
        assert!(AttemptState::Flaky.passed());
    }

    #[test]
    fn a_final_attempt_takes_no_more_events() {
        let a = verdict_running()
            .step(AttemptEvent::VerdictFinished(RunResult::Pass))
            .unwrap();
        for event in [
            AttemptEvent::VerdictFinished(RunResult::Fail),
            AttemptEvent::VerdictStarted,
            AttemptEvent::Failed,
            AttemptEvent::Respawned,
        ] {
            assert_eq!(a.step(event), None, "{event:?}");
        }
    }

    #[test]
    fn sessions_of_one_attempt_go_back_to_spawned() {
        let a = Attempt::new(1, 1);
        let limited = a.step(AttemptEvent::Exited { limited: true }).unwrap();
        assert_eq!(limited.state, AttemptState::Limited);
        let resumed = limited.step(AttemptEvent::Respawned).unwrap();
        assert_eq!(resumed.state, AttemptState::Spawned);
        assert_eq!(resumed.number, 1);
        assert_eq!(a.step(AttemptEvent::Verify), None);
    }
}
