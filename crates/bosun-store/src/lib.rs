//! The bosun database (#3, #19): sqlx over SQLite, one file per identity.
//!
//! - [`Store::open`] refuses a database newer than this binary without touching it, copies the
//!   file before a migration, applies the embedded forward-only migrations, seeds the price
//!   table from [`bosun_core::default_table`] and keeps the files readable by the owning user
//!   only.
//! - One writer task owns the only write connection. Everything else sends it batches through
//!   a [`Writer`]; each batch is one transaction, so a crash loses at most the uncommitted batch
//!   and never half of one. Messages are folded with [`bosun_core::merge`] read-modify-write:
//!   SQL holds no merge logic.
//! - The loop's tables (#7, #57): items, snapshots, attempts, run sessions, verdict runs, the
//!   transition log and the outbox. A transition is written under its idempotence key in one
//!   transaction with the item it produced and its board writes, so a replay is a no-op.
//! - Readers use a read-only pool ([`Reader`]). Every derived number (cost, eqv) is computed at
//!   query time from raw token counts and the price rows.
//!
//! # Durability
//!
//! WAL with `synchronous=NORMAL`: a commit is atomic and survives a crash of the process; a
//! power loss may lose the last commits, never corrupt the file. The writer checkpoints the WAL
//! on a timer ([`Options::checkpoint_interval`]) and truncates it on [`Store::close`].
//!
//! # File permissions
//!
//! On Unix the database file, its WAL and shared-memory files and the pre-migration copies are
//! mode 0600, and a directory the store creates is 0700. On Windows the store sets nothing: a
//! new file inherits the ACL of its directory. Under the user's profile (`%LOCALAPPDATA%`, where
//! the daemon keeps its data) that ACL grants full control to the user, `SYSTEM` and the
//! `Administrators` group and nothing to other users. A database placed in a directory with a
//! wider ACL gets that ACL.

mod activity;
mod error;
mod link;
mod open;
mod read;
mod report;
mod work;
mod writer;

pub use activity::RecentSession;
pub use error::Error;
pub use link::{CommitLink, Link, LinkedSession};
pub use read::{Bill, By, Group, Reader};
pub use report::{CheckRow, ToolRow, UsageRow, VersionHealth, WindowSums};
pub use work::{
    AgentCall, AgentCallKind, AgentCallRow, AttemptRow, ItemHistory, NewItem, OutboxOp, OutboxRow,
    RecordedChild, RunCommitRow, RunCommits, RunEnd, RunRow, RunStart, Snapshot, StoredItem,
    TransitionRow, VerdictEnd, VerdictRow, VerdictStart,
};
pub use writer::{PriceChange, Repo, SourceFile, Write, Writer};

use std::path::Path;
use std::time::Duration;

use tokio::task::JoinHandle;

/// Settings of an open store.
#[derive(Debug, Clone)]
pub struct Options {
    /// How often the writer checkpoints the WAL into the database file.
    pub checkpoint_interval: Duration,
    /// Connections of the read-only pool.
    pub read_connections: u32,
    /// How long a connection waits for a lock before it reports the database busy.
    pub busy_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            checkpoint_interval: Duration::from_secs(300),
            read_connections: 4,
            busy_timeout: Duration::from_secs(5),
        }
    }
}

/// An open database: the writer task and the read-only pool.
#[derive(Debug)]
pub struct Store {
    writer: Writer,
    reader: Reader,
    task: JoinHandle<()>,
}

impl Store {
    /// Opens the database at `path`, creating it when it does not exist, and migrates it.
    ///
    /// Must be called inside a tokio runtime: the writer task is spawned on it.
    ///
    /// # Errors
    ///
    /// [`Error::NewerDatabase`] when the file was written by a newer bosun (it is left as it
    /// was); I/O, SQLite and migration errors otherwise.
    pub async fn open(path: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        open::open(path.as_ref(), &options, &open::MIGRATOR).await
    }

    /// A handle that sends batches to the writer task. Handles are cheap to clone.
    #[must_use]
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// The read-only pool.
    #[must_use]
    pub fn reader(&self) -> &Reader {
        &self.reader
    }

    /// Closes the readers, then stops the writer, which checkpoints and truncates the WAL.
    ///
    /// Batches sent through other [`Writer`] handles after this fail with [`Error::Closed`].
    ///
    /// # Errors
    ///
    /// The final checkpoint failed, or the writer task had already died.
    pub async fn close(self) -> Result<(), Error> {
        self.reader.close().await;
        let result = self.writer.shutdown().await;
        self.task.await.map_err(|_| Error::Closed)?;
        result
    }
}
