//! The claim of #5, for every backend: no board offers compare-and-swap, so a claim is a write
//! and a re-read, and the user wins.
//!
//! The caller has already recorded the claim in its database. [`claim`] then:
//!
//! 1. moves the card to In progress ([`Queue::project`]);
//! 2. posts the daemon's **claim marker** on the item, a comment keyed `claim:<daemon id>`
//!    ([`Queue::comment`], so a replay leaves one);
//! 3. re-reads the card ([`Queue::lint_target`]).
//!
//! A re-read that shows another daemon's marker fails with [`QueueError::Contested`]: two
//! daemons point at one board (a second OS user, say), and the repository is refused. A
//! re-read that shows the card out of In progress means the user moved it between the write
//! and the re-read: [`Claim::Released`], and the caller releases its claim. Only a re-read
//! that shows the card in In progress with no other marker is [`Claim::Claimed`], and the card
//! it returns is what the snapshot is taken from.

use crate::card::{Card, Column, ItemId, Owned};
use crate::marker::valid_key;
use crate::queue::{Queue, QueueError};

/// The prefix of a claim marker's key; the daemon's id follows.
pub const CLAIM_KEY_PREFIX: &str = "claim:";

/// What a claim came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The card is in In progress and carries no other daemon's marker: the item is this
    /// daemon's. The card as the re-read saw it.
    Claimed(Card),
    /// The user moved the card between the claim write and the re-read: the user wins and the
    /// claim is released. The card as the re-read saw it.
    Released(Card),
}

/// The key of the claim marker of the daemon `daemon`.
#[must_use]
pub fn claim_key(daemon: &str) -> String {
    format!("{CLAIM_KEY_PREFIX}{daemon}")
}

/// The other daemons whose claim markers are on `card`, sorted; this daemon is `daemon`.
#[must_use]
pub fn other_claimants(card: &Card, daemon: &str) -> Vec<String> {
    let mut others: Vec<String> = card
        .comments
        .iter()
        .filter_map(|c| c.key.as_deref()?.strip_prefix(CLAIM_KEY_PREFIX))
        .filter(|id| *id != daemon)
        .map(str::to_owned)
        .collect();
    others.sort();
    others.dedup();
    others
}

/// Claims `card` for the daemon `daemon` (a stable id of this daemon: host and OS user, say),
/// as the module docs describe. `card` is the card the caller decided to claim, from its last
/// poll.
///
/// # Errors
///
/// [`QueueError::InvalidKey`] when `daemon` does not make a valid key; [`QueueError::Contested`]
/// when another daemon's marker is on the item; otherwise what the backend's calls fail with
/// ([`QueueError::is_cancel`] when the item left the board meanwhile).
pub fn claim(queue: &dyn Queue, card: &Card, daemon: &str) -> Result<Claim, QueueError> {
    let key = claim_key(daemon);
    if !valid_key(&key) {
        return Err(QueueError::InvalidKey(key));
    }
    let item: ItemId = card.id;
    queue.project(item, Owned::InProgress, None)?;
    queue.comment(item, &marker_body(daemon), &key)?;
    if cfg!(mutant_queue_skip_claim_reread) {
        // Mutant: the claim is taken for granted, whatever happened after the write.
        let mut claimed = card.clone();
        claimed.column = Column::InProgress;
        return Ok(Claim::Claimed(claimed));
    }
    let now = queue.lint_target(item)?;
    let others = other_claimants(&now, daemon);
    if !others.is_empty() {
        return Err(QueueError::Contested { item, others });
    }
    if now.column == Column::InProgress {
        Ok(Claim::Claimed(now))
    } else {
        Ok(Claim::Released(now))
    }
}

/// The visible text of the claim marker comment.
fn marker_body(daemon: &str) -> String {
    format!(
        "bosun: claimed by the daemon `{daemon}`. A second daemon on this board refuses the \
         repository when it finds this marker."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Comment;
    use crate::testsuite::card;

    #[test]
    fn other_claimants_skip_this_daemon_and_plain_comments() {
        let mut c = card(1, Column::InProgress, "true");
        for (key, author) in [
            (Some("claim:host-a/fox"), "a"),
            (Some("claim:host-b/fox"), "a"),
            (Some("claim:host-b/fox"), "a"),
            (Some("report:1"), "a"),
            (None, "a"),
        ] {
            c.comments.push(Comment {
                author: author.to_owned(),
                key: key.map(str::to_owned),
                body: String::new(),
            });
        }
        assert_eq!(other_claimants(&c, "host-a/fox"), ["host-b/fox"]);
        assert_eq!(other_claimants(&c, "host-b/fox"), ["host-a/fox"]);
    }

    #[test]
    fn a_daemon_id_that_makes_no_key_is_refused() {
        struct Never;
        impl Queue for Never {
            fn poll(&self, _: Option<&crate::Cursor>) -> Result<crate::Changes, QueueError> {
                unreachable!()
            }
            fn project(&self, _: ItemId, _: Owned, _: Option<&str>) -> Result<(), QueueError> {
                unreachable!("nothing is written")
            }
            fn comment(&self, _: ItemId, _: &str, _: &str) -> Result<(), QueueError> {
                unreachable!()
            }
            fn upsert_comment(&self, _: ItemId, _: &str, _: &str) -> Result<(), QueueError> {
                unreachable!()
            }
            fn add_label(&self, _: ItemId, _: &str) -> Result<(), QueueError> {
                unreachable!()
            }
            fn remove_label(&self, _: ItemId, _: &str) -> Result<(), QueueError> {
                unreachable!()
            }
            fn lint_target(&self, _: ItemId) -> Result<Card, QueueError> {
                unreachable!()
            }
        }
        let c = card(1, Column::Ready, "true");
        assert!(matches!(
            claim(&Never, &c, "a host with spaces"),
            Err(QueueError::InvalidKey(_))
        ));
    }
}
