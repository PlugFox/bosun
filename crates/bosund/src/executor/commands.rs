//! User actions on daemon-owned cards (#5, #65): commands, not conflicts.
//!
//! Every poll compares each recorded item of the repository with its card ([`read`]):
//!
//! | The user | On an item that is | Means |
//! | --- | --- | --- |
//! | removes the card, or moves it to Backlog | in flight or Needs you | cancel |
//! | closes the card's issue | in flight or Needs you | cancel (#111) |
//! | moves it to Ready | in flight | start over when its turn comes ([`UserCommand::Requeue`]) |
//! | moves it to Ready | Needs you or cancelled | retry with a fresh attempt counter |
//! | moves it to Done | in flight or Needs you | accepted, user-closed, no verdict |
//! | adds the `restart` label | in flight | kill, re-snapshot, start again; the label is removed |
//!
//! Anything else on a daemon-owned card (another daemon-owned column; a Done card back in
//! Ready, which is rework and belongs to the `pr` mode of #8) is reverted to the database's
//! state through the outbox, with one comment per item state ([`super::outbox`]), so a user
//! who keeps moving the card cannot make bosun loop or spam.
//!
//! A card missing from the board counts once a second poll confirms it, so a board read
//! mid-save is not a cancel. A card whose column write is still in the outbox is not compared:
//! the board has not caught up yet (an outage), and the difference is the daemon's own. An item
//! being claimed is left to the claim, which re-reads the card itself (#5), except in worktree
//! mode (#8), where a move to Backlog cancels it: its `setup` may run long. An item in flight
//! that is not the loop's (found at start) is crash recovery's (#68).

use std::collections::{BTreeMap, BTreeSet};

use bosun_api::EventBody;
use bosun_core::{Effect, EffectKey, EffectKind, Event, ItemId, State, UserCommand};
use bosun_queue::{Card, Changes, Column};
use bosun_store::{OutboxOp, StoredItem, Write};

use super::{Loop, Signal, blocking, claim, git, item_id, lookup, outbox};

/// The label that restarts an item's session (#5).
pub(crate) const RESTART_LABEL: &str = "restart";

/// What a card says about its item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    /// Nothing to do.
    Nothing,
    /// The user's command.
    Command(UserCommand),
    /// A foreign change: the card goes back to the database's state.
    Revert,
}

/// Whether a column write of an item is still in the outbox, as [`read`] takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pending {
    /// None is.
    No,
    /// One is.
    Write,
    /// One is, and it is the Needs-you write of a pull request closed unmerged (#8): the card
    /// is still in Done, where the item was until then.
    PrClosed,
}

impl From<bool> for Pending {
    fn from(pending: bool) -> Self {
        if pending { Self::Write } else { Self::No }
    }
}

/// What the card (`None`: gone from the board) says about an item in `state`. `pending`: a
/// column write of the item is still in the outbox ([`Pending`]); `restarted`: the `restart`
/// label on the card was already acted on.
pub(crate) fn read(
    state: State,
    card: Option<&Card>,
    pending: impl Into<Pending>,
    restarted: bool,
) -> Read {
    let pending = pending.into();
    // Mutant (`mutant_loop_done_ignored_while_pending`): any pending write hides a Done the
    // user moved the card to.
    let pr_closing = pending == Pending::PrClosed
        || (pending == Pending::Write && cfg!(mutant_loop_done_ignored_while_pending));
    let pending = pending != Pending::No;
    let in_flight = state.is_active();
    let owned = in_flight || matches!(state, State::NeedsYou(_));
    if state == State::Claimed {
        return Read::Nothing;
    }
    let Some(card) = card else {
        return if owned {
            Read::Command(UserCommand::Cancel)
        } else {
            Read::Nothing
        };
    };
    // Mutant (`mutant_queue_closed_not_cancel`): a closed issue is read by its column alone.
    if card.closed && !cfg!(mutant_queue_closed_not_cancel) {
        return if owned {
            Read::Command(UserCommand::Cancel)
        } else {
            Read::Nothing
        };
    }
    match card.column {
        Column::Backlog if owned => Read::Command(UserCommand::Cancel),
        Column::Ready if in_flight => Read::Command(UserCommand::Requeue),
        Column::Ready if matches!(state, State::NeedsYou(_) | State::Cancelled) && !pending => {
            Read::Command(UserCommand::Requeue)
        }
        Column::Ready if matches!(state, State::Done(_)) && !pending => Read::Revert,
        // A Done item whose pull request was closed unmerged (#8) is Needs you while its card
        // is still in Done: the column write is the daemon's own, still in the outbox. Any
        // other Done is the user's, whatever else is pending.
        Column::Done if in_flight || (owned && !pr_closing) => {
            Read::Command(UserCommand::ManualDone)
        }
        Column::InProgress | Column::Verify | Column::NeedsYou => {
            if in_flight && !restarted && card.labels.iter().any(|l| l == RESTART_LABEL) {
                return Read::Command(UserCommand::Restart);
            }
            match state.column().map(outbox::owned) {
                Some(expected) if expected.column() != card.column && !pending => Read::Revert,
                _ => Read::Nothing,
            }
        }
        _ => Read::Nothing,
    }
}

/// The board as one poll saw it, and the repository's recorded items.
#[derive(Debug)]
pub(crate) struct Synced {
    pub(crate) changes: Changes,
    pub(crate) stored: BTreeMap<ItemId, StoredItem>,
}

impl Loop {
    /// Whether a column write pending for `item`, of the transitions `seqs`, is the Needs-you
    /// write of its pull request closed unmerged: the item's [`NOTE_PR_CLOSED`] note names one
    /// of them.
    ///
    /// [`NOTE_PR_CLOSED`]: super::pulls::NOTE_PR_CLOSED
    async fn pr_closing(&self, item: &bosun_core::Item, seqs: &BTreeSet<u32>) -> bool {
        #[derive(serde::Deserialize)]
        struct Closed {
            seq: u32,
        }
        if item.state != State::NeedsYou(bosun_core::Reason::LandingRefused) {
            return false;
        }
        self.shared
            .deps
            .reader
            .item_note(&item.id, super::pulls::NOTE_PR_CLOSED)
            .await
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Closed>(&text).ok())
            .is_some_and(|c| seqs.contains(&c.seq))
    }

    /// Polls the board and applies the user's commands. `None` when the board or the store
    /// cannot be read, or when a command changed an item (the next tick schedules).
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn sync_board(&mut self) -> Option<Synced> {
        let repo_name = self.shared.options.repo.clone();
        let config = self.config();
        let (repo, _) = lookup(&config, &repo_name)?;
        // Read before the poll: a write acknowledged after it is on the board the poll sees.
        // Each item with a column write pending, and the transitions those writes belong to.
        let pending: BTreeMap<ItemId, BTreeSet<u32>> =
            match outbox::pending(&self.shared.deps.reader, &repo_name).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter(|r| matches!(r.op, OutboxOp::Project(_)))
                    .fold(BTreeMap::new(), |mut map, r| {
                        let seqs: &mut BTreeSet<u32> = map.entry(r.item).or_default();
                        seqs.extend(outbox::key_parts(&r.effect_key).map(|(_, seq, _)| seq));
                        map
                    }),
                Err(e) => {
                    self.shared
                        .trouble(format!("the outbox cannot be read: {e}"));
                    return None;
                }
            };
        let queue = self.shared.deps.queue.clone();
        let cursor = self.cursor.clone();
        let changes = match blocking(move || queue.poll(cursor.as_ref())).await {
            Ok(changes) => changes,
            Err(e) => {
                self.board_error("the board cannot be read", &e);
                return None;
            }
        };
        if !changes.is_empty() {
            self.shared.publish(EventBody::QueueChanged {
                repo: repo_name.clone(),
                hash: claim::text_hash(changes.cursor.as_str()),
            });
        }
        self.cursor = Some(changes.cursor.clone());
        self.board.note_titles(
            changes
                .cards
                .iter()
                .map(|c| (item_id(&repo_name, c.id), c.title.clone())),
        );

        let stored = match self.shared.deps.reader.items(false).await {
            Ok(items) => items,
            Err(e) => {
                self.shared
                    .trouble(format!("the store cannot be read: {e}"));
                return None;
            }
        };
        let stored: BTreeMap<ItemId, StoredItem> = stored
            .into_iter()
            .filter(|s| s.repo == repo.key)
            .map(|s| (s.item.id.clone(), s))
            .collect();

        let mut acted = false;
        for (id, entry) in &stored {
            let Some(number) = outbox::card_of(id) else {
                continue;
            };
            let card = changes.card(number);
            if card.is_none_or(|c| !c.labels.iter().any(|l| l == RESTART_LABEL)) {
                self.restarts.remove(id);
            }
            let current = self.current.as_ref().filter(|c| c.item.id == *id);
            let item = match current {
                Some(cur) => cur.item.clone(),
                // In flight but not the loop's: crash recovery's (#68).
                None if entry.item.state.is_active() => continue,
                None => entry.item.clone(),
            };
            let restarted = self.restarts.contains(id);
            let pending = match pending.get(id) {
                None => Pending::No,
                Some(seqs) if self.pr_closing(&item, seqs).await => Pending::PrClosed,
                Some(_) => Pending::Write,
            };
            let mut read = read(item.state, card, pending, restarted);
            // Worktree mode (#8, E19): a claim may spend long in the worktree's `setup`; the
            // card moved to Backlog meanwhile (only the user moves it there: the claim found it
            // in Ready) cancels it, and the kill ends the setup at once. Without worktree mode
            // nothing is read during a claim.
            if item.state == State::Claimed
                && repo.worktree.is_some()
                && card.is_some_and(|c| c.column == Column::Backlog)
            {
                read = Read::Command(UserCommand::Cancel);
            }
            // A card missing from one poll may be a board caught mid-save (an editor that
            // writes in place): only a second poll without it is a cancel.
            if card.is_some() {
                self.absent.remove(id);
            } else if read != Read::Nothing && self.absent.insert(id.clone()) {
                read = Read::Nothing;
            }
            match read {
                Read::Nothing => {}
                Read::Command(command) => {
                    crate::log::log!(
                        "bosund: repository {repo_name}: {id}: the user's {command:?} in state {}",
                        item.state.as_str()
                    );
                    acted = true;
                    if command == UserCommand::Restart {
                        self.restarts.insert(id.clone());
                    }
                    let event = Event::User(command);
                    if current.is_some() {
                        if let (Some(cur), Some(card)) = (self.current.as_mut(), card) {
                            cur.card = card.clone();
                        }
                        self.apply(event).await;
                    } else {
                        self.bounce(&item, &event).await;
                    }
                }
                Read::Revert => {
                    acted = true;
                    crate::log::log!(
                        "bosund: repository {repo_name}: {id}: a change bosun did not make is                          reverted to {}",
                        item.state.as_str()
                    );
                    self.revert(&item).await;
                }
            }
            if self.halted {
                return None;
            }
        }
        (!acted).then_some(Synced { changes, stored })
    }

    /// Queues the column write that reverts a foreign change on the card of `item`; the
    /// drainer adds the one comment of the item's state.
    async fn revert(&mut self, item: &bosun_core::Item) {
        let Some(column) = item.state.column() else {
            return;
        };
        let now = self.shared.deps.clock.wall_ms();
        let effect = Effect {
            key: EffectKey {
                item: item.id.clone(),
                attempt: item.attempt.as_ref().map_or(0, |a| a.number),
                seq: item.seq,
                index: outbox::revert_index(now),
            },
            kind: EffectKind::Project(column),
        };
        match self
            .shared
            .deps
            .writer
            .write(vec![Write::Enqueue { effect, at_ms: now }])
            .await
        {
            Ok(()) => self.board.wake(),
            Err(e) => self.halt(&e.to_string()),
        }
    }

    /// Before a claim of the item in flight that ran already (a restart): its task ends, its
    /// work is set aside and the checkout goes back to its base, so it starts again from the
    /// base it was first claimed at.
    pub(crate) async fn before_claim(&mut self) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        let Some(claimed) = cur.claimed.take() else {
            return;
        };
        let item = cur.item.id.clone();
        if let Some(mut task) = cur.task.take() {
            let _ = task.kill.send(Signal::Kill);
            let _ = (&mut task.handle).await;
        }
        // Worktree mode: the old worktree is closed with its work kept; the claim opens a new one.
        if let Some(wt) = &claimed.worktree {
            self.close_worktree(wt, Some(&item)).await;
            return;
        }
        let config = self.config();
        let Some((repo, _)) = lookup(&config, &self.shared.options.repo) else {
            return;
        };
        let (path, base) = (repo.path.clone(), claimed.base.clone());
        if let Err(e) = blocking(move || git::set_aside(&path, &base, &item)).await {
            self.shared.trouble(format!(
                "the checkout could not be restored for a restart: {e}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use bosun_core::{Outcome, Park, ParkReason, Reason};

    use super::*;

    fn card(column: Column, labels: &[&str]) -> Card {
        Card {
            id: bosun_queue::ItemId(1),
            title: "One".to_owned(),
            column,
            body: String::new(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            draft: false,
            closed: false,
            comments: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn the_users_moves_are_commands() {
        use UserCommand::{Cancel, ManualDone, Requeue, Restart};
        let parked = State::Parked(Park {
            reason: ParkReason::Limit,
            until_ms: None,
            next: bosun_core::Next::Resume,
        });
        let needs = State::NeedsYou(Reason::AttemptsExhausted);
        let done = State::Done(Outcome::Pass);
        let cases = [
            (State::Running, None, Read::Command(Cancel)),
            (
                State::Verifying,
                Some(card(Column::Backlog, &[])),
                Read::Command(Cancel),
            ),
            (
                needs,
                Some(card(Column::Backlog, &[])),
                Read::Command(Cancel),
            ),
            (
                parked,
                Some(card(Column::Ready, &[])),
                Read::Command(Requeue),
            ),
            (
                needs,
                Some(card(Column::Ready, &[])),
                Read::Command(Requeue),
            ),
            (
                State::Cancelled,
                Some(card(Column::Ready, &[])),
                Read::Command(Requeue),
            ),
            (
                State::Landing,
                Some(card(Column::Done, &[])),
                Read::Command(ManualDone),
            ),
            (
                needs,
                Some(card(Column::Done, &[])),
                Read::Command(ManualDone),
            ),
            (
                State::Verifying,
                Some(card(Column::Verify, &[RESTART_LABEL])),
                Read::Command(Restart),
            ),
            // A Done item: cancel is a no-op, rework is not this stage's.
            (done, None, Read::Nothing),
            (done, Some(card(Column::Backlog, &[])), Read::Nothing),
            (done, Some(card(Column::Ready, &[])), Read::Revert),
            // Foreign changes on daemon-owned columns.
            (
                State::Running,
                Some(card(Column::Verify, &[])),
                Read::Revert,
            ),
            (needs, Some(card(Column::InProgress, &[])), Read::Revert),
            (done, Some(card(Column::NeedsYou, &[])), Read::Revert),
            // As the database says.
            (
                State::Running,
                Some(card(Column::InProgress, &[])),
                Read::Nothing,
            ),
            (
                State::Landing,
                Some(card(Column::Verify, &[])),
                Read::Nothing,
            ),
            // The user's own states, and the claim's.
            (
                State::Ready,
                Some(card(Column::InProgress, &[])),
                Read::Nothing,
            ),
            (
                State::Claimed,
                Some(card(Column::Ready, &[])),
                Read::Nothing,
            ),
            (State::Claimed, None, Read::Nothing),
        ];
        for (state, card, want) in cases {
            assert_eq!(
                read(state, card.as_ref(), false, false),
                want,
                "{state:?} {card:?}"
            );
        }
    }

    /// #111: closing the issue of a card is a cancel, in whatever column the card stays.
    #[test]
    fn a_closed_issue_is_a_cancel() {
        let closed = |column| Card {
            closed: true,
            ..card(column, &[])
        };
        let needs = State::NeedsYou(Reason::Lint);
        for (state, column) in [
            (State::Running, Column::InProgress),
            (State::Verifying, Column::Verify),
            (needs, Column::NeedsYou),
            // Moved to Ready and closed: closed wins, no retry.
            (needs, Column::Ready),
        ] {
            assert_eq!(
                read(state, Some(&closed(column)), false, false),
                Read::Command(UserCommand::Cancel),
                "{state:?} in {column:?}"
            );
        }
        for state in [State::Done(Outcome::Pass), State::Cancelled, State::Ready] {
            assert_eq!(
                read(state, Some(&closed(Column::Ready)), false, false),
                Read::Nothing,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_board_behind_the_outbox_is_not_a_change() {
        let needs = State::NeedsYou(Reason::Lint);
        // The Needs-you column write is still queued: the card is where the user left it.
        assert_eq!(
            read(needs, Some(&card(Column::Ready, &[])), true, false),
            Read::Nothing
        );
        assert_eq!(
            read(
                State::Verifying,
                Some(&card(Column::InProgress, &[])),
                true,
                false
            ),
            Read::Nothing
        );
        // A Done item Needs you since its pull request was closed (#8): its card is still in
        // Done until the column write goes out.
        let closed = State::NeedsYou(Reason::LandingRefused);
        assert_eq!(
            read(
                closed,
                Some(&card(Column::Done, &[])),
                Pending::PrClosed,
                false
            ),
            Read::Nothing
        );
        assert_eq!(
            read(closed, Some(&card(Column::Done, &[])), false, false),
            Read::Command(UserCommand::ManualDone)
        );
        // Any other pending write, in any landing mode, leaves the user's Done the user's: an
        // item that needs the user, its Needs-you comment or column write still queued, is
        // dragged to Done.
        for state in [needs, closed] {
            assert_eq!(
                read(state, Some(&card(Column::Done, &[])), true, false),
                Read::Command(UserCommand::ManualDone),
                "{state:?}"
            );
        }
        // A user's column, though, is always the user's.
        assert_eq!(
            read(State::Running, Some(&card(Column::Done, &[])), true, false),
            Read::Command(UserCommand::ManualDone)
        );
        assert_eq!(
            read(State::Running, Some(&card(Column::Ready, &[])), true, false),
            Read::Command(UserCommand::Requeue)
        );
    }

    #[test]
    fn a_restart_label_acted_on_is_not_a_second_restart() {
        let labelled = card(Column::InProgress, &[RESTART_LABEL]);
        assert_eq!(
            read(State::Running, Some(&labelled), false, true),
            Read::Nothing
        );
        assert_eq!(
            read(State::Ready, Some(&labelled), false, false),
            Read::Nothing
        );
    }
}
