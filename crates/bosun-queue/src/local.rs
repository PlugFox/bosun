//! The local-file backend (#5, #51): the board is one TOML file the user edits by hand while the
//! daemon runs. For tests, and for repositories with no board.
//!
//! # Format
//!
//! ````toml
//! # Cards in board order: within a column, the card higher in the file is higher on the board.
//!
//! [[card]]
//! id = 1                         # the item's number, unique in the file; `blocked-by` uses it
//! title = "Add the frobnicator"
//! column = "Ready"               # Backlog, Ready, In progress, Verify, Done, Needs you
//! labels = ["restart"]           # optional
//! draft = false                  # optional; a draft fails lint in Ready
//! note = "attempt 2"             # optional; written by the daemon
//! body = '''
//! What to do, for the agent.
//!
//! ```bosun
//! verdict = "cargo test -p frob"
//! ```
//! '''
//!
//! [[card.comment]]               # optional, oldest first
//! author = "me"                  # optional; "local" when left out
//! body = '''
//! A remark the agent should see.
//! '''
//!
//! [[card.comment]]               # written by the daemon
//! author = "bosun"
//! key = "item-1/sticky"          # the idempotence key; do not edit
//! body = '''
//! Claimed; attempt 1.
//! '''
//! ````
//!
//! - **Order** is the order of the `[[card]]` tables: to reorder, move a card's block (its
//!   `[[card]]` header down to the next one, its comments included).
//! - **Columns** are matched ignoring case, with `-`, `_` and a space alike (`in-progress`).
//! - Bodies read best as literal multi-line strings (`'''`), which need no escaping. A body
//!   holding `'''` must use a basic string (`"""`, with `\` escapes) instead.
//! - An unknown key, a missing `id`, `title` or `column`, or two cards with one `id` make the
//!   file invalid: [`Queue::poll`] fails with [`QueueError::Invalid`] naming the card, and the
//!   daemon keeps its last good view until the file is fixed.
//!
//! # Editing while the daemon runs
//!
//! [`Queue::poll`] re-reads the whole file every time: edits, additions, removals and reorders
//! show in the next poll, whatever the file's size or time stamp say. The daemon's own writes
//! (the column and note of [`Queue::project`], the comments) never clobber a user's edit:
//!
//! 1. **Minimal edits.** The daemon changes only the keys it owns, through a format-preserving
//!    TOML editor; the user's comments, spacing, key order and line endings stay as written.
//! 2. **Optimistic concurrency.** It reads the file, applies its edit, writes the result to a
//!    temporary file in the same directory, then re-reads the file: if the bytes changed since
//!    its read (the user saved meanwhile), it discards its result and starts again from the
//!    user's version, up to [`WRITE_ATTEMPTS`] times, then fails with
//!    [`QueueError::Conflict`] for the outbox to retry later.
//! 3. **Atomic replace.** Only then is the temporary file renamed over the board, so a reader
//!    sees the old file or the new one, never a half-written one.
//! 4. **Broken files are left alone.** A file that does not parse (the user saved mid-edit) is
//!    never written; the write fails with [`QueueError::Invalid`] and is retried later.
//!
//! What remains is the window between the final re-read and the rename (microseconds), and an
//! editor buffer holding unsaved changes: when the user later saves it, that save wins over the
//! daemon's write, as any save would, and the daemon's next write re-applies its state. Editors
//! that watch the file (VS Code, `JetBrains` IDEs, Vim's `autoread`) reload or warn first.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use crate::card::{Card, Column, Comment, ItemId, Owned};
use crate::marker::{self, valid_key};
use crate::queue::{Changes, Cursor, Keyed, Queue, QueueError};

/// How many times a write starts over when the user saved the file under it.
pub const WRITE_ATTEMPTS: usize = 5;

/// The author of the daemon's comments in the file.
pub const DAEMON_AUTHOR: &str = "bosun";

/// The author of a comment that names none.
pub const DEFAULT_AUTHOR: &str = "local";

const CARD_KEYS: &[&str] = &[
    "id", "title", "column", "labels", "draft", "note", "body", "comment",
];
const COMMENT_KEYS: &[&str] = &["author", "key", "body"];

/// A board kept in one TOML file; see the module docs for its format.
#[derive(Debug)]
pub struct LocalFile {
    path: PathBuf,
    /// Serialises this process's writes; the user's are handled by the re-read.
    write: Mutex<()>,
    /// The keyed comments this backend posted or found, for the deletion rules of #5.
    keyed: Keyed,
    /// Mutant (#51): the file's length at the last poll and the cards read then.
    #[cfg(mutant_queue_stale_poll)]
    cache: Mutex<Option<(u64, Vec<Card>)>>,
}

impl LocalFile {
    /// The board in the file at `path`. Nothing is read until the first call.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write: Mutex::new(()),
            keyed: Keyed::default(),
            #[cfg(mutant_queue_stale_poll)]
            cache: Mutex::new(None),
        }
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Puts `label` on the card of `item`, as the user would: `bosun restart-item` puts the
    /// `restart` label (#5, #70), which the loop acts on and removes. A label the card already
    /// carries is left as it is, so a replay writes nothing.
    ///
    /// # Errors
    ///
    /// [`QueueError::NotFound`] when the item has no card; otherwise the write failed.
    pub fn add_label(&self, item: ItemId, label: &str) -> Result<(), QueueError> {
        self.edit(item, |card| {
            if !card.contains_key("labels") {
                card.insert("labels", Item::Value(Value::Array(toml_edit::Array::new())));
            }
            let Some(labels) = card.get_mut("labels").and_then(Item::as_array_mut) else {
                return false;
            };
            if labels.iter().any(|l| l.as_str() == Some(label)) {
                return false;
            }
            labels.push(label);
            true
        })
    }

    /// A whole file holding `cards` in this order, comments included: how a new board is
    /// written, and how tests stand in for the user.
    #[must_use]
    pub fn render(cards: &[Card]) -> String {
        let mut text = String::new();
        for (i, card) in cards.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            let _ = writeln!(
                text,
                "[[card]]
id = {}
title = {}
column = {}",
                card.id.0,
                repr(&card.title),
                repr(card.column.name())
            );
            if !card.labels.is_empty() {
                let labels: Vec<String> = card.labels.iter().map(|l| repr(l)).collect();
                let _ = writeln!(text, "labels = [{}]", labels.join(", "));
            }
            if card.draft {
                text.push_str(
                    "draft = true
",
                );
            }
            let _ = writeln!(text, "body = {}", repr(&card.body));
            for comment in &card.comments {
                let _ = writeln!(
                    text,
                    "
[[card.comment]]
author = {}",
                    repr(&comment.author)
                );
                if let Some(key) = &comment.key {
                    let _ = writeln!(text, "key = {}", repr(key));
                }
                let _ = writeln!(text, "body = {}", repr(&comment.body));
            }
        }
        text
    }

    fn read_text(&self) -> Result<String, QueueError> {
        let bytes = fs::read(&self.path)?;
        String::from_utf8(bytes)
            .map_err(|_| QueueError::Invalid(format!("{} is not UTF-8", self.path.display())))
    }

    fn parse(&self, text: &str) -> Result<DocumentMut, QueueError> {
        text.parse::<DocumentMut>().map_err(|e| {
            QueueError::Invalid(format!(
                "{} is not TOML: {}",
                self.path.display(),
                e.message().trim()
            ))
        })
    }

    fn cards(&self) -> Result<Vec<Card>, QueueError> {
        let text = self.read_text()?;
        let document = self.parse(&text)?;
        read_cards(&document)
            .map_err(|why| QueueError::Invalid(format!("{}: {why}", self.path.display())))
    }

    /// Applies `edit` to the card of `item`, as described in the module docs. `edit` returns
    /// whether it changed anything; nothing is written when it did not.
    fn edit(
        &self,
        item: ItemId,
        mut edit: impl FnMut(&mut Table) -> bool,
    ) -> Result<(), QueueError> {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for _ in 0..WRITE_ATTEMPTS {
            let before = fs::read(&self.path)?;
            let text = String::from_utf8(before.clone()).map_err(|_| {
                QueueError::Invalid(format!("{} is not UTF-8", self.path.display()))
            })?;
            let mut document = self.parse(&text)?;
            read_cards(&document)
                .map_err(|why| QueueError::Invalid(format!("{}: {why}", self.path.display())))?;
            let table = card_tables_mut(&mut document)
                .find(|t| card_id(t) == Some(item))
                .ok_or(QueueError::NotFound(item))?;
            if !edit(table) {
                return Ok(());
            }
            let mut out = document.to_string();
            if text.contains("\r\n") {
                out = out.replace("\r\n", "\n").replace('\n', "\r\n");
            }
            let temp = self.temp_path();
            write_synced(&temp, out.as_bytes())?;
            if fs::read(&self.path)? != before {
                // The user saved meanwhile: start again from their version.
                let _ = fs::remove_file(&temp);
                continue;
            }
            if let Err(e) = replace(&temp, &self.path) {
                let _ = fs::remove_file(&temp);
                return Err(e.into());
            }
            return Ok(());
        }
        Err(QueueError::Conflict(format!(
            "{} changed during each of {WRITE_ATTEMPTS} attempts to write it",
            self.path.display()
        )))
    }

    fn temp_path(&self) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map_or_else(|| "queue".into(), |n| n.to_string_lossy().into_owned());
        self.path
            .with_file_name(format!(".{name}.bosun-{}.tmp", std::process::id()))
    }

    fn comment_write(
        &self,
        item: ItemId,
        key: &str,
        body: &str,
        replace_existing: bool,
    ) -> Result<(), QueueError> {
        if !valid_key(key) {
            return Err(QueueError::InvalidKey(key.to_owned()));
        }
        // The key lives in its own field: a body rendered with its marker is stored without.
        let body = marker::without(key, body);
        // What the last attempt of the edit did: found the comment, or created it.
        let mut outcome = None;
        self.edit(item, |card| {
            if let Some(existing) = comment_by_key(card, key) {
                outcome = Some(None);
                let same = existing
                    .get("body")
                    .and_then(Item::as_str)
                    .is_some_and(|old| old.replace("\r\n", "\n") == body.replace("\r\n", "\n"));
                if !replace_existing || same {
                    return false;
                }
                set_str(existing, "body", body);
                return true;
            }
            let create = if cfg!(mutant_queue_local_no_key) {
                Some(false)
            } else {
                self.keyed.create_missing(item, key, replace_existing)
            };
            let Some(recreated) = create else {
                // Deleted by the user, and not to be posted again.
                outcome = None;
                return false;
            };
            outcome = Some(Some(recreated));
            let mut comment = Table::new();
            comment.decor_mut().set_prefix("\n");
            comment.insert("author", string_item(DAEMON_AUTHOR));
            comment.insert("key", string_item(key));
            comment.insert("body", string_item(body));
            let comments = card
                .entry("comment")
                .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
            if let Some(array) = comments.as_array_of_tables_mut() {
                array.push(comment);
            }
            true
        })?;
        match outcome {
            Some(None) => self.keyed.seen(item, key),
            Some(Some(recreated)) => self.keyed.created(item, key, recreated),
            None => {}
        }
        Ok(())
    }
}

impl Queue for LocalFile {
    fn poll(&self, cursor: Option<&Cursor>) -> Result<Changes, QueueError> {
        #[cfg(mutant_queue_stale_poll)]
        {
            // Mutant (#51): an unchanged length is taken for an unchanged file, and a reorder
            // keeps the length.
            let len = fs::metadata(&self.path)?.len();
            let mut cache = self.cache.lock().unwrap();
            if let Some((cached_len, cards)) = cache.as_ref()
                && *cached_len == len
            {
                return Ok(Changes::since(cursor, cards.clone()));
            }
            let cards = self.cards()?;
            *cache = Some((len, cards.clone()));
            return Ok(Changes::since(cursor, cards));
        }
        #[cfg(not(mutant_queue_stale_poll))]
        Ok(Changes::since(cursor, self.cards()?))
    }

    fn project(&self, item: ItemId, state: Owned, note: Option<&str>) -> Result<(), QueueError> {
        self.edit(item, |card| {
            let mut changed = false;
            let column = state.column();
            let current = card
                .get("column")
                .and_then(Item::as_str)
                .and_then(Column::from_name);
            if current != Some(column) {
                set_str(card, "column", column.name());
                changed = true;
            }
            let current_note = card.get("note").and_then(Item::as_str);
            match note {
                Some(note) if current_note != Some(note) => {
                    set_str(card, "note", note);
                    changed = true;
                }
                None if current_note.is_some() => {
                    card.remove("note");
                    changed = true;
                }
                _ => {}
            }
            changed
        })
    }

    fn comment(&self, item: ItemId, body: &str, key: &str) -> Result<(), QueueError> {
        self.comment_write(item, key, body, false)
    }

    fn upsert_comment(&self, item: ItemId, key: &str, body: &str) -> Result<(), QueueError> {
        self.comment_write(item, key, body, true)
    }

    fn lint_target(&self, item: ItemId) -> Result<Card, QueueError> {
        self.cards()?
            .into_iter()
            .find(|c| c.id == item)
            .ok_or(QueueError::NotFound(item))
    }
}

/// The comment table with `key` on a card.
fn comment_by_key<'t>(card: &'t mut Table, key: &str) -> Option<&'t mut Table> {
    if cfg!(mutant_queue_local_no_key) {
        // Mutant (#51): no lookup by key, so every replay posts again.
        return None;
    }
    card.get_mut("comment")?
        .as_array_of_tables_mut()?
        .iter_mut()
        .find(|c| c.get("key").and_then(Item::as_str) == Some(key))
}

fn card_tables_mut(document: &mut DocumentMut) -> impl Iterator<Item = &mut Table> {
    document
        .get_mut("card")
        .and_then(Item::as_array_of_tables_mut)
        .into_iter()
        .flat_map(ArrayOfTables::iter_mut)
}

fn card_id(table: &Table) -> Option<ItemId> {
    let id = table.get("id")?.as_integer()?;
    u64::try_from(id).ok().filter(|id| *id > 0).map(ItemId)
}

/// The cards of a parsed file, in file order, or what makes the file invalid.
fn read_cards(document: &DocumentMut) -> Result<Vec<Card>, String> {
    let root = document.as_table();
    for (key, _) in root {
        if key != "card" {
            return Err(format!(
                "unknown top-level key `{key}`; cards are [[card]] tables"
            ));
        }
    }
    let Some(item) = root.get("card") else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or("`card` must be [[card]] tables")?;
    let mut cards: Vec<Card> = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let card =
            read_card(table).map_err(|why| format!("card {} of the file: {why}", index + 1))?;
        if cards.iter().any(|c| c.id == card.id) {
            return Err(format!("two cards have id {}", card.id.0));
        }
        cards.push(card);
    }
    Ok(cards)
}

fn read_card(table: &Table) -> Result<Card, String> {
    unknown_keys(table, CARD_KEYS)?;
    let id = card_id(table).ok_or("`id` must be a positive integer")?;
    let at = |why: String| format!("(id {}) {why}", id.0);
    let title = required_str(table, "title").map_err(at)?;
    let column_name = required_str(table, "column").map_err(at)?;
    let column = Column::from_name(&column_name).ok_or_else(|| {
        at(format!(
            "`column` is `{column_name}`; the columns are Backlog, Ready, In progress, Verify, \
             Done and Needs you"
        ))
    })?;
    let body = optional_str(table, "body").map_err(at)?.unwrap_or_default();
    optional_str(table, "note").map_err(at)?;
    let labels = match table.get("labels") {
        None => Vec::new(),
        Some(item) => item
            .as_array()
            .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_owned)).collect())
            .ok_or_else(|| at("`labels` must be an array of strings".into()))?,
    };
    let draft = match table.get("draft") {
        None => false,
        Some(item) => item
            .as_bool()
            .ok_or_else(|| at("`draft` must be true or false".into()))?,
    };
    let comments = match table.get("comment") {
        None => Vec::new(),
        Some(item) => item
            .as_array_of_tables()
            .ok_or_else(|| at("comments must be [[card.comment]] tables".into()))?
            .iter()
            .enumerate()
            .map(|(i, c)| read_comment(c).map_err(|why| at(format!("comment {}: {why}", i + 1))))
            .collect::<Result<_, _>>()?,
    };
    Ok(Card {
        id,
        title,
        column,
        body,
        labels,
        draft,
        comments,
        blocked_by: Vec::new(),
    })
}

fn read_comment(table: &Table) -> Result<Comment, String> {
    unknown_keys(table, COMMENT_KEYS)?;
    Ok(Comment {
        author: optional_str(table, "author")?.unwrap_or_else(|| DEFAULT_AUTHOR.to_owned()),
        key: optional_str(table, "key")?,
        body: required_str(table, "body")?,
    })
}

fn unknown_keys(table: &dyn TableLike, known: &[&str]) -> Result<(), String> {
    match table.iter().find(|(key, _)| !known.contains(key)) {
        Some((key, _)) => Err(format!(
            "unknown key `{key}`; the keys are {}",
            known.join(", ")
        )),
        None => Ok(()),
    }
}

fn optional_str(table: &Table, key: &str) -> Result<Option<String>, String> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`{key}` must be a string"))
        })
        .transpose()
}

fn required_str(table: &Table, key: &str) -> Result<String, String> {
    optional_str(table, key)?.ok_or_else(|| format!("`{key}` is missing"))
}

/// `text` as a TOML string: a literal multi-line string when it spans lines and can be one,
/// otherwise the editor's own choice.
fn repr(text: &str) -> String {
    let literal_ok = !text.contains("'''")
        && text
            .chars()
            .all(|c| c == '\n' || c == '\t' || !c.is_control());
    if text.contains('\n') && literal_ok {
        format!("'''\n{text}'''")
    } else {
        Value::from(text).to_string()
    }
}

fn string_item(text: &str) -> Item {
    let snippet: DocumentMut = format!("v = {}", repr(text))
        .parse()
        .expect("repr writes valid TOML");
    let mut item = snippet
        .as_table()
        .get("v")
        .cloned()
        .expect("the snippet has v");
    if let Some(value) = item.as_value_mut() {
        value.decor_mut().set_prefix(" ");
    }
    item
}

/// Sets `key` to `text`, keeping the decoration of the value it replaces: a trailing comment
/// the user wrote after it stays.
fn set_str(table: &mut Table, key: &str, text: &str) {
    let mut item = string_item(text);
    if let (Some(old), Some(new)) = (table.get(key).and_then(Item::as_value), item.as_value_mut()) {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(key, item);
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Renames `from` over `to`. On Windows a reader holding the file open without delete sharing
/// (a scanner, an editor reading it) makes the rename fail for a moment; it is retried briefly.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testsuite::{self, Harness};

    struct LocalHarness {
        _dir: tempfile::TempDir,
        queue: LocalFile,
    }

    impl LocalHarness {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("queue.toml");
            fs::write(&path, "").unwrap();
            Self {
                queue: LocalFile::new(path),
                _dir: dir,
            }
        }
    }

    impl Harness for LocalHarness {
        fn queue(&self) -> &dyn Queue {
            &self.queue
        }

        fn user_writes(&mut self, cards: &[Card]) {
            fs::write(self.queue.path(), LocalFile::render(cards)).unwrap();
        }
    }

    // The backend-agnostic suite, one test per check so a mutant names the one that goes red.

    #[test]
    fn suite_edit_addition_and_reorder_appear_in_the_next_poll() {
        testsuite::edit_addition_and_reorder_appear_in_the_next_poll(&mut LocalHarness::new());
    }

    #[test]
    fn suite_a_reorder_alone_appears_in_the_next_poll() {
        testsuite::a_reorder_alone_appears_in_the_next_poll(&mut LocalHarness::new());
    }

    #[test]
    fn suite_comment_replayed_with_one_key_leaves_one_comment() {
        testsuite::comment_replayed_with_one_key_leaves_one_comment(&mut LocalHarness::new());
    }

    #[test]
    fn suite_upsert_replayed_with_one_key_leaves_one_comment() {
        testsuite::upsert_replayed_with_one_key_leaves_one_comment(&mut LocalHarness::new());
    }

    #[test]
    fn suite_upsert_recreates_a_deleted_comment() {
        testsuite::upsert_recreates_a_deleted_comment(&mut LocalHarness::new());
    }

    #[test]
    fn suite_a_sticky_deleted_twice_is_recreated_once() {
        testsuite::a_sticky_deleted_twice_is_recreated_once(&mut LocalHarness::new());
    }

    #[test]
    fn suite_a_deleted_comment_is_not_posted_again() {
        testsuite::a_deleted_comment_is_not_posted_again(&mut LocalHarness::new());
    }

    #[test]
    fn suite_a_body_with_its_marker_keeps_one_marker() {
        testsuite::a_body_with_its_marker_keeps_one_marker(&mut LocalHarness::new());
    }

    #[test]
    fn suite_project_is_idempotent() {
        testsuite::project_is_idempotent(&mut LocalHarness::new());
    }

    #[test]
    fn suite_unknown_items_and_keys_are_refused() {
        testsuite::unknown_items_and_keys_are_refused(&mut LocalHarness::new());
    }

    #[test]
    fn suite_lint_target_reads_the_live_card() {
        testsuite::lint_target_reads_the_live_card(&mut LocalHarness::new());
    }

    #[test]
    fn suite_runs_whole() {
        testsuite::run(LocalHarness::new);
    }

    const FILE: &str = "# my board\n\n[[card]]\nid = 1\ntitle = \"One\"\ncolumn = \"ready\"  # mine\n\
                        body = '''\nDo one.\n\n```bosun\nverdict = \"true\"\n```\n'''\n\n\
                        [[card.comment]]\nauthor = \"me\"\nbody = \"first\"\n\n\
                        [[card]]\nid = 2\ntitle = \"Two\"\ncolumn = \"Backlog\"\nbody = \"\"\n";

    fn board(text: &str) -> (tempfile::TempDir, LocalFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.toml");
        fs::write(&path, text).unwrap();
        (dir, LocalFile::new(path))
    }

    #[test]
    fn a_daemon_write_keeps_the_users_formatting() {
        let (_dir, queue) = board(FILE);
        queue
            .comment(ItemId(1), "Claimed.\nAttempt 1.\n", "item-1/sticky")
            .unwrap();
        queue
            .project(ItemId(1), Owned::InProgress, Some("attempt 1"))
            .unwrap();
        let text = fs::read_to_string(queue.path()).unwrap();
        assert!(text.starts_with("# my board\n"), "{text}");
        assert!(
            text.contains("column = \"In progress\"  # mine\n"),
            "{text}"
        );
        // The new comment follows the card's own comment, before the next card.
        let mine = text.find("body = \"first\"").unwrap();
        let ours = text.find("key = \"item-1/sticky\"").unwrap();
        let next = text.find("id = 2").unwrap();
        assert!(mine < ours && ours < next, "{text}");
        assert!(
            text.contains("body = '''\nClaimed.\nAttempt 1.\n'''"),
            "{text}"
        );

        let card = queue.lint_target(ItemId(1)).unwrap();
        assert_eq!(card.column, Column::InProgress);
        assert_eq!(card.comments.len(), 2);
        assert_eq!(card.comments[1].author, DAEMON_AUTHOR);
        assert_eq!(card.comments[1].body, "Claimed.\nAttempt 1.\n");
        let leftovers: Vec<_> = fs::read_dir(queue.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no temporary file is left: {leftovers:?}"
        );
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let (_dir, queue) = board(&FILE.replace('\n', "\r\n"));
        queue.upsert_comment(ItemId(2), "k", "a\nb").unwrap();
        let text = fs::read_to_string(queue.path()).unwrap();
        assert!(!text.replace("\r\n", "").contains('\n'), "{text:?}");
        let card = queue.lint_target(ItemId(2)).unwrap();
        assert_eq!(
            card.comment_by_key("k").unwrap().body.replace('\r', ""),
            "a\nb"
        );
    }

    #[test]
    fn whitespace_of_the_file_does_not_change_the_snapshot() {
        let (_dir, queue) = board(FILE);
        let before = Snapshot::take(&queue.lint_target(ItemId(1)).unwrap(), |_| true).unwrap();
        let spaced = FILE
            .replace("id = 1\n", "\n\n   id   =   1\n")
            .replace("body = '''", "body =    '''")
            .replace("\n\n[[card]]", "\n\n\n\n[[card]]");
        fs::write(queue.path(), &spaced).unwrap();
        let after = Snapshot::take(&queue.lint_target(ItemId(1)).unwrap(), |_| true).unwrap();
        assert_eq!(before.hash, after.hash);

        fs::write(queue.path(), FILE.replace("Do one.", "Do one, then two.")).unwrap();
        let edited = Snapshot::take(&queue.lint_target(ItemId(1)).unwrap(), |_| true).unwrap();
        assert_ne!(before.hash, edited.hash);
    }

    #[test]
    fn an_invalid_file_is_reported_and_never_written() {
        for (text, needle) in [
            ("[[card]]\nid = 1\ntitle = \"x\"\n", "`column` is missing"),
            (
                "[[card]]\nid = 1\ntitle = \"x\"\ncolumn = \"Waiting\"\n",
                "Waiting",
            ),
            (
                "[[card]]\nid = 1\ntitle = \"x\"\ncolumn = \"Ready\"\ncolour = 1\n",
                "colour",
            ),
            (
                "[[card]]\nid = 1\ntitle = \"x\"\ncolumn = \"Ready\"\n[[card]]\nid = 1\ntitle = \"y\"\ncolumn = \"Ready\"\n",
                "two cards have id 1",
            ),
            ("[[card]]\nid = 1\ntitle = \"x\n", "not TOML"),
            ("cards = 1\n", "unknown top-level key"),
        ] {
            let (_dir, queue) = board(text);
            let error = queue.poll(None).unwrap_err();
            assert!(matches!(error, QueueError::Invalid(_)), "{error}");
            assert!(error.to_string().contains(needle), "{error} lacks {needle}");
            let write = queue.project(ItemId(1), Owned::Done, None).unwrap_err();
            assert!(matches!(write, QueueError::Invalid(_)), "{write}");
            assert_eq!(fs::read_to_string(queue.path()).unwrap(), text);
        }
    }

    #[test]
    fn a_save_during_a_write_is_not_clobbered() {
        // The user's save lands between the daemon's read and its rename: the first attempt's
        // re-read sees it and the write starts over from the user's version.
        let (_dir, queue) = board(FILE);
        let mut saved = false;
        queue
            .edit(ItemId(1), |card| {
                if !saved {
                    saved = true;
                    fs::write(queue.path(), FILE.replace("Do one.", "Do one (edited).")).unwrap();
                }
                card.insert("note", string_item("ours"));
                true
            })
            .unwrap();
        let card = queue.lint_target(ItemId(1)).unwrap();
        assert!(card.body.contains("Do one (edited)."), "{}", card.body);
        let text = fs::read_to_string(queue.path()).unwrap();
        assert!(text.contains("note = \"ours\""), "{text}");
    }

    #[test]
    fn a_file_that_never_settles_is_a_conflict() {
        let (_dir, queue) = board(FILE);
        let mut n = 0;
        let error = queue
            .edit(ItemId(1), |card| {
                n += 1;
                fs::write(queue.path(), format!("{FILE}# save {n}\n")).unwrap();
                card.insert("note", string_item("ours"));
                true
            })
            .unwrap_err();
        assert!(matches!(error, QueueError::Conflict(_)), "{error}");
        assert!(error.is_transient());
        assert!(!fs::read_to_string(queue.path()).unwrap().contains("ours"));
    }

    #[test]
    fn a_label_is_added_once_and_the_rest_is_kept() {
        let (_dir, queue) = board(FILE);
        queue.add_label(ItemId(1), "restart").unwrap();
        let once = fs::read_to_string(queue.path()).unwrap();
        assert!(once.contains("column = \"ready\"  # mine"), "{once}");
        assert_eq!(queue.lint_target(ItemId(1)).unwrap().labels, ["restart"]);
        queue.add_label(ItemId(1), "restart").unwrap();
        assert_eq!(
            fs::read_to_string(queue.path()).unwrap(),
            once,
            "a replay writes nothing"
        );
        assert!(matches!(
            queue.add_label(ItemId(9), "restart"),
            Err(QueueError::NotFound(_))
        ));
    }

    #[test]
    fn render_round_trips_awkward_text() {
        let card = Card {
            id: ItemId(9),
            title: "Quotes \" and 'single'".into(),
            column: Column::NeedsYou,
            body: "has ''' inside\nand \\ backslash\n".into(),
            labels: vec!["restart".into(), "a b".into()],
            draft: true,
            comments: vec![Comment {
                author: "me".into(),
                key: Some("k".into()),
                body: "line\r\nwith cr".into(),
            }],
            blocked_by: Vec::new(),
        };
        let (_dir, queue) = board(&LocalFile::render(std::slice::from_ref(&card)));
        assert_eq!(queue.lint_target(ItemId(9)).unwrap(), card);
    }
}
