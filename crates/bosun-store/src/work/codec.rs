//! The stable names of the loop's enums in the database, and their way back.
//!
//! The state and attempt-state names come from `bosun_core` (`as_str`); the detail of a state
//! (a park, an outcome, a reason) and the smaller enums are named here. Every name is in a
//! `CHECK` of migration 0005 (stage 2's of migration 0013), so a value that does not read back
//! is corruption.

use bosun_core::{
    AttemptState, Column, EffectKind, Exit, Guard, Next, Outcome, Park, ParkReason, Reason,
    RunResult, SpawnMode, State,
};

use super::{PrState, WorktreeKind, WorktreeState};
use crate::error::Error;

fn corrupt(what: &str, value: &str) -> Error {
    Error::Corrupt(format!("unknown {what} {value:?}"))
}

/// The columns of `item` that hold a [`State`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StateColumns {
    pub(crate) state: &'static str,
    pub(crate) park_reason: Option<&'static str>,
    pub(crate) park_until_ms: Option<i64>,
    pub(crate) park_next: Option<&'static str>,
    pub(crate) outcome: Option<&'static str>,
    pub(crate) reason: Option<&'static str>,
}

pub(crate) fn state_columns(state: State) -> StateColumns {
    let mut columns = StateColumns {
        state: state.as_str(),
        ..StateColumns::default()
    };
    match state {
        State::Parked(park) => {
            columns.park_reason = Some(park_reason(park.reason));
            columns.park_until_ms = park.until_ms;
            columns.park_next = Some(next(park.next));
        }
        State::Done(outcome) => columns.outcome = Some(self::outcome(outcome)),
        State::NeedsYou(why) => columns.reason = Some(reason(why)),
        _ => {}
    }
    columns
}

/// The detail of a state as one name: the park reason, the outcome or the reason.
pub(crate) fn state_detail(state: State) -> Option<&'static str> {
    let columns = state_columns(state);
    columns.park_reason.or(columns.outcome).or(columns.reason)
}

/// The stored columns of `item` back to a [`State`].
pub(crate) fn parse_state(
    state: &str,
    park_reason: Option<&str>,
    park_until_ms: Option<i64>,
    park_next: Option<&str>,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> Result<State, Error> {
    let missing = |what: &str| Error::Corrupt(format!("a {state} item has no {what}"));
    Ok(match state {
        "ready" => State::Ready,
        "claimed" => State::Claimed,
        "running" => State::Running,
        "parked" => State::Parked(Park {
            reason: parse_park_reason(park_reason.ok_or_else(|| missing("park reason"))?)?,
            until_ms: park_until_ms,
            next: parse_next(park_next.ok_or_else(|| missing("wake step"))?)?,
        }),
        "verifying" => State::Verifying,
        "landing" => State::Landing,
        "done" => State::Done(parse_outcome(outcome.ok_or_else(|| missing("outcome"))?)?),
        "needs_you" => State::NeedsYou(parse_reason(reason.ok_or_else(|| missing("reason"))?)?),
        "cancelled" => State::Cancelled,
        other => return Err(corrupt("item state", other)),
    })
}

fn park_reason(reason: ParkReason) -> &'static str {
    match reason {
        ParkReason::Limit => "limit",
        ParkReason::Paused => "paused",
        ParkReason::Killed => "killed",
        ParkReason::DailyCap => "daily_cap",
    }
}

fn parse_park_reason(name: &str) -> Result<ParkReason, Error> {
    Ok(match name {
        "limit" => ParkReason::Limit,
        "paused" => ParkReason::Paused,
        "killed" => ParkReason::Killed,
        "daily_cap" => ParkReason::DailyCap,
        other => return Err(corrupt("park reason", other)),
    })
}

fn next(next: Next) -> &'static str {
    match next {
        Next::NewAttempt => "new_attempt",
        Next::Resume => "resume",
        Next::Reset => "reset",
        Next::Verify => "verify",
    }
}

fn parse_next(name: &str) -> Result<Next, Error> {
    Ok(match name {
        "new_attempt" => Next::NewAttempt,
        "resume" => Next::Resume,
        "reset" => Next::Reset,
        "verify" => Next::Verify,
        other => return Err(corrupt("wake step", other)),
    })
}

fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pass => "pass",
        Outcome::Flaky => "flaky",
        Outcome::UserClosed => "user_closed",
    }
}

fn parse_outcome(name: &str) -> Result<Outcome, Error> {
    Ok(match name {
        "pass" => Outcome::Pass,
        "flaky" => Outcome::Flaky,
        "user_closed" => Outcome::UserClosed,
        other => return Err(corrupt("outcome", other)),
    })
}

pub(crate) fn reason(reason: Reason) -> &'static str {
    match reason {
        Reason::Lint => "lint",
        Reason::Cycle => "cycle",
        Reason::Question => "question",
        Reason::Auth => "auth",
        Reason::Budget => "budget",
        Reason::ParkCap => "park_cap",
        Reason::ConfigChanged => "config_changed",
        Reason::AttemptsExhausted => "attempts_exhausted",
        Reason::Guard(Guard::ProtectedPath) => "guard_protected_path",
        Reason::Guard(Guard::IgnoredFile) => "guard_ignored_file",
        Reason::Guard(Guard::GitConfig) => "guard_git_config",
        Reason::Guard(Guard::Checkout) => "guard_checkout",
        Reason::LandingRefused => "landing_refused",
        Reason::Setup => "setup",
        Reason::Handshake => "handshake",
        Reason::Identity => "identity",
        Reason::Stale => "stale",
    }
}

pub(crate) fn parse_reason(name: &str) -> Result<Reason, Error> {
    Ok(match name {
        "lint" => Reason::Lint,
        "cycle" => Reason::Cycle,
        "question" => Reason::Question,
        "auth" => Reason::Auth,
        "budget" => Reason::Budget,
        "park_cap" => Reason::ParkCap,
        "config_changed" => Reason::ConfigChanged,
        "attempts_exhausted" => Reason::AttemptsExhausted,
        "guard_protected_path" => Reason::Guard(Guard::ProtectedPath),
        "guard_ignored_file" => Reason::Guard(Guard::IgnoredFile),
        "guard_git_config" => Reason::Guard(Guard::GitConfig),
        "guard_checkout" => Reason::Guard(Guard::Checkout),
        "landing_refused" => Reason::LandingRefused,
        "setup" => Reason::Setup,
        "handshake" => Reason::Handshake,
        "identity" => Reason::Identity,
        "stale" => Reason::Stale,
        other => return Err(corrupt("reason", other)),
    })
}

pub(crate) fn parse_attempt_state(name: &str) -> Result<AttemptState, Error> {
    use AttemptState as S;
    [
        S::Spawned,
        S::Exited,
        S::Killed,
        S::Limited,
        S::VerdictPending,
        S::VerdictRunning,
        S::Pass,
        S::Fail,
        S::Flaky,
        S::Timeout,
    ]
    .into_iter()
    .find(|s| s.as_str() == name)
    .ok_or_else(|| corrupt("attempt state", name))
}

pub(crate) fn run_result(result: RunResult) -> &'static str {
    match result {
        RunResult::Pass => "pass",
        RunResult::Fail => "fail",
        RunResult::Timeout => "timeout",
    }
}

pub(crate) fn parse_run_result(name: &str) -> Result<RunResult, Error> {
    Ok(match name {
        "pass" => RunResult::Pass,
        "fail" => RunResult::Fail,
        "timeout" => RunResult::Timeout,
        other => return Err(corrupt("verdict result", other)),
    })
}

pub(crate) fn column(column: Column) -> &'static str {
    match column {
        Column::InProgress => "in_progress",
        Column::Verify => "verify",
        Column::Done => "done",
        Column::NeedsYou => "needs_you",
    }
}

pub(crate) fn parse_column(name: &str) -> Result<Column, Error> {
    Ok(match name {
        "in_progress" => Column::InProgress,
        "verify" => Column::Verify,
        "done" => Column::Done,
        "needs_you" => Column::NeedsYou,
        other => return Err(corrupt("column", other)),
    })
}

pub(crate) fn spawn_mode(mode: SpawnMode) -> &'static str {
    match mode {
        SpawnMode::Fresh => "fresh",
        SpawnMode::Resume => "resume",
        SpawnMode::Reset => "reset",
    }
}

pub(crate) fn parse_spawn_mode(name: &str) -> Result<SpawnMode, Error> {
    Ok(match name {
        "fresh" => SpawnMode::Fresh,
        "resume" => SpawnMode::Resume,
        "reset" => SpawnMode::Reset,
        other => return Err(corrupt("spawn mode", other)),
    })
}

/// An exit as its name and a limit's reset time.
pub(crate) fn exit(exit: Exit) -> (&'static str, Option<i64>) {
    match exit {
        Exit::Ok => ("ok", None),
        Exit::Limit { reset_ms } => ("limit", reset_ms),
        Exit::Auth => ("auth", None),
        Exit::Crash => ("crash", None),
    }
}

pub(crate) fn parse_exit(name: &str, reset_ms: Option<i64>) -> Result<Exit, Error> {
    Ok(match name {
        "ok" => Exit::Ok,
        "limit" => Exit::Limit { reset_ms },
        "auth" => Exit::Auth,
        "crash" => Exit::Crash,
        other => return Err(corrupt("exit", other)),
    })
}

/// The name of an effect's kind, as the transition log records it.
pub(crate) fn effect_kind(kind: EffectKind) -> &'static str {
    match kind {
        EffectKind::Claim => "claim",
        EffectKind::Spawn { .. } => "spawn",
        EffectKind::Kill => "kill",
        EffectKind::CheckPreconditions { .. } => "check_preconditions",
        EffectKind::RunVerdict { .. } => "run_verdict",
        EffectKind::Land { .. } => "land",
        EffectKind::Project(_) => "project",
        EffectKind::Comment(_) => "comment",
        EffectKind::Report => "report",
    }
}

pub(crate) fn worktree_kind(kind: WorktreeKind) -> &'static str {
    match kind {
        WorktreeKind::Item => "item",
        WorktreeKind::Verdict => "verdict",
    }
}

pub(crate) fn parse_worktree_kind(name: &str) -> Result<WorktreeKind, Error> {
    Ok(match name {
        "item" => WorktreeKind::Item,
        "verdict" => WorktreeKind::Verdict,
        other => return Err(corrupt("worktree kind", other)),
    })
}

pub(crate) fn worktree_state(state: WorktreeState) -> &'static str {
    match state {
        WorktreeState::Created => "created",
        WorktreeState::Removed => "removed",
        WorktreeState::Quarantined => "quarantined",
    }
}

pub(crate) fn parse_worktree_state(name: &str) -> Result<WorktreeState, Error> {
    Ok(match name {
        "created" => WorktreeState::Created,
        "removed" => WorktreeState::Removed,
        "quarantined" => WorktreeState::Quarantined,
        other => return Err(corrupt("worktree state", other)),
    })
}

pub(crate) fn pr_state(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Merged => "merged",
        PrState::Closed => "closed",
    }
}

pub(crate) fn parse_pr_state(name: &str) -> Result<PrState, Error> {
    Ok(match name {
        "open" => PrState::Open,
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        other => return Err(corrupt("pull request state", other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_reads_back() {
        let parks = [
            ParkReason::Limit,
            ParkReason::Paused,
            ParkReason::Killed,
            ParkReason::DailyCap,
        ]
        .into_iter()
        .flat_map(|reason| {
            [Next::NewAttempt, Next::Resume, Next::Reset, Next::Verify]
                .into_iter()
                .flat_map(move |next| {
                    [None, Some(7)].into_iter().map(move |until_ms| {
                        State::Parked(Park {
                            reason,
                            until_ms,
                            next,
                        })
                    })
                })
        });
        let reasons = [
            Reason::Lint,
            Reason::Cycle,
            Reason::Question,
            Reason::Auth,
            Reason::Budget,
            Reason::ParkCap,
            Reason::ConfigChanged,
            Reason::AttemptsExhausted,
            Reason::Guard(Guard::ProtectedPath),
            Reason::Guard(Guard::IgnoredFile),
            Reason::Guard(Guard::GitConfig),
            Reason::Guard(Guard::Checkout),
            Reason::LandingRefused,
            Reason::Setup,
            Reason::Handshake,
            Reason::Identity,
            Reason::Stale,
        ]
        .map(State::NeedsYou);
        let states = [
            State::Ready,
            State::Claimed,
            State::Running,
            State::Verifying,
            State::Landing,
            State::Cancelled,
            State::Done(Outcome::Pass),
            State::Done(Outcome::Flaky),
            State::Done(Outcome::UserClosed),
        ]
        .into_iter()
        .chain(parks)
        .chain(reasons);
        for state in states {
            let c = state_columns(state);
            let back = parse_state(
                c.state,
                c.park_reason,
                c.park_until_ms,
                c.park_next,
                c.outcome,
                c.reason,
            )
            .unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn the_small_enums_read_back() {
        for column in [
            Column::InProgress,
            Column::Verify,
            Column::Done,
            Column::NeedsYou,
        ] {
            assert_eq!(parse_column(self::column(column)).unwrap(), column);
        }
        for mode in [SpawnMode::Fresh, SpawnMode::Resume, SpawnMode::Reset] {
            assert_eq!(parse_spawn_mode(spawn_mode(mode)).unwrap(), mode);
        }
        for result in [RunResult::Pass, RunResult::Fail, RunResult::Timeout] {
            assert_eq!(parse_run_result(run_result(result)).unwrap(), result);
        }
        for e in [
            Exit::Ok,
            Exit::Auth,
            Exit::Crash,
            Exit::Limit { reset_ms: None },
            Exit::Limit { reset_ms: Some(5) },
        ] {
            let (name, reset) = exit(e);
            assert_eq!(parse_exit(name, reset).unwrap(), e);
        }
        for kind in [WorktreeKind::Item, WorktreeKind::Verdict] {
            assert_eq!(parse_worktree_kind(worktree_kind(kind)).unwrap(), kind);
        }
        for state in [
            WorktreeState::Created,
            WorktreeState::Removed,
            WorktreeState::Quarantined,
        ] {
            assert_eq!(parse_worktree_state(worktree_state(state)).unwrap(), state);
        }
        for state in [PrState::Open, PrState::Merged, PrState::Closed] {
            assert_eq!(parse_pr_state(pr_state(state)).unwrap(), state);
        }
        assert!(parse_attempt_state("nope").is_err());
        assert!(parse_state("parked", None, None, Some("resume"), None, None).is_err());
    }
}
