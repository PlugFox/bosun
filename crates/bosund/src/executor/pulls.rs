//! After Done in the `pr` landing mode (#8, E6): an item is Done when its pull request is open,
//! and the daemon goes on asking the forge about it.
//!
//! On the loop's tick, at most once per [`super::Options::pr_poll`] and never while the kill
//! switch is thrown, every open pull request of a Done item of this repository (the `landing`
//! rows that say `open`) is asked about ([`super::forge::Forge::state`]):
//!
//! - merged: the row says `merged`. Final: it is not asked about again.
//! - closed unmerged: the item's [`NOTE_PR_CLOSED`] note (its number, its URL, and the
//!   transition it explains), then [`Event::PullRequestClosed`] applied as to any item not in
//!   flight (Needs you with one comment, which names the pull request), then the row says
//!   `closed`. A crash in between asks again; the event, applied twice, adds nothing.
//! - open, or no answer, or a refusal: nothing changes. A failure is reported once per item
//!   until the forge answers again.
//!
//! A blocker of a repository in the `pr` mode satisfies `blocked-by` only once its pull request
//! is merged ([`waits_for_merge`], E6): one still open holds its dependants as any blocker not
//! Done does. A card the user closed by hand, a blocker of another mode and one without a
//! `landing` row satisfy as soon as they are Done.

use std::collections::BTreeSet;
use std::time::Duration;

use bosun_config::{LandingMode, Repo};
use bosun_core::{Event, Hold, ItemId, Outcome, State};
use bosun_identity::{Env, inherited};
use bosun_store::{LandingRow, PrState, Write};

use super::forge::{At, PullRequestState};
use super::{Loop, blocking, identity, lookup, now_ms};

/// The kind of the item note a pull request closed unmerged leaves (`Write::ItemNote`): its
/// number, its URL and the transition its Needs-you comment belongs to (`seq`).
pub(crate) const NOTE_PR_CLOSED: &str = "pr_closed";

/// The loop's own state for the poll.
#[derive(Debug, Default)]
pub(super) struct Watch {
    /// When the forge was last asked (the loop's monotonic clock).
    last: Option<Duration>,
    /// Items whose pull request could not be asked about, reported once until it answers.
    troubled: BTreeSet<ItemId>,
}

impl Watch {
    fn due(&mut self, now: Duration, every: Duration) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_sub(last) < every)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// Whether a Done blocker in state `state`, whose pull request is `open` (its `landing` row
/// says so), still holds its dependants: a verdict that passed, a pull request not merged.
pub(super) fn waits_for_merge(state: State, open: bool) -> bool {
    open && matches!(state, State::Done(Outcome::Pass | Outcome::Flaky))
}

impl Loop {
    /// The items of this repository whose pull request is open, when its landing mode is `pr`;
    /// empty in any other mode. `None` when the store cannot be read.
    pub(super) async fn unmerged(&self, repo: &Repo) -> Option<BTreeSet<ItemId>> {
        // Mutant (`mutant_loop_pr_open_releases_blockers`): a Done blocker satisfies whatever
        // its pull request's state.
        if repo.landing.mode != LandingMode::Pr || cfg!(mutant_loop_pr_open_releases_blockers) {
            return Some(BTreeSet::new());
        }
        match self.open_rows().await {
            Ok(rows) => Some(rows.into_iter().map(|r| r.item).collect()),
            Err(e) => {
                self.shared
                    .trouble(format!("the open pull requests cannot be read: {e}"));
                None
            }
        }
    }

    /// The `landing` rows of this repository's items that say `open`.
    async fn open_rows(&self) -> Result<Vec<LandingRow>, bosun_store::Error> {
        let prefix = format!("{}#", self.shared.options.repo);
        Ok(self
            .shared
            .deps
            .reader
            .open_pull_requests()
            .await?
            .into_iter()
            .filter(|r| r.item.0.starts_with(&prefix))
            .collect())
    }

    /// Asks the forge about the open pull requests of this repository's Done items, when due.
    pub(super) async fn poll_pull_requests(&mut self) {
        let now = self.shared.deps.clock.mono();
        if !self.pulls.due(now, self.shared.options.pr_poll) {
            return;
        }
        if self.hold(self.shared.deps.clock.wall_ms()) == Some(Hold::Kill) {
            return;
        }
        let rows = match self.open_rows().await {
            Ok(rows) => rows,
            Err(e) => {
                self.shared
                    .trouble(format!("the open pull requests cannot be read: {e}"));
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        let config = self.config();
        let Some((repo, identity)) = lookup(&config, &self.shared.options.repo) else {
            return;
        };
        // The daemon's own `gh` for the repository's identity, never an agent's (#2).
        let bundle = match identity::bundle(&repo.identity, identity, &repo.path) {
            Ok(bundle) => bundle,
            Err(e) => {
                for row in &rows {
                    if self.pulls.troubled.insert(row.item.clone()) {
                        self.shared.trouble(format!(
                            "{}: its pull request cannot be asked about: {e}",
                            row.item
                        ));
                    }
                }
                return;
            }
        };
        let at = At {
            dir: repo.path.clone(),
            env: Env::tool(&bundle, &inherited()),
        };
        for row in rows {
            let Some(number) = row.pr_number else {
                continue;
            };
            let stored = match self.shared.deps.reader.item(&row.item).await {
                Ok(stored) => stored,
                Err(e) => {
                    self.shared
                        .trouble(format!("the store cannot be read: {e}"));
                    return;
                }
            };
            // Only a Done item's: one requeued since lands again under a row of its own.
            let Some(item) = stored
                .map(|s| s.item)
                .filter(|i| matches!(i.state, State::Done(_)))
            else {
                continue;
            };
            let (forge, asked) = (self.shared.deps.forge.clone(), at.clone());
            let state = match blocking(move || forge.state(&asked, number)).await {
                Ok(state) => {
                    self.pulls.troubled.remove(&row.item);
                    state
                }
                Err(e) => {
                    if self.pulls.troubled.insert(row.item.clone()) {
                        self.shared.trouble(format!(
                            "{}: pull request #{number} cannot be asked about: {e}; it is asked \
                             again",
                            row.item
                        ));
                    }
                    continue;
                }
            };
            match state {
                PullRequestState::Open => {}
                // Mutant (`mutant_loop_pr_closed_ignored`): a pull request closed unmerged is
                // taken for an open one.
                PullRequestState::Closed if cfg!(mutant_loop_pr_closed_ignored) => {}
                PullRequestState::Merged => self.store_pr_state(&row, PrState::Merged).await,
                PullRequestState::Closed => {
                    self.pr_closed(&item, &row, number).await;
                    if self.halted {
                        return;
                    }
                    self.store_pr_state(&row, PrState::Closed).await;
                }
            }
        }
    }

    /// Pull request `number` of `item` was closed unmerged: its note, then the event.
    async fn pr_closed(&mut self, item: &bosun_core::Item, row: &LandingRow, number: u64) {
        let at_ms = now_ms();
        let note = Write::ItemNote {
            item: item.id.clone(),
            kind: NOTE_PR_CLOSED.to_owned(),
            body: serde_json::json!({
                "number": number,
                "url": row.pr_url,
                "seq": item.seq + 1,
                "at_ms": at_ms,
            })
            .to_string(),
            at_ms,
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.shared.trouble(format!(
                "the closed pull request of {} cannot be noted: {e}",
                item.id
            ));
        }
        crate::log::log!(
            "bosund: repository {}: {}: pull request #{number} was closed without being merged",
            self.shared.options.repo,
            item.id
        );
        self.bounce(item, &Event::PullRequestClosed).await;
    }

    /// The row of `row`'s own attempt says `state`; every other column keeps its value.
    async fn store_pr_state(&self, row: &LandingRow, state: PrState) {
        let update = LandingRow {
            item: row.item.clone(),
            attempt: row.attempt,
            target: row.target.clone(),
            head_sha: None,
            verified_tree: None,
            landed_sha: None,
            delivered_ms: None,
            pr_number: None,
            pr_url: None,
            pr_state: Some(state),
            updated_ms: now_ms(),
        };
        if let Err(e) = self
            .shared
            .deps
            .writer
            .write(vec![Write::Landing(update)])
            .await
        {
            self.shared.trouble(format!(
                "{}: the state of its pull request cannot be stored: {e}",
                row.item
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_passed_blocker_with_an_open_pull_request_waits() {
        for outcome in [Outcome::Pass, Outcome::Flaky] {
            assert!(waits_for_merge(State::Done(outcome), true));
            assert!(!waits_for_merge(State::Done(outcome), false));
        }
        assert!(!waits_for_merge(State::Done(Outcome::UserClosed), true));
    }

    #[test]
    fn the_poll_is_due_once_per_period() {
        let mut watch = Watch::default();
        let every = Duration::from_mins(5);
        assert!(watch.due(Duration::from_secs(1), every));
        assert!(!watch.due(Duration::from_secs(60), every));
        assert!(watch.due(Duration::from_secs(301), every));
    }
}
