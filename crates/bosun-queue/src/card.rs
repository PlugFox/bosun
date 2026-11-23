//! What a backend reports about a card: its item, column, body, labels and comments.

use std::fmt;

/// An item on the board: the number of the issue behind the card (`#N`), or the card's own
/// number in a backend without issues (the local file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u64);

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A column of the board (#5). The order of the variants is the board's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Column {
    /// Ignored by the daemon; the user's and the proposals' column.
    Backlog,
    /// Claimable, in board order. Only the user moves a card here.
    Ready,
    /// Daemon-owned: the item is claimed and running.
    InProgress,
    /// Daemon-owned: the verdict is running or the item is landing.
    Verify,
    /// Daemon-owned (or moved here by the user, recorded as user-closed).
    Done,
    /// Daemon-owned: the item needs the user; the reason is a comment.
    NeedsYou,
}

impl Column {
    /// Every column, in board order.
    pub const ALL: [Self; 6] = [
        Self::Backlog,
        Self::Ready,
        Self::InProgress,
        Self::Verify,
        Self::Done,
        Self::NeedsYou,
    ];

    /// The column's name on the board.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Ready => "Ready",
            Self::InProgress => "In progress",
            Self::Verify => "Verify",
            Self::Done => "Done",
            Self::NeedsYou => "Needs you",
        }
    }

    /// The column named `name`, ignoring ASCII case and treating `-`, `_` and a space alike:
    /// `In progress`, `in-progress` and `IN_PROGRESS` are one column.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let wanted = fold(name);
        Self::ALL.into_iter().find(|c| fold(c.name()) == wanted)
    }

    /// Whether the daemon owns the column: it moves cards in, and reads them as its own state.
    #[must_use]
    pub fn is_daemon_owned(self) -> bool {
        Owned::from_column(self).is_some()
    }
}

fn fold(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '-' | '_' => ' ',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A daemon-owned state, the only thing [`Queue::project`](crate::Queue::project) writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owned {
    /// [`Column::InProgress`].
    InProgress,
    /// [`Column::Verify`].
    Verify,
    /// [`Column::Done`].
    Done,
    /// [`Column::NeedsYou`].
    NeedsYou,
}

impl Owned {
    /// The column this state is shown in.
    #[must_use]
    pub fn column(self) -> Column {
        match self {
            Self::InProgress => Column::InProgress,
            Self::Verify => Column::Verify,
            Self::Done => Column::Done,
            Self::NeedsYou => Column::NeedsYou,
        }
    }

    /// The state shown in `column`, if the daemon owns it.
    #[must_use]
    pub fn from_column(column: Column) -> Option<Self> {
        match column {
            Column::Backlog | Column::Ready => None,
            Column::InProgress => Some(Self::InProgress),
            Column::Verify => Some(Self::Verify),
            Column::Done => Some(Self::Done),
            Column::NeedsYou => Some(Self::NeedsYou),
        }
    }
}

/// A comment on a card's item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Who wrote it: a login on GitHub, a free name in the local file.
    pub author: String,
    /// The idempotence key of a comment the daemon wrote ([`Queue::comment`]); `None` for
    /// everyone else's.
    ///
    /// [`Queue::comment`]: crate::Queue::comment
    pub key: Option<String>,
    /// The text, without the hidden key marker.
    pub body: String,
}

/// A card as a backend sees it: everything the loop and the card lint read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The item behind the card.
    pub id: ItemId,
    /// The item's title.
    pub title: String,
    /// The column the card is in.
    pub column: Column,
    /// The issue body: prose for the agent and the contract block (see [`crate::contract`]).
    pub body: String,
    /// The item's labels, in the backend's order.
    pub labels: Vec<String>,
    /// A draft card: no issue behind it. Ignored outside Backlog; fails lint in Ready.
    pub draft: bool,
    /// The item's comments, oldest first.
    pub comments: Vec<Comment>,
}

impl Card {
    /// The comment written with `key`, if any.
    #[must_use]
    pub fn comment_by_key(&self, key: &str) -> Option<&Comment> {
        self.comments.iter().find(|c| c.key.as_deref() == Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_names_round_trip_and_fold() {
        for column in Column::ALL {
            assert_eq!(Column::from_name(column.name()), Some(column));
        }
        assert_eq!(Column::from_name("in-progress"), Some(Column::InProgress));
        assert_eq!(Column::from_name(" NEEDS_YOU "), Some(Column::NeedsYou));
        assert_eq!(Column::from_name("Waiting"), None);
    }

    #[test]
    fn only_the_daemon_columns_are_owned() {
        let owned: Vec<_> = Column::ALL
            .into_iter()
            .filter(|c| c.is_daemon_owned())
            .collect();
        assert_eq!(
            owned,
            [
                Column::InProgress,
                Column::Verify,
                Column::Done,
                Column::NeedsYou
            ]
        );
        for column in owned {
            assert_eq!(Owned::from_column(column).map(Owned::column), Some(column));
        }
    }
}
