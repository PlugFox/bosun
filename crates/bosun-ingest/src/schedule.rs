//! Which file is tailed when (#22): the bounded set of tailers, the two lanes, and the task
//! [`spawn`] starts.
//!
//! - **Lanes.** A changed file is *live* when a watch event names it or it was written within
//!   [`Options::live_window`]; every other changed file is *backfill*, newest first. A free
//!   tailer always takes a live file first, and the backfill may hold all tailers but one, so a
//!   live file never waits for the backfill to finish, nor for more than one tailer to free.
//! - **Bounds.** At most [`Options::max_open_files`] tailers run, each holding at most one file
//!   open; their reads share the [`Options::max_in_flight_bytes`] budget. Queued work is a path
//!   per file, never its bytes. Watch events arrive through a bounded queue; when it is full
//!   they are dropped and a rescan is asked for instead.
//! - **One tailer per file.** A file named again while a tailer holds it is tailed once more
//!   after it, so a burst of events becomes at most one more read.
//! - **Debounce.** Watch events are gathered for [`Options::debounce`] from the first one; the
//!   files they name are then queued once each.
//! - **Resume.** Every read commits with the file's new offset, so a killed backfill resumes
//!   where its commits stopped: the restarted ingest's first pass finds the same files changed
//!   by the bytes not yet parsed, and nothing else.
//!
//! [`Options::live_window`]: crate::Options::live_window
//! [`Options::max_open_files`]: crate::Options::max_open_files
//! [`Options::max_in_flight_bytes`]: crate::Options::max_in_flight_bytes
//! [`Options::debounce`]: crate::Options::debounce

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::{mpsc, oneshot};
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};

use crate::ingest::{Error, FileError, Ingest, Planned, Report, Tailed};
use crate::layout::{FileKind, Provider};
use crate::meter::{Meters, Stats};
use crate::tail::{self, Found};
use crate::watch::{Change, Changes, Watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    Live,
    Backfill,
}

#[derive(Debug, Clone, Copy)]
struct Queued {
    lane: Lane,
    /// Read from zero (a reingest).
    restart: bool,
}

type Outcome = (Lane, Result<Tailed, FileError>);

/// The queue of changed files and the tailers that read them.
#[derive(Debug)]
pub(crate) struct Core<P> {
    ingest: Ingest<P>,
    live: VecDeque<PathBuf>,
    backfill: VecDeque<PathBuf>,
    /// The lane of every queued file; deque entries whose lane no longer matches are stale.
    queued: HashMap<PathBuf, Queued>,
    running: HashMap<PathBuf, Lane>,
    /// Files named again while a tailer held them.
    again: HashSet<PathBuf>,
    tasks: HashMap<Id, PathBuf>,
    jobs: JoinSet<Outcome>,
    backfill_running: usize,
    report: Report,
    fatal: Option<Error>,
}

impl<P: Provider> Core<P> {
    pub(crate) fn new(ingest: Ingest<P>) -> Self {
        Self {
            ingest,
            live: VecDeque::new(),
            backfill: VecDeque::new(),
            queued: HashMap::new(),
            running: HashMap::new(),
            again: HashSet::new(),
            tasks: HashMap::new(),
            jobs: JoinSet::new(),
            backfill_running: 0,
            report: Report::default(),
            fatal: None,
        }
    }

    fn meters(&self) -> &Meters {
        &self.ingest.meters
    }

    /// Queues the changed files of a pass: recently written ones live, the rest as backfill,
    /// newest first.
    pub(crate) async fn plan(
        &mut self,
        found: Vec<Found>,
        failed: Vec<(PathBuf, String)>,
        reingest: bool,
    ) -> Result<(), Error> {
        // A rescan may join a pass still reading: the files found are those of the latest walk,
        // not the sum over walks.
        if cfg!(mutant_ingest_pass_files_summed) {
            self.report.files += found.len() as u64;
        } else {
            self.report.files = found.len() as u64;
        }
        self.report.failed.extend(failed);
        let planned = self.ingest.changed(found, reingest).await?;
        let now = SystemTime::now();
        let window = self.ingest.options.live_window;
        let (live, mut backfill): (Vec<Planned>, Vec<Planned>) =
            planned.into_iter().partition(|file| {
                // A time in the future (a clock step) counts as recent.
                file.modified.is_some_and(|at| {
                    now.duration_since(at).is_ok_and(|age| age <= window) || at > now
                })
            });
        backfill.sort_by_key(|file| std::cmp::Reverse(file.modified));
        for file in live {
            self.enqueue(file.path, Lane::Live, reingest);
        }
        for file in backfill {
            self.enqueue(file.path, Lane::Backfill, reingest);
        }
        Ok(())
    }

    fn enqueue(&mut self, path: PathBuf, lane: Lane, restart: bool) {
        if self.running.contains_key(&path) {
            self.again.insert(path);
            return;
        }
        if let Some(queued) = self.queued.get_mut(&path) {
            queued.restart |= restart;
            if lane == Lane::Live && queued.lane == Lane::Backfill {
                queued.lane = Lane::Live;
                self.live.push_back(path);
                self.meters()
                    .queued_backfill
                    .fetch_sub(1, Ordering::Relaxed);
                self.meters().queued_live.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        self.queued.insert(path.clone(), Queued { lane, restart });
        match lane {
            Lane::Live => {
                self.live.push_back(path);
                self.meters().queued_live.fetch_add(1, Ordering::Relaxed);
            }
            Lane::Backfill => {
                self.backfill.push_back(path);
                self.meters()
                    .queued_backfill
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn pop(&mut self, lane: Lane) -> Option<(PathBuf, bool)> {
        loop {
            let path = match lane {
                Lane::Live => self.live.pop_front()?,
                Lane::Backfill => self.backfill.pop_front()?,
            };
            if self.queued.get(&path).is_some_and(|q| q.lane == lane) {
                let queued = self.queued.remove(&path)?;
                let counter = match lane {
                    Lane::Live => &self.meters().queued_live,
                    Lane::Backfill => &self.meters().queued_backfill,
                };
                counter.fetch_sub(1, Ordering::Relaxed);
                return Some((path, queued.restart));
            }
        }
    }

    /// Starts tailers on queued files while the bounds allow: live files first.
    pub(crate) fn dispatch(&mut self) {
        let cap = if cfg!(mutant_ingest_unbounded) {
            // Mutant: no cap on tailers, and so on open files.
            usize::MAX
        } else {
            self.ingest.options.max_open_files.max(1)
        };
        let backfill_first = cfg!(mutant_ingest_backfill_first);
        // One tailer is kept for live files, unless there is only one.
        let backfill_cap = if backfill_first || cap < 2 {
            cap
        } else {
            cap - 1
        };
        while self.fatal.is_none() && self.jobs.len() < cap {
            let backfill_free = self.backfill_running < backfill_cap;
            let next = if backfill_first {
                // Mutant: the backfill is taken before live files.
                backfill_free
                    .then(|| {
                        self.pop(Lane::Backfill)
                            .map(|(p, r)| (p, Lane::Backfill, r))
                    })
                    .flatten()
                    .or_else(|| self.pop(Lane::Live).map(|(p, r)| (p, Lane::Live, r)))
            } else {
                self.pop(Lane::Live)
                    .map(|(p, r)| (p, Lane::Live, r))
                    .or_else(|| {
                        backfill_free
                            .then(|| {
                                self.pop(Lane::Backfill)
                                    .map(|(p, r)| (p, Lane::Backfill, r))
                            })
                            .flatten()
                    })
            };
            let Some((path, lane, restart)) = next else {
                break;
            };
            let ingest = self.ingest.clone();
            let file = path.clone();
            let task = self
                .jobs
                .spawn(async move { (lane, ingest.tail(file, restart).await) });
            self.tasks.insert(task.id(), path.clone());
            self.running.insert(path, lane);
            if lane == Lane::Backfill {
                self.backfill_running += 1;
            }
        }
    }

    pub(crate) fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// Nothing runs and nothing can start.
    pub(crate) fn idle(&self) -> bool {
        self.jobs.is_empty() && (self.queued.is_empty() || self.fatal.is_some())
    }

    pub(crate) async fn join_next(&mut self) -> Option<Result<(Id, Outcome), JoinError>> {
        self.jobs.join_next_with_id().await
    }

    /// Records what a tailer did and frees its file.
    pub(crate) fn finish(&mut self, joined: Result<(Id, Outcome), JoinError>) {
        let (id, outcome) = match joined {
            Ok((id, outcome)) => (id, Ok(outcome)),
            Err(e) => (e.id(), Err(e)),
        };
        let Some(path) = self.tasks.remove(&id) else {
            return;
        };
        if self.running.remove(&path) == Some(Lane::Backfill) {
            self.backfill_running -= 1;
        }
        match outcome {
            Ok((_, Ok(tailed))) => {
                self.report.parsed_bytes += tailed.parsed;
                self.report.restarted += u64::from(tailed.restarted);
            }
            Ok((_, Err(FileError::Io(e)))) => self.report.failed.push((path.clone(), e)),
            Ok((_, Err(FileError::Fatal(e)))) => self.fail(e),
            Err(e) => self.fail(Error::Task(e.to_string())),
        }
        if self.again.remove(&path) {
            self.enqueue(path, Lane::Live, false);
        }
    }

    /// Stops starting tailers until the error is taken by [`Core::take`].
    pub(crate) fn fail(&mut self, e: Error) {
        self.fatal.get_or_insert(e);
    }

    /// What was done since the last call. After a fatal error, the queue is dropped: the next
    /// pass finds the same files again.
    pub(crate) fn take(&mut self) -> Result<Report, Error> {
        let report = std::mem::take(&mut self.report);
        match self.fatal.take() {
            None => Ok(report),
            Some(e) => {
                self.live.clear();
                self.backfill.clear();
                self.queued.clear();
                self.again.clear();
                self.meters().queued_live.store(0, Ordering::Relaxed);
                self.meters().queued_backfill.store(0, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Runs until every queued file is tailed.
    pub(crate) async fn run(&mut self) -> Result<Report, Error> {
        loop {
            self.dispatch();
            if self.idle() {
                return self.take();
            }
            if let Some(joined) = self.join_next().await {
                self.finish(joined);
            }
        }
    }

    /// Waits for the running tailers; starts none.
    async fn drain(&mut self) {
        while let Some(joined) = self.join_next().await {
            self.finish(joined);
        }
    }
}

#[derive(Debug)]
enum Command {
    Rescan(oneshot::Sender<Result<Report, Error>>),
    Reingest(oneshot::Sender<Result<Report, Error>>),
    Shutdown(oneshot::Sender<()>),
}

type Last = Arc<Mutex<Option<Result<Report, String>>>>;

/// Who waits for a pass: callers of [`Handle::rescan`], and the timer (whose outcome is kept
/// as [`Handle::last_pass`]).
#[derive(Debug, Default)]
struct Waiting {
    callers: Vec<oneshot::Sender<Result<Report, Error>>>,
    timed: bool,
}

impl Waiting {
    fn is_empty(&self) -> bool {
        self.callers.is_empty() && !self.timed
    }

    fn merge(&mut self, other: Self) {
        self.callers.extend(other.callers);
        self.timed |= other.timed;
    }

    fn answer(self, result: Result<Report, Error>, last: &Last) {
        let copy = match &result {
            Ok(report) => Ok(report.clone()),
            Err(e) => Err(e.to_string()),
        };
        if self.timed
            && let Ok(mut last) = last.lock()
        {
            *last = Some(copy.clone());
        }
        // The first caller gets the result; any other a copy, an error as its text.
        let mut callers = self.callers.into_iter();
        if let Some(first) = callers.next() {
            let _ = first.send(result);
        }
        for caller in callers {
            let _ = caller.send(copy.clone().map_err(Error::Task));
        }
    }
}

/// A running ingest task: rescans at start and then every
/// [`Options::rescan_interval`](crate::Options::rescan_interval), tails what watch events
/// name in between, and takes commands.
#[derive(Debug)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
    meters: Arc<Meters>,
    last: Last,
}

/// Starts the ingest task of `ingest` on the current tokio runtime. The first pass runs at
/// once; then one runs every [`Options::rescan_interval`](crate::Options::rescan_interval).
/// A tick missed while the machine slept runs once on wake-up. With
/// [`Options::watch`](crate::Options::watch), the directory is watched from before the first
/// pass, and a file an event names is tailed within
/// [`Options::debounce`](crate::Options::debounce), ahead of the backfill.
#[must_use]
pub fn spawn<P: Provider>(ingest: Ingest<P>) -> Handle {
    let (commands, inbox) = mpsc::channel(8);
    let meters = ingest.meters.clone();
    let last = Arc::new(Mutex::new(None));
    let task = tokio::spawn(run(ingest, inbox, last.clone()));
    Handle {
        commands,
        task,
        meters,
        last,
    }
}

type Discovery = JoinHandle<Result<(Vec<Found>, Vec<(PathBuf, String)>), Error>>;

#[allow(clippy::too_many_lines)]
async fn run<P: Provider>(ingest: Ingest<P>, mut inbox: mpsc::Receiver<Command>, last: Last) {
    let options = ingest.options.clone();
    let mut core = Core::new(ingest.clone());
    let mut changes = Changes::new();
    let mut watch: Option<Watch> = None;
    let mut timer = tokio::time::interval(options.rescan_interval);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Files named by watch events, read together when the debounce ends.
    let mut dirty: HashSet<PathBuf> = HashSet::new();
    let mut deadline: Option<Instant> = None;
    // One discovery at a time runs beside the tailers.
    let mut discovery: Option<Discovery> = None;
    let mut requested = Waiting::default();
    let mut discovering = Waiting::default();
    // Passes whose files are queued: answered once the queue is empty.
    let mut planned = Waiting::default();
    loop {
        core.dispatch();
        if core.idle() && !planned.is_empty() {
            std::mem::take(&mut planned).answer(core.take(), &last);
        }
        if discovery.is_none() && !requested.is_empty() {
            if options.watch && watch.is_none() {
                // Watching starts before the walk, so nothing written after it is missed. A
                // directory that does not exist yet is watched from a later pass.
                watch = changes.watch(ingest.root()).ok();
            }
            let walker = ingest.clone();
            discovery = Some(tokio::spawn(async move { walker.discover().await }));
            discovering = std::mem::take(&mut requested);
        }
        tokio::select! {
            command = inbox.recv() => match command {
                Some(Command::Rescan(reply)) => requested.callers.push(reply),
                Some(Command::Reingest(reply)) => {
                    core.drain().await;
                    let _ = reply.send(ingest.reingest().await);
                }
                Some(Command::Shutdown(reply)) => {
                    core.drain().await;
                    let _ = reply.send(());
                    return;
                }
                None => {
                    core.drain().await;
                    return;
                }
            },
            Some(joined) = core.join_next(), if core.busy() => core.finish(joined),
            found = async { discovery.as_mut().expect("guarded").await }, if discovery.is_some() => {
                discovery = None;
                let planned_now = match found {
                    Ok(Ok((found, failed))) => core.plan(found, failed, false).await,
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(Error::Task(e.to_string())),
                };
                if let Err(e) = planned_now {
                    core.fail(e);
                }
                planned.merge(std::mem::take(&mut discovering));
            }
            Some(change) = changes.events.recv() => {
                match change {
                    Change::Paths(paths) => {
                        for path in paths {
                            let transcript = tail::within(&ingest.provider, ingest.root(), &path)
                                && ingest
                                    .provider
                                    .locate(&path)
                                    .is_some_and(|located| located.kind == FileKind::Transcript);
                            if transcript {
                                dirty.insert(path);
                            }
                        }
                    }
                    Change::Rescan => requested.timed = true,
                    Change::Broken => {
                        // Watched again from the next pass.
                        watch = None;
                        requested.timed = true;
                    }
                }
                if changes.lost() {
                    requested.timed = true;
                }
                if !dirty.is_empty() && deadline.is_none() {
                    deadline = Some(Instant::now() + options.debounce);
                }
            }
            () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                for path in dirty.drain() {
                    core.enqueue(path, Lane::Live, false);
                }
            }
            _ = timer.tick() => requested.timed = true,
        }
    }
}

impl Handle {
    /// Runs a pass now and waits for it: until every file it found changed is tailed.
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

    /// Runs a reingest now and waits for it. The tailers running when it is asked for finish
    /// first; queued files wait for it.
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
        self.meters.parsed.load(Ordering::Relaxed)
    }

    /// The counters of the ingest: bytes parsed, the high-water marks of its bounds, and what
    /// is queued.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.meters.stats()
    }

    /// The outcome of the last timed pass, if one has run.
    #[must_use]
    pub fn last_pass(&self) -> Option<Result<Report, String>> {
        self.last.lock().ok().and_then(|last| last.clone())
    }

    /// Stops the task once the running tailers finish; queued files are left for the next
    /// start, which resumes them from their stored offsets.
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
