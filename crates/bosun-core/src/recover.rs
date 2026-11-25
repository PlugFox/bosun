//! The crash recovery plan of #7: what to do at start for an item in a daemon-owned state.
//!
//! In order: apply the user's command found on the board first (a cancel made while the
//! daemon was down wins over the resume); reap the recorded children that survived; then hand
//! the state's lost work back to the machine as [`Event::Restarted`]: `running` with a dead
//! child is classified from the transcript tail and resumed (or verified, parked, sent to Needs
//! you), uncounted but capped; `verifying` re-runs the verdict; `landing` redoes its keyed
//! step; `claimed` redoes the claim procedure. The caller drains the outbox after every item is
//! recovered.
//!
//! Stage 1 adopts no child (D1). In worktree mode (#8, E3) a `running` item whose session went
//! on under its shim, or ended under it while no daemon ran, is adopted instead
//! ([`Facts::attachable`]): unless a user command replaced the work, the plan is
//! [`Action::Attach`] and then [`Event::Reattached`]; the session's child is not reaped, and
//! nothing is resumed or counted.

use crate::item::{Ctx, Event, Exit, Item, State, UserCommand, step};

/// A child process recorded for the item (#50: pid and start time identify it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Child {
    /// Its process id.
    pub pid: u32,
    /// Its start time, ms since the Unix epoch, which makes a reused pid harmless.
    pub started_ms: i64,
    /// It is still alive (same pid and start time).
    pub alive: bool,
    /// It is the agent of the item's current session, the one [`Facts::attachable`] is about.
    pub session: bool,
}

/// What the daemon found at start for one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// This start's boot number, greater than any earlier one.
    pub boot: u32,
    /// A user command read from the board, if any.
    pub command: Option<UserCommand>,
    /// The item's recorded children.
    pub children: Vec<Child>,
    /// The exit classified from the session's transcript tail, if one could be.
    pub tail: Option<Exit>,
    /// The item's current session can be attached (#8, worktree mode): its shim's record is
    /// readable, and the shim still runs or recorded the session's exit.
    pub attachable: bool,
}

/// One recovery step, performed in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Apply this event to the item ([`step`]) and perform its effects.
    Apply(Event),
    /// Follow the item's current session, running or ended under its shim, to its end, as if
    /// this daemon had spawned it (#8, E3).
    Attach,
    /// Kill a surviving child and its tree; it cannot be adopted.
    Reap {
        /// Its process id.
        pid: u32,
        /// Its start time.
        started_ms: i64,
    },
}

/// The recovery plan for `item`. Empty when there is nothing to do (an item that is not in a
/// daemon-owned state, or one already recovered in this boot).
#[must_use]
pub fn recover(item: &Item, facts: &Facts, ctx: &Ctx) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut current = item.clone();
    let mut commanded = false;
    if let Some(command) = facts.command {
        let event = Event::User(command);
        if let Some(stepped) = step(&current, &event, ctx) {
            current = stepped.item;
            commanded = true;
            actions.push(Action::Apply(event));
        }
    }
    // A command replaces the work in flight, an attached session included.
    let attach = !commanded && facts.attachable && current.state == State::Running;
    actions.extend(
        facts
            .children
            .iter()
            .filter(|c| c.alive && !(attach && c.session))
            .map(|c| Action::Reap {
                pid: c.pid,
                started_ms: c.started_ms,
            }),
    );
    if attach {
        let event = Event::Reattached { boot: facts.boot };
        if step(&current, &event, ctx).is_some() {
            actions.push(Action::Attach);
            actions.push(Action::Apply(event));
        }
        return actions;
    }
    // A command replaces the work in flight: cancel, requeue and manual done end it, a
    // restart starts a new claim.
    if !commanded {
        let event = Event::Restarted {
            boot: facts.boot,
            tail: facts.tail,
        };
        if step(&current, &event, ctx).is_some() {
            actions.push(Action::Apply(event));
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::RunResult;
    use crate::item::{EffectKind, ItemId, SpawnMode, State, Stepped};

    fn ctx() -> Ctx {
        Ctx::at(1_800_000_000_000)
    }

    fn go(item: &Item, event: &Event) -> Item {
        step(item, event, &ctx()).expect("takes an arrow").item
    }

    fn running() -> Item {
        let item = Item::new(ItemId::new("local:demo#3"));
        go(&go(&item, &Event::Claim), &Event::ClaimConfirmed)
    }

    fn verifying() -> Item {
        let item = running();
        let item = go(
            &item,
            &Event::Exited {
                attempt: 1,
                spawn: 1,
                exit: Exit::Ok,
                text: String::new(),
            },
        );
        go(
            &item,
            &Event::Preconditions {
                attempt: 1,
                passed: true,
            },
        )
    }

    fn facts(boot: u32) -> Facts {
        Facts {
            boot,
            command: None,
            children: vec![
                Child {
                    pid: 10,
                    started_ms: 5,
                    alive: true,
                    session: true,
                },
                Child {
                    pid: 11,
                    started_ms: 6,
                    alive: false,
                    session: false,
                },
            ],
            tail: None,
            attachable: false,
        }
    }

    /// Performs the plan's `Apply` actions.
    fn perform(item: &Item, actions: &[Action]) -> Vec<Stepped> {
        let mut item = item.clone();
        let mut out = Vec::new();
        for action in actions {
            if let Action::Apply(event) = action {
                let stepped = step(&item, event, &ctx()).expect("a planned event applies");
                item = stepped.item.clone();
                out.push(stepped);
            }
        }
        out
    }

    #[test]
    fn a_running_item_with_a_dead_child_resumes_uncounted() {
        let item = running();
        let actions = recover(&item, &facts(1), &ctx());
        assert_eq!(
            actions[0],
            Action::Reap {
                pid: 10,
                started_ms: 5
            },
            "only the survivor is reaped"
        );
        let steps = perform(&item, &actions);
        let after = &steps.last().unwrap().item;
        assert_eq!((after.state, after.failed), (State::Running, 0));
        assert_eq!(after.attempt.as_ref().unwrap().number, 1);
        assert!(steps[0].transition.effects.iter().any(|e| matches!(
            e.kind,
            EffectKind::Spawn {
                mode: SpawnMode::Resume,
                ..
            }
        )));
        assert_eq!(
            recover(after, &facts(1), &ctx()).len(),
            1,
            "recovered once per boot: only the reap is left"
        );
    }

    #[test]
    fn the_transcript_tail_classifies_a_lost_session() {
        let mut f = facts(1);
        f.tail = Some(Exit::Ok);
        let item = running();
        let after = perform(&item, &recover(&item, &f, &ctx()));
        assert_eq!(after[0].item.state, State::Verifying);

        f.tail = Some(Exit::Limit { reset_ms: None });
        let after = perform(&item, &recover(&item, &f, &ctx()));
        assert!(matches!(after[0].item.state, State::Parked(_)));
    }

    #[test]
    fn a_verifying_item_reruns_its_verdict() {
        let item = verifying();
        let run = item.attempt.as_ref().unwrap().run;
        let after = perform(&item, &recover(&item, &facts(1), &ctx()));
        let next = &after[0];
        assert_eq!(next.item.state, State::Verifying);
        assert_eq!(next.item.failed, 0);
        assert!(next.transition.effects.iter().any(|e| e.kind
            == EffectKind::RunVerdict {
                attempt: 1,
                run: run + 1
            }));
        // The lost run's result can no longer count.
        let stale = Event::Verdict {
            attempt: 1,
            run,
            result: RunResult::Fail,
        };
        assert_eq!(step(&next.item, &stale, &ctx()), None);
    }

    #[test]
    fn a_landing_item_redoes_its_step() {
        let item = go(
            &verifying(),
            &Event::Verdict {
                attempt: 1,
                run: 1,
                result: RunResult::Pass,
            },
        );
        assert_eq!(item.state, State::Landing);
        let after = perform(&item, &recover(&item, &facts(1), &ctx()));
        assert!(
            after[0]
                .transition
                .effects
                .iter()
                .any(|e| e.kind == EffectKind::Land { attempt: 1 })
        );
    }

    #[test]
    fn a_user_cancel_made_while_down_wins_over_the_resume() {
        let item = running();
        let mut f = facts(1);
        f.command = Some(UserCommand::Cancel);
        let actions = recover(&item, &f, &ctx());
        assert_eq!(actions[0], Action::Apply(Event::User(UserCommand::Cancel)));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Apply(Event::Restarted { .. })))
        );
        assert_eq!(perform(&item, &actions)[0].item.state, State::Cancelled);
    }

    #[test]
    fn an_attachable_session_is_attached_not_reaped() {
        let item = running();
        let mut f = facts(1);
        f.children.push(Child {
            pid: 12,
            started_ms: 7,
            alive: true,
            session: false,
        });
        f.attachable = true;
        let actions = recover(&item, &f, &ctx());
        assert_eq!(
            actions,
            vec![
                Action::Reap {
                    pid: 12,
                    started_ms: 7
                },
                Action::Attach,
                Action::Apply(Event::Reattached { boot: 1 }),
            ],
            "the session's child is followed, not reaped"
        );
        let steps = perform(&item, &actions);
        let after = &steps[0].item;
        assert_eq!(
            (after.state, after.uncounted, after.boot),
            (State::Running, 0, 1)
        );
        assert!(
            steps[0]
                .transition
                .effects
                .iter()
                .all(|e| e.kind == EffectKind::Report),
            "nothing is spawned"
        );
        assert!(
            !recover(after, &f, &ctx()).contains(&Action::Attach),
            "attached once per boot"
        );

        // A cancel on the board still wins: the session is reaped.
        let mut cancelled = f.clone();
        cancelled.command = Some(UserCommand::Cancel);
        let actions = recover(&item, &cancelled, &ctx());
        assert_eq!(actions[0], Action::Apply(Event::User(UserCommand::Cancel)));
        assert!(actions.contains(&Action::Reap {
            pid: 10,
            started_ms: 5
        }));
        assert!(!actions.contains(&Action::Attach));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Apply(Event::Reattached { .. })))
        );

        // Without `attachable`, today's plan.
        let mut lost = f.clone();
        lost.attachable = false;
        let today = recover(&item, &lost, &ctx());
        assert_eq!(
            today[..2],
            [
                Action::Reap {
                    pid: 10,
                    started_ms: 5
                },
                Action::Reap {
                    pid: 12,
                    started_ms: 7
                },
            ]
        );
        assert!(matches!(today[2], Action::Apply(Event::Restarted { .. })));
        assert_eq!(today.len(), 3);

        // Only a running item is attached.
        let verifying = verifying();
        let plan = recover(&verifying, &f, &ctx());
        assert!(!plan.contains(&Action::Attach));
    }

    #[test]
    fn items_not_in_flight_need_nothing() {
        let item = Item::new(ItemId::new("local:demo#4"));
        let mut f = facts(1);
        f.children.clear();
        assert_eq!(recover(&item, &f, &ctx()), Vec::new());
    }
}
