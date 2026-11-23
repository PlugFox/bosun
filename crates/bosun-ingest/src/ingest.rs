//! Discovery, tailing, rescan and reingest of one identity's transcripts (#3, #21).
//!
//! - **Discovery** walks the provider's directory (for Claude Code, `<config dir>/projects`)
//!   and keys every transcript by its file id, so a rename keeps the file and its offset.
//! - **Tailing**: each file is read from its stored offset. The stored hash of the last parsed
//!   line tells a grown file from a rewritten one; a file that shrank, or whose line before the
//!   offset no longer matches, is re-read from zero, which message dedup makes safe. A last
//!   line without its newline is left for the next read.
//! - **Writes**: the rows of one read of one file and that file's new offset are one batch,
//!   one transaction of the writer task, so a crash never records an offset past rows that
//!   were not written.
//! - **Rescan**: every file is stat'ed and compared with its stored offset, at start and then
//!   on a timer ([`spawn`]). It is the only discovery mechanism until watching (#22), and it
//!   covers sleep and clock steps without detecting them: the next tick simply finds the
//!   bytes.
//! - **Reingest** re-reads every file that still exists from zero with the current parser and
//!   upserts its rows. It never deletes: rows of a deleted file keep their parser version.
//! - A file that vanished keeps its rows; nothing is ever deleted by ingest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bosun_core::{ContextWindow, WindowKey, WindowKind, WindowMeta};
use bosun_store::{Repo, SourceFile, Store, Write, Writer};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::layout::Provider;
use crate::record::{Record, Source};
use crate::tail::{self, Chunk, Found};

/// Settings of an [`Ingest`].
#[derive(Debug, Clone)]
pub struct Options {
    /// How often [`spawn`] rescans every file.
    pub rescan_interval: Duration,
    /// The most bytes one read takes, and so the most one batch holds. A line longer than
    /// this is read whole, by doubling the read until the line fits.
    pub read_limit: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            rescan_interval: Duration::from_secs(45),
            read_limit: 8 << 20,
        }
    }
}

/// What can go wrong in ingest.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The store refused a batch, or is closed.
    Store(bosun_store::Error),
    /// A blocking task of ingest panicked or was cancelled.
    Task(String),
    /// The ingest task has stopped.
    Stopped,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Task(e) => write!(f, "ingest task: {e}"),
            Self::Stopped => f.write_str("the ingest task has stopped"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<bosun_store::Error> for Error {
    fn from(e: bosun_store::Error) -> Self {
        Self::Store(e)
    }
}

/// What one pass over the files did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Transcript files found.
    pub files: u64,
    /// Bytes of complete lines parsed in this pass.
    pub parsed_bytes: u64,
    /// Files re-read from zero: they shrank or were rewritten (or, in a reingest, all).
    pub restarted: u64,
    /// Files that could not be read, with why; the pass went on without them.
    pub failed: Vec<(PathBuf, String)>,
}

/// The ingest of one provider's transcripts into one identity's store.
#[derive(Debug, Clone)]
pub struct Ingest<P> {
    provider: P,
    root: PathBuf,
    store: Arc<StoreHandles>,
    options: Options,
    parsed: Arc<AtomicU64>,
}

#[derive(Debug)]
struct StoreHandles {
    writer: Writer,
    reader: bosun_store::Reader,
}

/// Where a read of a file starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Nothing new: the stored offset is the size.
    Unchanged,
    /// The file grew: from the stored offset.
    Resume(u64),
    /// New, shrunk or rewritten: from zero.
    Restart,
}

impl<P: Provider> Ingest<P> {
    /// An ingest of the transcripts under `root` (for Claude Code, `<config dir>/projects`)
    /// into `store`. Nothing is read until [`Ingest::scan`].
    #[must_use]
    pub fn new(provider: P, root: PathBuf, store: &Store, options: Options) -> Self {
        Self {
            provider,
            root,
            store: Arc::new(StoreHandles {
                writer: store.writer().clone(),
                reader: store.reader().clone(),
            }),
            options,
            parsed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The directory this ingest discovers files in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Bytes of complete lines parsed since this ingest was created, over every pass.
    #[must_use]
    pub fn parsed_bytes(&self) -> u64 {
        self.parsed.load(Ordering::Relaxed)
    }

    /// Registers the configured repository roots messages are attributed to.
    ///
    /// # Errors
    ///
    /// The store refused the batch.
    pub async fn register_repos(&self, roots: &[String]) -> Result<(), Error> {
        let batch = roots
            .iter()
            .map(|path| {
                Write::Repo(Repo {
                    path: path.clone(),
                    remote: None,
                    default_branch: None,
                })
            })
            .collect();
        Ok(self.store.writer.write(batch).await?)
    }

    /// One pass: every file is compared with its stored offset, and the bytes past it are
    /// parsed and written. A file that cannot be read is reported and skipped.
    ///
    /// # Errors
    ///
    /// The store failed; files already written stay written.
    pub async fn scan(&self) -> Result<Report, Error> {
        self.pass(false).await
    }

    /// Re-reads every file that still exists from zero with the current parser and upserts
    /// its rows. Nothing is deleted: rows of a deleted file keep their parser version.
    ///
    /// # Errors
    ///
    /// As [`Ingest::scan`].
    pub async fn reingest(&self) -> Result<Report, Error> {
        let prefix = truncate();
        if !prefix.is_empty() {
            self.store.writer.write(prefix).await?;
        }
        self.pass(true).await
    }

    async fn pass(&self, reingest: bool) -> Result<Report, Error> {
        let provider = self.provider.clone();
        let root = self.root.clone();
        let (found, failed) = blocking(move || tail::discover(&provider, &root)).await?;
        let stored: BTreeMap<String, SourceFile> = self
            .store
            .reader
            .source_files(self.provider.provider())
            .await?
            .into_iter()
            .map(|file| (file.file_key.clone(), file))
            .collect();

        let mut report = Report {
            files: found.len() as u64,
            failed: failed
                .into_iter()
                .map(|(path, e)| (path, e.to_string()))
                .collect(),
            ..Report::default()
        };
        for file in found {
            let known = stored.get(&file.key);
            match self.file(&file, known, reingest, &mut report).await {
                Ok(()) => {}
                Err(FileError::Io(e)) => report.failed.push((file.path, e)),
                Err(FileError::Fatal(e)) => return Err(e),
            }
        }
        Ok(report)
    }

    /// Where the read of `file` starts, given what is stored of it.
    async fn start(&self, file: &Found, known: Option<&SourceFile>) -> Result<Start, FileError> {
        let Some(known) = known else {
            return Ok(Start::Restart);
        };
        let offset = if cfg!(mutant_ingest_no_offset) {
            // Mutant: the stored offset is ignored.
            0
        } else {
            known.parsed_offset
        };
        if file.size < offset {
            return Ok(Start::Restart);
        }
        if offset == 0 {
            return Ok(Start::Restart);
        }
        if file.size == offset {
            return Ok(Start::Unchanged);
        }
        let Some(hash) = known.last_line_hash.clone() else {
            return Ok(Start::Restart);
        };
        let path = file.path.clone();
        let same = blocking(move || tail::hash_matches(&path, offset, &hash)).await?;
        Ok(if same? {
            Start::Resume(offset)
        } else {
            Start::Restart
        })
    }

    async fn file(
        &self,
        file: &Found,
        known: Option<&SourceFile>,
        reingest: bool,
        report: &mut Report,
    ) -> Result<(), FileError> {
        let start = if reingest {
            Start::Restart
        } else {
            self.start(file, known).await?
        };
        let renamed = known.is_some_and(|known| known.path != display(&file.path));
        let mut offset = match start {
            Start::Unchanged => {
                if renamed {
                    // Only the path changed: record it, parse nothing.
                    let known = known.cloned().map(|known| SourceFile {
                        path: display(&file.path),
                        ..known
                    });
                    let batch = known.map(Write::SourceFile).into_iter().collect();
                    self.store.writer.write(batch).await?;
                }
                return Ok(());
            }
            Start::Resume(offset) => offset,
            Start::Restart => {
                if known.is_some_and(|known| known.parsed_offset > 0) {
                    report.restarted += 1;
                }
                0
            }
        };

        let source = Source {
            file: file.key.clone(),
            window: file.window.clone(),
        };
        let mut session = Some(self.session(file).await?);
        let mut limit = self.options.read_limit.max(1);
        let mut first = true;
        loop {
            let chunk = {
                let (parser, source, path) =
                    (self.provider.clone(), source.clone(), file.path.clone());
                blocking(move || tail::read_chunk(&parser, &source, &path, offset, limit)).await??
            };
            if chunk.batch.lines.is_empty() {
                if chunk.full {
                    // One line longer than the limit: read it whole.
                    limit = limit.saturating_mul(2);
                    continue;
                }
                // Nothing complete past the offset. A restart still records that the file
                // starts over, so the next pass does not restart again.
                if first && (start == Start::Restart || renamed) {
                    let batch = vec![self.source_file(file, offset, None)];
                    self.store.writer.write(batch).await?;
                }
                return Ok(());
            }
            let end = chunk.batch.end;
            let batch = self.batch(file, offset, &chunk, session.take());
            self.store.writer.write(batch).await?;
            let parsed = end - offset;
            report.parsed_bytes += parsed;
            self.parsed.fetch_add(parsed, Ordering::Relaxed);
            offset = end;
            first = false;
            limit = self.options.read_limit.max(1);
        }
    }

    /// The writes of one read: the file's new offset first, then the window, the rows and the
    /// gaps. One batch, one transaction.
    fn batch(
        &self,
        file: &Found,
        offset: u64,
        chunk: &Chunk,
        session: Option<Vec<Write>>,
    ) -> Vec<Write> {
        let provider = self.provider.provider();
        let version = self.provider.version();
        let mut batch = vec![self.source_file(file, chunk.batch.end, chunk.last_hash.clone())];
        batch.extend(session.into_iter().flatten());
        let recorded_ms = now_ms();
        for record in chunk.batch.records() {
            batch.push(match record.clone() {
                Record::Message(mut message) => {
                    if cfg!(mutant_ingest_no_dedup) {
                        // Mutant: the message key includes the file, so a message seen in two
                        // files is counted twice.
                        message.id = format!("{}@{}", message.id, message.origin.file);
                    }
                    Write::Message {
                        message,
                        parser_version: version,
                    }
                }
                Record::ToolResult(result) => Write::ToolResult(result),
                Record::FileTouch(touch) => Write::FileTouch(touch),
                Record::SessionCheck(check) => Write::SessionCheck { check, recorded_ms },
            });
        }
        for gap in chunk.batch.gaps() {
            batch.push(Write::Gap {
                provider: provider.to_owned(),
                file_key: file.key.clone(),
                gap: gap.clone(),
                parser_version: version,
            });
        }
        debug_assert!(offset <= chunk.batch.end);
        batch
    }

    fn source_file(&self, file: &Found, offset: u64, hash: Option<String>) -> Write {
        Write::SourceFile(SourceFile {
            provider: self.provider.provider().to_owned(),
            file_key: file.key.clone(),
            path: display(&file.path),
            size: file.size.max(offset),
            parsed_offset: offset,
            parser_version: self.provider.version(),
            last_line_hash: hash,
        })
    }

    /// The window's session row, from its meta file when it has one. A meta file that does
    /// not parse is recorded as a gap of its own source file, and the window keeps what its
    /// path says.
    async fn session(&self, file: &Found) -> Result<Vec<Write>, FileError> {
        let provider = self.provider.provider().to_owned();
        let main = WindowKey {
            session_id: file.window.session_id.clone(),
            agent_id: None,
        };
        let plain = WindowMeta {
            window: ContextWindow {
                key: file.window.clone(),
                parent: file.window.agent_id.is_some().then_some(main),
                // Stage 0 spawns nothing: every window is interactive (#6).
                kind: WindowKind::Interactive,
            },
            agent_type: None,
            spawn_depth: None,
            spawned_by: None,
        };
        let Some(meta) = file.meta.clone() else {
            return Ok(vec![Write::Session {
                provider,
                meta: plain,
            }]);
        };
        let read = blocking(move || {
            let bytes = std::fs::read(&meta)?;
            let key = tail::file_key(&meta)?;
            Ok::<_, std::io::Error>((meta, key, bytes))
        })
        .await?;
        // A meta file that went away or cannot be read: the window keeps what its path says.
        let Ok((path, key, bytes)) = read else {
            return Ok(vec![Write::Session {
                provider,
                meta: plain,
            }]);
        };
        Ok(match self.provider.parse_meta(&file.window, &bytes) {
            Ok(meta) => vec![Write::Session { provider, meta }],
            Err(gap) => {
                let size = bytes.len() as u64;
                vec![
                    Write::SourceFile(SourceFile {
                        provider: provider.clone(),
                        file_key: key.clone(),
                        path: display(&path),
                        size,
                        parsed_offset: size,
                        parser_version: self.provider.version(),
                        last_line_hash: None,
                    }),
                    Write::Gap {
                        provider: provider.clone(),
                        file_key: key,
                        gap,
                        parser_version: self.provider.version(),
                    },
                    Write::Session {
                        provider,
                        meta: plain,
                    },
                ]
            }
        })
    }
}

/// What a reingest writes before it re-reads: nothing, since it never truncates. Under the
/// mutant `mutant_ingest_reingest_truncates`, the deletion of every row.
fn truncate() -> Vec<Write> {
    #[cfg(mutant_ingest_reingest_truncates)]
    return vec![Write::Truncate];
    #[cfg(not(mutant_ingest_reingest_truncates))]
    Vec::new()
}

/// Why one file failed: an I/O error skips the file; anything else ends the pass.
#[derive(Debug)]
enum FileError {
    Io(String),
    Fatal(Error),
}

impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<Error> for FileError {
    fn from(e: Error) -> Self {
        Self::Fatal(e)
    }
}

impl From<bosun_store::Error> for FileError {
    fn from(e: bosun_store::Error) -> Self {
        Self::Fatal(Error::Store(e))
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Task(e.to_string()))
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[derive(Debug)]
enum Command {
    Rescan(oneshot::Sender<Result<Report, Error>>),
    Reingest(oneshot::Sender<Result<Report, Error>>),
    Shutdown(oneshot::Sender<()>),
}

/// A running ingest task: rescans at start and then every
/// [`Options::rescan_interval`], and takes commands in between.
#[derive(Debug)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
    parsed: Arc<AtomicU64>,
    last: Arc<std::sync::Mutex<Option<Result<Report, String>>>>,
}

/// Starts the ingest task of `ingest` on the current tokio runtime. The first pass runs at
/// once; then one runs every [`Options::rescan_interval`]. A tick missed while the machine
/// slept runs once on wake-up.
#[must_use]
pub fn spawn<P: Provider>(ingest: Ingest<P>) -> Handle {
    let (commands, mut inbox) = mpsc::channel(8);
    let parsed = ingest.parsed.clone();
    let last = Arc::new(std::sync::Mutex::new(None));
    let record = last.clone();
    let task = tokio::spawn(async move {
        let every = ingest.options.rescan_interval;
        let mut timer = tokio::time::interval(every);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = timer.tick() => {
                    let result = ingest.scan().await.map_err(|e| e.to_string());
                    if let Ok(mut last) = record.lock() {
                        *last = Some(result);
                    }
                }
                command = inbox.recv() => match command {
                    Some(Command::Rescan(reply)) => {
                        let _ = reply.send(ingest.scan().await);
                    }
                    Some(Command::Reingest(reply)) => {
                        let _ = reply.send(ingest.reingest().await);
                    }
                    Some(Command::Shutdown(reply)) => {
                        let _ = reply.send(());
                        return;
                    }
                    None => return,
                },
            }
        }
    });
    Handle {
        commands,
        task,
        parsed,
        last,
    }
}

impl Handle {
    /// Runs a pass now and waits for it.
    ///
    /// # Errors
    ///
    /// The pass failed, or the task has stopped.
    pub async fn rescan(&self) -> Result<Report, Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Rescan(reply))
            .await
            .map_err(|_| Error::Stopped)?;
        answer.await.map_err(|_| Error::Stopped)?
    }

    /// Runs a reingest now and waits for it.
    ///
    /// # Errors
    ///
    /// The reingest failed, or the task has stopped.
    pub async fn reingest(&self) -> Result<Report, Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Reingest(reply))
            .await
            .map_err(|_| Error::Stopped)?;
        answer.await.map_err(|_| Error::Stopped)?
    }

    /// Bytes of complete lines parsed since the ingest was created.
    #[must_use]
    pub fn parsed_bytes(&self) -> u64 {
        self.parsed.load(Ordering::Relaxed)
    }

    /// The outcome of the last timed pass, if one has run.
    #[must_use]
    pub fn last_pass(&self) -> Option<Result<Report, String>> {
        self.last.lock().ok().and_then(|last| last.clone())
    }

    /// Stops the task after the pass in progress, if any.
    ///
    /// # Errors
    ///
    /// The task had already died.
    pub async fn shutdown(self) -> Result<(), Error> {
        let (reply, answer) = oneshot::channel();
        if self.commands.send(Command::Shutdown(reply)).await.is_ok() {
            let _ = answer.await;
        }
        self.task.await.map_err(|e| Error::Task(e.to_string()))
    }
}
