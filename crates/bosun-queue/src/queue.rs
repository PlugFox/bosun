//! The `Queue` trait of #5, what `poll` returns, and the errors of a backend.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::card::{Card, Column, ItemId, Owned};

/// A source of cards and a projection of the daemon's states onto it (#5).
///
/// A backend never decides anything: it reports what the board holds and writes what it is
/// told. Every write is idempotent, so the outbox (#5) may replay it after a crash.
pub trait Queue {
    /// The board now, and what changed since `cursor` (`None`: since nothing, so every card is
    /// [`Changes::added`]). A cursor this backend cannot read counts as `None`.
    ///
    /// # Errors
    ///
    /// The board cannot be read, or what it holds is not valid (a local file mid-edit).
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError>;

    /// Moves the card of `item` into the daemon-owned `state`, with an optional one-line
    /// `note` the backend may show beside it (the local file writes it as `note`; `None`
    /// clears it). Writing the state the card already has is a no-op.
    ///
    /// # Errors
    ///
    /// [`QueueError::NotFound`] when the item has no card; otherwise the write failed.
    fn project(&self, item: ItemId, state: Owned, note: Option<&str>) -> Result<(), QueueError>;

    /// Posts `body` as a new comment on `item`, unless a comment with `key` already exists:
    /// replayed with the same key, it leaves one comment (and does not edit it). A keyed
    /// comment the user deleted is not posted again. `body` may already start with the marker
    /// of `key` ([`crate::marker`]); it is not doubled.
    ///
    /// # Errors
    ///
    /// [`QueueError::InvalidKey`], [`QueueError::NotFound`], or the write failed.
    fn comment(&self, item: ItemId, body: &str, key: &str) -> Result<(), QueueError>;

    /// Creates the comment with `key` on `item`, or replaces its body. The lookup by key runs
    /// before every create, so a crash between creating a comment and recording that it exists
    /// never yields a second one; a comment the user deleted is recreated once, and left
    /// deleted when the user deletes it again.
    ///
    /// # Errors
    ///
    /// [`QueueError::InvalidKey`], [`QueueError::NotFound`], or the write failed.
    fn upsert_comment(&self, item: ItemId, key: &str, body: &str) -> Result<(), QueueError>;

    /// The card of `item` as the card lint reads it, fresh from the board.
    ///
    /// # Errors
    ///
    /// [`QueueError::NotFound`] when the item has no card; otherwise the board cannot be read.
    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError>;
}

/// Where a previous [`Queue::poll`] left off. Opaque to the caller, who stores it as text
/// ([`Cursor::as_str`], [`Cursor::from_text`]) and hands it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    /// The cursor as text, for storage.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A cursor read back from storage. Text that no backend wrote is accepted and treated as
    /// no cursor at all.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

/// What [`Queue::poll`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// The cursor for the next poll.
    pub cursor: Cursor,
    /// Every card on the board, in board order: by column, and in each column top to bottom.
    pub cards: Vec<Card>,
    /// Cards that were not on the board at the cursor.
    pub added: Vec<ItemId>,
    /// Cards whose title, body, labels, draft flag or dependencies changed since the cursor. Comments are not
    /// tracked here: they are read from [`Changes::cards`] when the item is snapshotted.
    pub edited: Vec<ItemId>,
    /// Cards that changed column since the cursor.
    pub moved: Vec<ItemId>,
    /// Cards that were on the board at the cursor and are gone.
    pub removed: Vec<ItemId>,
    /// Columns in which cards present at both polls changed their relative order.
    pub reordered: Vec<Column>,
}

impl Changes {
    /// The changes from `cursor` to `cards`, which must be in board order. Every backend builds
    /// its [`Changes`] here, so they all report the same deltas for the same boards.
    #[must_use]
    pub fn since(cursor: Option<&Cursor>, mut cards: Vec<Card>) -> Self {
        // Board order: by column, stable within one.
        cards.sort_by_key(|card| card.column);
        let now: Vec<Mark> = cards.iter().map(Mark::of).collect();
        let before = cursor
            .and_then(|c| Mark::read_all(&c.0))
            .unwrap_or_default();
        let before_by_id: BTreeMap<ItemId, &Mark> = before.iter().map(|m| (m.id, m)).collect();
        let now_by_id: BTreeMap<ItemId, &Mark> = now.iter().map(|m| (m.id, m)).collect();

        let mut changes = Self {
            cursor: Cursor(Mark::write_all(&now)),
            cards: Vec::new(),
            added: Vec::new(),
            edited: Vec::new(),
            moved: Vec::new(),
            removed: Vec::new(),
            reordered: Vec::new(),
        };
        for mark in &now {
            match before_by_id.get(&mark.id) {
                None => changes.added.push(mark.id),
                Some(old) => {
                    if old.digest != mark.digest {
                        changes.edited.push(mark.id);
                    }
                    if old.column != mark.column {
                        changes.moved.push(mark.id);
                    }
                }
            }
        }
        changes.removed = before
            .iter()
            .filter(|m| !now_by_id.contains_key(&m.id))
            .map(|m| m.id)
            .collect();
        for column in Column::ALL {
            // The relative order of the cards that stayed in this column.
            let stayed = |marks: &[Mark], other: &BTreeMap<ItemId, &Mark>| -> Vec<ItemId> {
                marks
                    .iter()
                    .filter(|m| {
                        m.column == column && other.get(&m.id).is_some_and(|o| o.column == column)
                    })
                    .map(|m| m.id)
                    .collect()
            };
            if stayed(&before, &now_by_id) != stayed(&now, &before_by_id) {
                changes.reordered.push(column);
            }
        }
        changes.cards = cards;
        changes
    }

    /// Whether anything changed since the cursor.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.edited.is_empty()
            && self.moved.is_empty()
            && self.removed.is_empty()
            && self.reordered.is_empty()
    }

    /// The card of `item`, if it is on the board.
    #[must_use]
    pub fn card(&self, item: ItemId) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == item)
    }
}

/// One card at one poll, as a cursor remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    id: ItemId,
    column: Column,
    /// A digest of the title, body, labels, draft flag and dependencies.
    digest: String,
}

const CURSOR_VERSION: &str = "q1";

impl Mark {
    fn of(card: &Card) -> Self {
        let mut hasher = Sha256::new();
        for part in [card.title.as_str(), card.body.as_str()]
            .into_iter()
            .chain(card.labels.iter().map(String::as_str))
        {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        hasher.update([u8::from(card.draft)]);
        for blocker in &card.blocked_by {
            hasher.update(blocker.0.to_le_bytes());
        }
        let digest = hasher.finalize();
        Self {
            id: card.id,
            column: card.column,
            digest: crate::hex(&digest[..12]),
        }
    }

    fn write_all(marks: &[Self]) -> String {
        let mut text = CURSOR_VERSION.to_owned();
        for mark in marks {
            let column = Column::ALL.iter().position(|c| *c == mark.column);
            let _ = write!(
                text,
                ";{},{},{}",
                mark.id.0,
                column.unwrap_or_default(),
                mark.digest
            );
        }
        text
    }

    fn read_all(text: &str) -> Option<Vec<Self>> {
        let mut parts = text.split(';');
        if parts.next()? != CURSOR_VERSION {
            return None;
        }
        parts
            .map(|part| {
                let mut fields = part.split(',');
                let id = ItemId(fields.next()?.parse().ok()?);
                let column = *Column::ALL.get(fields.next()?.parse::<usize>().ok()?)?;
                let digest = fields.next()?.to_owned();
                fields
                    .next()
                    .is_none()
                    .then_some(Self { id, column, digest })
            })
            .collect()
    }
}

/// What a backend remembers of the keyed comments it posted or found, for the deletion rules
/// of #5: a keyed comment the user deleted is never reposted by [`Queue::comment`], and is
/// recreated by [`Queue::upsert_comment`] once, not in a loop.
///
/// The record lives as long as the backend: after a restart a comment deleted before it may be
/// recreated once more, never more.
#[cfg(any(feature = "local_file", feature = "projects_v2"))]
#[derive(Debug, Default)]
pub(crate) struct Keyed(std::sync::Mutex<BTreeMap<(ItemId, String), bool>>);

#[cfg(any(feature = "local_file", feature = "projects_v2"))]
impl Keyed {
    /// Notes that the comment with `key` exists on `item` (found by key, or created).
    pub(crate) fn seen(&self, item: ItemId, key: &str) {
        self.lock().entry((item, key.to_owned())).or_insert(false);
    }

    /// Whether a comment with `key` that is missing from `item` now is to be created, and if
    /// so whether that is a recreation: `Some(false)` for a first creation (or one after a
    /// crash lost the record), `Some(true)` to recreate a comment the user deleted, `None` to
    /// leave it deleted. `upsert` tells `upsert_comment` from `comment`.
    pub(crate) fn create_missing(&self, item: ItemId, key: &str, upsert: bool) -> Option<bool> {
        match self.lock().get(&(item, key.to_owned())) {
            None => Some(false),
            Some(false) if upsert => Some(true),
            // Mutant: a sticky comment deleted again is recreated again, without end.
            Some(true) if upsert && cfg!(mutant_queue_recreate_always) => Some(true),
            Some(_) => None,
        }
    }

    /// Notes that the comment with `key` was created on `item`, `recreated` as
    /// [`Keyed::create_missing`] said.
    pub(crate) fn created(&self, item: ItemId, key: &str, recreated: bool) {
        let mut map = self.lock();
        let entry = map.entry((item, key.to_owned())).or_insert(false);
        *entry |= recreated;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(ItemId, String), bool>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Why a backend could not do what it was asked.
#[derive(Debug)]
pub enum QueueError {
    /// The item has no card on the board: its issue does not exist (or was deleted), or its
    /// card was removed. For an item in flight this is a cancel (#5).
    NotFound(ItemId),
    /// The item's issue left the repository mid-item (#5): a cancel, never retried.
    Gone(ItemId, Gone),
    /// A comment key the backend cannot carry; see [`crate::marker::valid_key`].
    InvalidKey(String),
    /// The board's content is not valid: a local file that does not parse, two cards with one
    /// number. Nothing is written over it; the next poll after the user fixes it succeeds.
    Invalid(String),
    /// The board kept changing under a write (the user editing the local file); nothing was
    /// written. Retrying later is safe.
    Conflict(String),
    /// An I/O error reading or writing the board.
    Io(io::Error),
    /// A remote board could not be asked, or answered with an error (a GraphQL backend).
    /// Retrying later may succeed.
    Remote(String),
    /// The remote board refused the request for its rate limit: retry after
    /// [`RateLimited::wait`], in order, without a burst.
    RateLimited(RateLimited),
    /// The identity's credentials no longer allow the request: a token that lost a scope, was
    /// revoked, or cannot see the project. Not transient: the repository pauses with this
    /// error until the user fixes the token.
    Scope(String),
    /// The board does not match the configuration: a project, its Status field or a column's
    /// option is missing, ambiguous, or was renamed since start. Not transient: the repository
    /// pauses with this error until the user fixes the board or the configuration.
    Setup(String),
    /// Another daemon claims items on this board (#5): its claim marker is on an item this
    /// daemon claimed. Not transient: the repository is refused until the user stops one of
    /// them (and deletes the marker comment, when the other daemon is gone for good).
    Contested {
        /// The item where the other daemon's marker was found.
        item: ItemId,
        /// The other daemons' ids, as their markers name them.
        others: Vec<String>,
    },
}

/// How an item's issue left the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gone {
    /// Transferred to the repository named here (`owner/name`).
    Transferred(String),
    /// Converted to a discussion.
    Converted,
}

/// A rate-limit refusal of a remote board (#5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimited {
    /// A secondary limit (too many requests or mutations in a short time) rather than the
    /// hourly quota.
    pub secondary: bool,
    /// The wait the server named (`Retry-After`), if it named one.
    pub retry_after: Option<Duration>,
    /// When the hourly quota resets, in Unix seconds, if the server said.
    pub reset: Option<u64>,
}

impl RateLimited {
    /// The least wait before a retry: `Retry-After` when the server named it; for an exhausted
    /// quota, the time from `now` (Unix seconds) to the reset; otherwise a minute, which is
    /// what GitHub asks for after a secondary limit that names no wait.
    #[must_use]
    pub fn wait(&self, now: u64) -> Duration {
        if let Some(after) = self.retry_after {
            return after;
        }
        match self.reset {
            Some(reset) if !self.secondary => Duration::from_secs(reset.saturating_sub(now)),
            _ => Duration::from_secs(60),
        }
    }
}

impl QueueError {
    /// Whether retrying the same call later may succeed with nothing else changed (the outbox
    /// keeps the row).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Conflict(_)
                | Self::Io(_)
                | Self::Invalid(_)
                | Self::Remote(_)
                | Self::RateLimited(_)
        )
    }

    /// Whether the item is no longer on the board (its issue deleted, transferred or
    /// converted, its card removed): the loop cancels an item in flight (#5), and the outbox
    /// drops its rows.
    #[must_use]
    pub fn is_cancel(&self) -> bool {
        matches!(self, Self::NotFound(_) | Self::Gone(..))
    }

    /// Whether the whole repository pauses with this error until the user acts.
    #[must_use]
    pub fn pauses_repository(&self) -> bool {
        matches!(
            self,
            Self::Setup(_) | Self::Scope(_) | Self::Contested { .. }
        )
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(item) => write!(f, "item {item} has no card on the board"),
            Self::Gone(item, Gone::Transferred(to)) => {
                write!(f, "the issue of item {item} was transferred to {to}")
            }
            Self::Gone(item, Gone::Converted) => {
                write!(f, "the issue of item {item} was converted to a discussion")
            }
            Self::InvalidKey(key) => write!(f, "comment key `{key}` is not valid"),
            Self::Invalid(why) => write!(f, "the board is not valid: {why}"),
            Self::Conflict(why) => write!(f, "the board changed during a write: {why}"),
            Self::Io(e) => write!(f, "board I/O failed: {e}"),
            Self::Remote(why) => write!(f, "the board cannot be read: {why}"),
            Self::RateLimited(limit) => {
                let kind = if limit.secondary {
                    "a secondary rate limit"
                } else {
                    "the rate limit"
                };
                match limit.retry_after {
                    Some(after) => write!(f, "the board refused for {kind}; retry after {after:?}"),
                    None => write!(f, "the board refused for {kind}"),
                }
            }
            Self::Scope(why) => write!(f, "the identity's token cannot do this: {why}"),
            Self::Setup(why) => write!(f, "the board does not match the configuration: {why}"),
            Self::Contested { item, others } => write!(
                f,
                "another daemon claims items on this board ({}; its marker is on item {item}); \
                 the repository is refused",
                others.join(", ")
            ),
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for QueueError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: u64, column: Column, body: &str) -> Card {
        Card {
            id: ItemId(id),
            title: format!("card {id}"),
            column,
            body: body.to_owned(),
            labels: Vec::new(),
            draft: false,
            comments: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn the_first_poll_adds_everything_in_board_order() {
        let changes = Changes::since(
            None,
            vec![
                card(2, Column::Done, ""),
                card(1, Column::Ready, ""),
                card(3, Column::Ready, ""),
            ],
        );
        assert_eq!(changes.added, [ItemId(1), ItemId(3), ItemId(2)]);
        let order: Vec<_> = changes.cards.iter().map(|c| c.id).collect();
        assert_eq!(order, [ItemId(1), ItemId(3), ItemId(2)]);
    }

    #[test]
    fn deltas_since_a_cursor() {
        let first = Changes::since(
            None,
            vec![
                card(1, Column::Ready, "a"),
                card(2, Column::Ready, "b"),
                card(3, Column::Ready, "c"),
                card(4, Column::Backlog, "d"),
            ],
        );
        let second = Changes::since(
            Some(&first.cursor),
            vec![
                card(2, Column::Ready, "b"),
                card(1, Column::Ready, "a"),
                card(3, Column::Ready, "c2"),
                card(5, Column::Ready, "e"),
                card(4, Column::Done, "d"),
            ],
        );
        assert_eq!(second.added, [ItemId(5)]);
        assert_eq!(second.edited, [ItemId(3)]);
        assert_eq!(second.moved, [ItemId(4)]);
        assert_eq!(second.removed, []);
        assert_eq!(second.reordered, [Column::Ready]);

        let third = Changes::since(Some(&second.cursor), second.cards.clone());
        assert!(third.is_empty(), "{third:?}");
        let gone = Changes::since(Some(&third.cursor), Vec::new());
        assert_eq!(gone.removed.len(), 5);
    }

    #[test]
    fn a_foreign_cursor_counts_as_none() {
        let changes = Changes::since(
            Some(&Cursor::from_text("not a cursor")),
            vec![card(1, Column::Ready, "")],
        );
        assert_eq!(changes.added, [ItemId(1)]);
        let round = Cursor::from_text(changes.cursor.as_str());
        assert!(Changes::since(Some(&round), changes.cards).is_empty());
    }
}
