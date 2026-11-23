//! Discovery, tailing, rescan and reingest of one identity's transcripts (#3, #21, #22).
//!
//! - **Discovery** walks the provider's directory (for Claude Code, `<config dir>/projects`)
//!   and keys every transcript by its file id, so a rename keeps the file and its offset.
//! - **Tailing**: each file is read from its stored offset. The stored hash of the last parsed
//!   line tells a grown file from a rewritten one; a file that shrank, or whose line before the
//!   offset no longer matches, is re-read from zero, which message dedup makes safe. A last
//!   line without its newline is left for the next read.
//! - **Writes**: the rows of one read of one file and that file's new offset are one batch,
//!   one transaction of the writer task, so a crash never records an offset past rows that
//!   were not written, and a restart resumes every file where its last commit left it.
//! - **Rescan**: every file is stat'ed and compared with its stored offset, at start and then
//!   on a timer ([`crate::spawn`]). It is the source of truth: watching (#22) only makes a
//!   change arrive sooner, and the rescan covers missed events, sleep and clock steps without
//!   detecting them.
//! - **Scheduling** ([`crate::schedule`]): changed files are tailed by a bounded set of
//!   tailers, live files before the backfill.
//! - **Reingest** re-reads every file that still exists from zero with the current parser and
//!   upserts its rows. It never deletes: rows of a deleted file keep their parser version.
//! - A file that vanished keeps its rows; nothing is ever deleted by ingest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bosun_core::{ContextWindow, WindowKey, WindowKind, WindowMeta};
use bosun_store::{Repo, SourceFile, Store, Write, Writer};
use tokio::sync::{OnceCell, Semaphore};

use crate::layout::Provider;
use crate::meter::{Gauge, Meters, Stats};
use crate::record::{Record, Source};
use crate::schedule::Core;
use crate::tail::{self, Chunk, Found};

/// Settings of an [`Ingest`].
#[derive(Debug, Clone)]
pub struct Options {
    /// How often [`crate::spawn`] rescans every file.
    pub rescan_interval: Duration,
    /// The most bytes one read takes, and so the most one batch holds. A line longer than
    /// this is read whole, by doubling the read until the line fits.
    pub read_limit: u64,
    /// Whether [`crate::spawn`] watches the provider's directory, so that a change is read
    /// within [`Options::debounce`] instead of at the next rescan. Missed or dropped events
    /// are harmless: the rescan still finds the bytes.
    pub watch: bool,
    /// How long watch events are gathered before the files they name are read: a burst of
    /// appends to one file is one read.
    pub debounce: Duration,
    /// The most files tailers hold open at once (at least 1). With 2 or more, one of them is
    /// kept for live files, so the backfill never holds them all.
    pub max_open_files: usize,
    /// The most bytes read and not yet committed, over all tailers. Each read reserves its
    /// whole limit first; a single line longer than this budget is read alone.
    pub max_in_flight_bytes: u64,
    /// A changed file written within this window is live: it is tailed before the backfill.
    pub live_window: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            rescan_interval: Duration::from_secs(45),
            read_limit: 8 << 20,
            watch: true,
            debounce: Duration::from_millis(200),
            max_open_files: 8,
            max_in_flight_bytes: 64 << 20,
            live_window: Duration::from_mins(10),
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
///
/// Clones share the counters and the stored offsets. One ingest (and its clones) is the only
/// writer of its provider's files in its store; passes of one ingest run one at a time.
#[derive(Debug, Clone)]
pub struct Ingest<P> {
    pub(crate) provider: P,
    root: PathBuf,
    store: Arc<StoreHandles>,
    pub(crate) options: Options,
    pub(crate) meters: Arc<Meters>,
    /// The stored offset of every file, read from the store once and then kept as each batch
    /// commits, so a tailer does not query the store for it.
    known: Arc<OnceCell<Mutex<HashMap<String, SourceFile>>>>,
    /// The in-flight byte budget, in bytes (at most `u32::MAX`).
    budget: Arc<Semaphore>,
}

#[derive(Debug)]
struct StoreHandles {
    writer: Writer,
    reader: bosun_store::Reader,
}

/// Where a read of a file starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// The file grew: from the stored offset, with the lines counted up to it (`None` when
    /// they were not counted) and the last timestamp before it.
    Resume {
        offset: u64,
        lines: Option<u64>,
        last_ts_ms: Option<i64>,
    },
    /// New, shrunk or rewritten: from zero.
    Restart,
}

/// What one tail of one file did.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Tailed {
    pub(crate) parsed: u64,
    pub(crate) restarted: bool,
}

/// A file of a pass that may need reading, and whether it is live.
#[derive(Debug)]
pub(crate) struct Planned {
    pub(crate) path: PathBuf,
    pub(crate) modified: Option<SystemTime>,
}

impl<P: Provider> Ingest<P> {
    /// An ingest of the transcripts under `root` (for Claude Code, `<config dir>/projects`)
    /// into `store`. Nothing is read until [`Ingest::scan`].
    #[must_use]
    pub fn new(provider: P, root: PathBuf, store: &Store, options: Options) -> Self {
        let budget = u32::try_from(options.max_in_flight_bytes.max(1)).unwrap_or(u32::MAX);
        Self {
            provider,
            root,
            store: Arc::new(StoreHandles {
                writer: store.writer().clone(),
                reader: store.reader().clone(),
            }),
            options,
            meters: Arc::new(Meters::default()),
            known: Arc::new(OnceCell::new()),
            budget: Arc::new(Semaphore::new(budget as usize)),
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
        self.meters.parsed.load(Ordering::Relaxed)
    }

    /// The counters of this ingest and of its clones.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.meters.stats()
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
    /// parsed and written, by at most [`Options::max_open_files`] tailers at once. A file
    /// that cannot be read is reported and skipped.
    ///
    /// # Errors
    ///
    /// The store failed; files already written stay written.
    pub async fn scan(&self) -> Result<Report, Error> {
        let mut core = Core::new(self.clone());
        let (found, failed) = self.discover().await?;
        core.plan(found, failed, false).await?;
        core.run().await
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
        let mut core = Core::new(self.clone());
        let (found, failed) = self.discover().await?;
        core.plan(found, failed, true).await?;
        core.run().await
    }

    /// Every transcript under the root, and the files that could not be examined.
    pub(crate) async fn discover(&self) -> Result<(Vec<Found>, Vec<(PathBuf, String)>), Error> {
        let provider = self.provider.clone();
        let root = self.root.clone();
        let (found, failed) = blocking(move || tail::discover(&provider, &root)).await?;
        let failed = failed
            .into_iter()
            .map(|(path, e)| (path, e.to_string()))
            .collect();
        Ok((found, failed))
    }

    /// The files of a pass that may need reading: all of them for a reingest, else those
    /// whose size or path differs from what is stored.
    pub(crate) async fn changed(
        &self,
        found: Vec<Found>,
        reingest: bool,
    ) -> Result<Vec<Planned>, Error> {
        let known = self.known().await?;
        let known = known.lock().map_err(|e| Error::Task(e.to_string()))?;
        Ok(found
            .into_iter()
            .filter(|file| {
                reingest
                    || known.get(&file.key).is_none_or(|known| {
                        stored_offset(known) != file.size || known.path != display(&file.path)
                    })
            })
            .map(|file| Planned {
                path: file.path,
                modified: file.modified,
            })
            .collect())
    }

    /// The stored offsets, read from the store on first use.
    async fn known(&self) -> Result<&Mutex<HashMap<String, SourceFile>>, Error> {
        self.known
            .get_or_try_init(|| async {
                let files = if cfg!(mutant_ingest_backfill_from_zero) {
                    // Mutant: a restarted ingest forgets every stored offset.
                    Vec::new()
                } else {
                    self.store
                        .reader
                        .source_files(self.provider.provider())
                        .await?
                };
                let files = files
                    .into_iter()
                    .map(|file| (file.file_key.clone(), file))
                    .collect();
                Ok::<_, Error>(Mutex::new(files))
            })
            .await
    }

    async fn known_file(&self, key: &str) -> Result<Option<SourceFile>, Error> {
        let known = self.known().await?;
        let known = known.lock().map_err(|e| Error::Task(e.to_string()))?;
        Ok(known.get(key).cloned())
    }

    /// Writes `batch`, whose first write is `file`'s row, and then keeps `file` as known.
    async fn commit(&self, file: Option<SourceFile>, batch: Vec<Write>) -> Result<(), Error> {
        #[cfg(test)]
        if crate::meter::failpoint::fires(crate::meter::failpoint::Point::Write) {
            // The batch goes to the writer, which may be inside its transaction at the abort.
            let writer = self.store.writer.clone();
            drop(tokio::spawn(async move { writer.write(batch).await }));
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            crate::meter::failpoint::abort(crate::meter::failpoint::Point::Write);
        }
        self.store.writer.write(batch).await?;
        #[cfg(test)]
        if crate::meter::failpoint::fires(crate::meter::failpoint::Point::Commit) {
            crate::meter::failpoint::abort(crate::meter::failpoint::Point::Commit);
        }
        if let Some(file) = file {
            let known = self.known().await?;
            let mut known = known.lock().map_err(|e| Error::Task(e.to_string()))?;
            known.insert(file.file_key.clone(), file);
        }
        Ok(())
    }

    /// Tails the transcript at `path` from its stored offset (from zero when `restart`) to its
    /// last complete line. At most one file is held open at a time.
    pub(crate) async fn tail(&self, path: PathBuf, restart: bool) -> Result<Tailed, FileError> {
        let examined = {
            let (provider, open) = (self.provider.clone(), self.meters.open.clone());
            blocking(move || tail::examine(&provider, &path, Some(&open))).await??
        };
        let Some(file) = examined else {
            // Gone, or not a transcript: its rows stay.
            return Ok(Tailed::default());
        };
        let known = self.known_file(&file.key).await?;
        let stored = known.as_ref().map_or(0, stored_offset);
        let renamed = known
            .as_ref()
            .is_some_and(|known| known.path != display(&file.path));
        if !restart && stored > 0 && file.size == stored {
            if let Some(known) = known.filter(|_| renamed) {
                // Only the path changed: record it, parse nothing.
                let known = SourceFile {
                    path: display(&file.path),
                    ..known
                };
                self.commit(Some(known.clone()), vec![Write::SourceFile(known)])
                    .await?;
            }
            return Ok(Tailed::default());
        }

        // Something to read: the window's session row (its meta file is read and closed
        // first), then the transcript, held open until its last chunk is committed.
        let session = self.session(&file).await?;
        self.meters.tails.fetch_add(1, Ordering::Relaxed);
        let mut handle = {
            let (path, open) = (file.path.clone(), self.meters.open.clone());
            blocking(move || Open::file(&path, &open)).await??
        };
        let stored_lines = known.as_ref().and_then(|known| known.parsed_lines);
        let stored_ts = known.as_ref().and_then(|known| known.last_ts_ms);
        let start = match known
            .as_ref()
            .and_then(|known| known.last_line_hash.clone())
        {
            Some(hash) if !restart && stored > 0 && file.size > stored => {
                let (back, same) = blocking(move || {
                    let same = tail::hash_matches(&mut handle.file, stored, &hash);
                    (handle, same)
                })
                .await?;
                handle = back;
                if same? {
                    Start::Resume {
                        offset: stored,
                        lines: stored_lines,
                        last_ts_ms: stored_ts,
                    }
                } else {
                    Start::Restart
                }
            }
            _ => Start::Restart,
        };
        let restarted =
            start == Start::Restart && known.is_some_and(|known| known.parsed_offset > 0);
        let parsed = self.read(&file, handle, start, renamed, session).await?;
        Ok(Tailed { parsed, restarted })
    }

    /// Reads `file` through `handle` from where `start` says to its last complete line, one
    /// committed batch per read; the first carries `session`. Returns the bytes parsed.
    async fn read(
        &self,
        file: &Found,
        mut handle: Open,
        start: Start,
        renamed: bool,
        session: Vec<Write>,
    ) -> Result<u64, FileError> {
        let (mut offset, mut lines, mut last_ts) = match start {
            Start::Resume {
                offset,
                lines,
                last_ts_ms,
            } => (offset, lines, last_ts_ms),
            Start::Restart => (0, Some(0), None),
        };
        let mut session = Some(session);
        let mut parsed = 0;
        let source = Source {
            file: file.key.clone(),
            window: file.window.clone(),
        };
        let mut limit = self.options.read_limit.max(1);
        let mut first = true;
        loop {
            // The budget is reserved before the read and given back once its batch commits.
            let reserve = u32::try_from(limit.min(self.options.max_in_flight_bytes.max(1)))
                .unwrap_or(u32::MAX);
            let _permit = self
                .budget
                .clone()
                .acquire_many_owned(reserve)
                .await
                .map_err(|e| Error::Task(e.to_string()))?;
            let (back, chunk) = {
                let (parser, source) = (self.provider.clone(), source.clone());
                blocking(move || {
                    let at = (offset, last_ts);
                    let chunk = tail::read_chunk(&parser, &source, &mut handle.file, at, limit);
                    (handle, chunk)
                })
                .await?
            };
            handle = back;
            let chunk = chunk?;
            let _in_flight = Gauge::hold(&self.meters.in_flight, chunk.read);
            #[cfg(test)]
            if crate::meter::failpoint::fires(crate::meter::failpoint::Point::Read) {
                crate::meter::failpoint::abort(crate::meter::failpoint::Point::Read);
            }
            if chunk.batch.lines.is_empty() {
                if chunk.full {
                    // One line longer than the limit: read it whole.
                    limit = limit.saturating_mul(2);
                    continue;
                }
                // Nothing complete past the offset. A restart still records that the file
                // starts over, so the next pass does not restart again.
                if first && (start == Start::Restart || renamed) {
                    let row = self.source_file(file, offset, None, (lines, last_ts));
                    self.commit(Some(row.clone()), vec![Write::SourceFile(row)])
                        .await?;
                }
                return Ok(parsed);
            }
            let end = chunk.batch.end;
            lines = lines.map(|n| n + chunk.batch.lines.len() as u64);
            last_ts = chunk.batch.last_ts_ms;
            let row = self.source_file(file, end, chunk.last_hash.clone(), (lines, last_ts));
            let batch = self.batch(file, offset, row.clone(), &chunk, session.take());
            self.commit(Some(row), batch).await?;
            parsed += end - offset;
            self.meters
                .parsed
                .fetch_add(end - offset, Ordering::Relaxed);
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
        row: SourceFile,
        chunk: &Chunk,
        session: Option<Vec<Write>>,
    ) -> Vec<Write> {
        let provider = self.provider.provider();
        let version = self.provider.version();
        let mut batch = vec![Write::SourceFile(row)];
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

    fn source_file(
        &self,
        file: &Found,
        offset: u64,
        hash: Option<String>,
        (lines, last_ts_ms): (Option<u64>, Option<i64>),
    ) -> SourceFile {
        SourceFile {
            provider: self.provider.provider().to_owned(),
            file_key: file.key.clone(),
            path: display(&file.path),
            size: file.size.max(offset),
            parsed_offset: offset,
            parser_version: self.provider.version(),
            last_line_hash: hash,
            parsed_lines: lines,
            last_ts_ms,
        }
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
        let open = self.meters.open.clone();
        let read = blocking(move || {
            let _held = Gauge::hold(&open, 1);
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
                        // A meta file is one record, not lines: the gap is its one line.
                        parsed_lines: Some(1),
                        last_ts_ms: None,
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

/// The offset a file is resumed from.
fn stored_offset(known: &SourceFile) -> u64 {
    if cfg!(mutant_ingest_no_offset) {
        // Mutant: the stored offset is ignored.
        0
    } else {
        known.parsed_offset
    }
}

/// A transcript held open by a tailer, counted while it is.
#[derive(Debug)]
struct Open {
    file: std::fs::File,
    _held: crate::meter::Hold,
}

impl Open {
    fn file(path: &Path, open: &Arc<Gauge>) -> std::io::Result<Self> {
        let held = Gauge::hold(open, 1);
        Ok(Self {
            file: std::fs::File::open(path)?,
            _held: held,
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
pub(crate) enum FileError {
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

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Error> {
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
