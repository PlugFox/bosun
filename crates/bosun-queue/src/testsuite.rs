//! The backend-agnostic trait test suite (#5: "the local-file backend passes the same trait test
//! suite as the Projects backend's fake"). Enabled by the `testsuite` feature.
//!
//! A backend's tests implement [`Harness`] (the backend, and a way to act as the user) and call
//! [`run`], or each check as its own test so that a failure names the check:
//!
//! ```ignore
//! #[test]
//! fn suite() {
//!     bosun_queue::testsuite::run(MyHarness::new);
//! }
//! ```
//!
//! Every check starts from an empty board. Bodies are compared after
//! [`normalise`](crate::snapshot::normalise), so a backend may trim them.

use crate::card::{Card, Column, Comment, ItemId, Owned};
use crate::queue::{Changes, Queue, QueueError};
use crate::snapshot::normalise;

/// A backend under test, and the user editing its board.
pub trait Harness {
    /// The backend.
    fn queue(&self) -> &dyn Queue;

    /// Acts as the user: afterwards the board holds exactly `cards`, in this order, with
    /// their columns, bodies, labels and comments.
    fn user_writes(&mut self, cards: &[Card]);
}

/// One check of the suite.
pub type Check = fn(&mut dyn Harness);

/// Every check, by name.
pub const CHECKS: &[(&str, Check)] = &[
    (
        "edit_addition_and_reorder_appear_in_the_next_poll",
        edit_addition_and_reorder_appear_in_the_next_poll,
    ),
    (
        "a_reorder_alone_appears_in_the_next_poll",
        a_reorder_alone_appears_in_the_next_poll,
    ),
    (
        "comment_replayed_with_one_key_leaves_one_comment",
        comment_replayed_with_one_key_leaves_one_comment,
    ),
    (
        "upsert_replayed_with_one_key_leaves_one_comment",
        upsert_replayed_with_one_key_leaves_one_comment,
    ),
    (
        "upsert_recreates_a_deleted_comment",
        upsert_recreates_a_deleted_comment,
    ),
    (
        "a_sticky_deleted_twice_is_recreated_once",
        a_sticky_deleted_twice_is_recreated_once,
    ),
    (
        "a_deleted_comment_is_not_posted_again",
        a_deleted_comment_is_not_posted_again,
    ),
    (
        "a_body_with_its_marker_keeps_one_marker",
        a_body_with_its_marker_keeps_one_marker,
    ),
    ("project_is_idempotent", project_is_idempotent),
    (
        "unknown_items_and_keys_are_refused",
        unknown_items_and_keys_are_refused,
    ),
    (
        "lint_target_reads_the_live_card",
        lint_target_reads_the_live_card,
    ),
];

/// Runs every check, each on a fresh harness from `new`.
///
/// # Panics
///
/// When a check fails, naming it.
pub fn run<H: Harness>(mut new: impl FnMut() -> H) {
    for (name, check) in CHECKS {
        let mut harness = new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&mut harness)));
        if let Err(panic) = result {
            eprintln!("queue test suite: `{name}` failed");
            std::panic::resume_unwind(panic);
        }
    }
}

/// A card in `column` whose body carries `verdict`.
#[must_use]
pub fn card(id: u64, column: Column, verdict: &str) -> Card {
    Card {
        id: ItemId(id),
        title: format!("Card {id}"),
        column,
        body: format!("Work for card {id}.\n\n```bosun\nverdict = {verdict:?}\n```\n"),
        labels: Vec::new(),
        draft: false,
        comments: Vec::new(),
        blocked_by: Vec::new(),
    }
}

fn poll(harness: &dyn Harness, cursor: Option<&crate::Cursor>) -> Changes {
    harness.queue().poll(cursor).expect("poll")
}

fn order(changes: &Changes) -> Vec<u64> {
    changes.cards.iter().map(|c| c.id.0).collect()
}

fn keyed<'c>(changes: &'c Changes, item: u64, key: &str) -> Vec<&'c Comment> {
    changes
        .card(ItemId(item))
        .expect("the card is on the board")
        .comments
        .iter()
        .filter(|c| c.key.as_deref() == Some(key))
        .collect()
}

/// An edit, an addition and a two-card reorder all appear in the next poll (#51).
///
/// # Panics
///
/// When the backend fails the check.
pub fn edit_addition_and_reorder_appear_in_the_next_poll(harness: &mut dyn Harness) {
    let mut cards = vec![
        card(1, Column::Ready, "true"),
        card(2, Column::Ready, "true"),
        card(3, Column::Ready, "true"),
        card(4, Column::Backlog, "true"),
    ];
    harness.user_writes(&cards);
    let first = poll(harness, None);
    assert_eq!(order(&first), [4, 1, 2, 3]);
    assert_eq!(first.added.len(), 4);

    cards.swap(0, 1);
    cards[2].body = cards[2].body.replace("Work", "Edited work");
    cards.insert(3, card(5, Column::Ready, "true"));
    harness.user_writes(&cards);
    let second = poll(harness, Some(&first.cursor));
    assert_eq!(order(&second), [4, 2, 1, 3, 5]);
    assert_eq!(second.added, [ItemId(5)]);
    assert_eq!(second.edited, [ItemId(3)]);
    assert_eq!(second.reordered, [Column::Ready]);
    assert!(second.moved.is_empty() && second.removed.is_empty());
    assert!(
        normalise(&second.card(ItemId(3)).unwrap().body).starts_with("Edited work"),
        "the edit is in the card"
    );

    let third = poll(harness, Some(&second.cursor));
    assert!(third.is_empty(), "nothing changed: {third:?}");
}

/// Two cards swapped and nothing else changed: the reorder appears in the next poll (#51).
///
/// # Panics
///
/// When the backend fails the check.
pub fn a_reorder_alone_appears_in_the_next_poll(harness: &mut dyn Harness) {
    let mut cards = vec![
        card(1, Column::Ready, "true"),
        card(2, Column::Ready, "true"),
    ];
    harness.user_writes(&cards);
    let first = poll(harness, None);
    assert_eq!(order(&first), [1, 2]);

    cards.swap(0, 1);
    harness.user_writes(&cards);
    let second = poll(harness, Some(&first.cursor));
    assert_eq!(order(&second), [2, 1], "the board order is the new one");
    assert_eq!(second.reordered, [Column::Ready]);
    assert!(second.added.is_empty() && second.edited.is_empty());
}

/// `comment` replayed with one key leaves one comment, and a replay does not edit it (#51).
///
/// # Panics
///
/// When the backend fails the check.
pub fn comment_replayed_with_one_key_leaves_one_comment(harness: &mut dyn Harness) {
    harness.user_writes(&[card(1, Column::Ready, "true")]);
    let queue = harness.queue();
    for _ in 0..3 {
        queue
            .comment(
                ItemId(1),
                "Needs you: the verdict fails.",
                "item-1/needs-you-1",
            )
            .expect("comment");
    }
    queue
        .comment(ItemId(1), "a later body", "item-1/needs-you-1")
        .expect("comment");
    queue
        .comment(ItemId(1), "another event", "item-1/needs-you-2")
        .expect("comment");
    let changes = poll(harness, None);
    let one = keyed(&changes, 1, "item-1/needs-you-1");
    assert_eq!(one.len(), 1, "one comment per key: {one:?}");
    assert_eq!(normalise(&one[0].body), "Needs you: the verdict fails.");
    assert_eq!(keyed(&changes, 1, "item-1/needs-you-2").len(), 1);
}

/// `upsert_comment` replayed with one key leaves one comment, holding the last body (#51).
///
/// # Panics
///
/// When the backend fails the check.
pub fn upsert_replayed_with_one_key_leaves_one_comment(harness: &mut dyn Harness) {
    harness.user_writes(&[card(1, Column::Ready, "true")]);
    let queue = harness.queue();
    for body in ["Claimed.", "Claimed.", "Attempt 1: running."] {
        queue
            .upsert_comment(ItemId(1), "item-1/sticky", body)
            .expect("upsert");
    }
    let changes = poll(harness, None);
    let sticky = keyed(&changes, 1, "item-1/sticky");
    assert_eq!(sticky.len(), 1, "one sticky comment: {sticky:?}");
    assert_eq!(normalise(&sticky[0].body), "Attempt 1: running.");
}

/// A sticky comment the user deleted is recreated by the next upsert, once (#5).
///
/// # Panics
///
/// When the backend fails the check.
pub fn upsert_recreates_a_deleted_comment(harness: &mut dyn Harness) {
    let plain = card(1, Column::Ready, "true");
    harness.user_writes(std::slice::from_ref(&plain));
    harness
        .queue()
        .upsert_comment(ItemId(1), "item-1/sticky", "Claimed.")
        .expect("upsert");
    harness.user_writes(std::slice::from_ref(&plain));
    for _ in 0..2 {
        harness
            .queue()
            .upsert_comment(ItemId(1), "item-1/sticky", "Attempt 2.")
            .expect("upsert");
    }
    let changes = poll(harness, None);
    let sticky = keyed(&changes, 1, "item-1/sticky");
    assert_eq!(sticky.len(), 1);
    assert_eq!(normalise(&sticky[0].body), "Attempt 2.");
}

/// A sticky comment the user deleted is recreated once; deleted again, it stays deleted, so
/// the daemon and the user never fight over it (#5, #61).
///
/// # Panics
///
/// When the backend fails the check.
pub fn a_sticky_deleted_twice_is_recreated_once(harness: &mut dyn Harness) {
    let plain = card(1, Column::InProgress, "true");
    harness.user_writes(std::slice::from_ref(&plain));
    let upsert = |harness: &dyn Harness, body: &str| {
        harness
            .queue()
            .upsert_comment(ItemId(1), "report:1", body)
            .expect("upsert");
    };
    upsert(harness, "Claimed.");
    harness.user_writes(std::slice::from_ref(&plain));
    upsert(harness, "Attempt 1.");
    let changes = poll(harness, None);
    assert_eq!(keyed(&changes, 1, "report:1").len(), 1, "recreated once");

    harness.user_writes(std::slice::from_ref(&plain));
    for body in ["Attempt 2.", "Verdict passed."] {
        upsert(harness, body);
    }
    let changes = poll(harness, None);
    assert_eq!(
        keyed(&changes, 1, "report:1").len(),
        0,
        "deleted a second time, the sticky comment stays deleted"
    );
}

/// A keyed comment the user deleted is not posted again by a replay of `comment` (#5).
///
/// # Panics
///
/// When the backend fails the check.
pub fn a_deleted_comment_is_not_posted_again(harness: &mut dyn Harness) {
    let plain = card(1, Column::NeedsYou, "true");
    harness.user_writes(std::slice::from_ref(&plain));
    let queue = harness.queue();
    queue
        .comment(ItemId(1), "Needs you: the verdict fails.", "needs-you:1:1")
        .expect("comment");
    harness.user_writes(std::slice::from_ref(&plain));
    harness
        .queue()
        .comment(ItemId(1), "Needs you: the verdict fails.", "needs-you:1:1")
        .expect("comment");
    let changes = poll(harness, None);
    assert_eq!(
        keyed(&changes, 1, "needs-you:1:1").len(),
        0,
        "not posted again"
    );
}

/// A body that already starts with the marker of its key (the sticky report as the renderer of
/// #56 writes it) reads back with the key, and without the marker in its text.
///
/// # Panics
///
/// When the backend fails the check.
pub fn a_body_with_its_marker_keeps_one_marker(harness: &mut dyn Harness) {
    harness.user_writes(&[card(1, Column::InProgress, "true")]);
    let rendered = format!(
        "{}\n### bosun: Card 1\n\nClaimed.",
        bosun_core::publish::marker("report:1")
    );
    let queue = harness.queue();
    queue
        .upsert_comment(ItemId(1), "report:1", &rendered)
        .expect("upsert");
    queue
        .upsert_comment(ItemId(1), "report:1", &rendered)
        .expect("upsert");
    let changes = poll(harness, None);
    let sticky = keyed(&changes, 1, "report:1");
    assert_eq!(sticky.len(), 1);
    assert_eq!(normalise(&sticky[0].body), "### bosun: Card 1\n\nClaimed.");
}

/// `project` moves the card, and writing the same state again changes nothing.
///
/// # Panics
///
/// When the backend fails the check.
pub fn project_is_idempotent(harness: &mut dyn Harness) {
    harness.user_writes(&[
        card(1, Column::Ready, "true"),
        card(2, Column::Ready, "true"),
    ]);
    let first = poll(harness, None);
    let queue = harness.queue();
    queue
        .project(ItemId(2), Owned::InProgress, Some("attempt 1"))
        .expect("project");
    let second = poll(harness, Some(&first.cursor));
    assert_eq!(second.moved, [ItemId(2)]);
    assert_eq!(second.card(ItemId(2)).unwrap().column, Column::InProgress);

    queue
        .project(ItemId(2), Owned::InProgress, Some("attempt 1"))
        .expect("project");
    let third = poll(harness, Some(&second.cursor));
    assert!(third.is_empty(), "a replay changes nothing: {third:?}");

    queue
        .project(ItemId(2), Owned::NeedsYou, None)
        .expect("project");
    let fourth = poll(harness, Some(&third.cursor));
    assert_eq!(fourth.card(ItemId(2)).unwrap().column, Column::NeedsYou);
    assert_eq!(fourth.card(ItemId(1)).unwrap().column, Column::Ready);
}

/// Writes to an item with no card fail with `NotFound`; a key that cannot be carried fails
/// with `InvalidKey`.
///
/// # Panics
///
/// When the backend fails the check.
pub fn unknown_items_and_keys_are_refused(harness: &mut dyn Harness) {
    harness.user_writes(&[card(1, Column::Ready, "true")]);
    let queue = harness.queue();
    let missing = ItemId(404);
    assert!(matches!(
        queue.project(missing, Owned::Done, None),
        Err(QueueError::NotFound(id)) if id == missing
    ));
    assert!(matches!(
        queue.comment(missing, "x", "k"),
        Err(QueueError::NotFound(_))
    ));
    assert!(matches!(
        queue.lint_target(missing),
        Err(QueueError::NotFound(_))
    ));
    assert!(matches!(
        queue.comment(ItemId(1), "x", "a key with spaces"),
        Err(QueueError::InvalidKey(_))
    ));
    assert!(matches!(
        queue.upsert_comment(ItemId(1), "", "x"),
        Err(QueueError::InvalidKey(_))
    ));
}

/// `lint_target` reads the board now, not the last poll.
///
/// # Panics
///
/// When the backend fails the check.
pub fn lint_target_reads_the_live_card(harness: &mut dyn Harness) {
    let mut cards = vec![card(1, Column::Ready, "true")];
    harness.user_writes(&cards);
    poll(harness, None);
    cards[0].body = card(1, Column::Ready, "cargo test").body;
    cards[0].labels = vec!["restart".to_owned()];
    harness.user_writes(&cards);
    let live = harness.queue().lint_target(ItemId(1)).expect("lint_target");
    assert!(live.body.contains("cargo test"), "{}", live.body);
    assert_eq!(live.labels, ["restart"]);
    assert_eq!(live.title, "Card 1");
}
