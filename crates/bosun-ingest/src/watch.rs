//! Watching a provider's directory (#22): for latency only. An event names files that may
//! have changed; the scheduler reads them sooner than the next rescan would. Nothing depends
//! on an event arriving: a missed, dropped or overflowed event leaves the bytes for the
//! rescan, and an overflow asks for one at once.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// How many events may wait for the scheduler. When it is full, further events are dropped
/// and a rescan is asked for instead.
pub(crate) const QUEUE: usize = 1024;

/// A recursive watch of one directory, alive while this value is.
#[derive(Debug)]
pub(crate) struct Watch {
    _watcher: RecommendedWatcher,
}

/// What the watch tells the scheduler.
#[derive(Debug)]
pub(crate) enum Change {
    /// These paths may have changed.
    Paths(Vec<PathBuf>),
    /// Events were lost (the queue was full, or the platform's buffer overflowed): rescan.
    Rescan,
    /// The watch failed (its directory went away, say): rescan, and watch again.
    Broken,
}

/// The receiving end of a watch, and the flag the event thread raises when it drops events.
#[derive(Debug)]
pub(crate) struct Changes {
    pub(crate) events: mpsc::Receiver<Change>,
    sender: mpsc::Sender<Change>,
    lost: Arc<AtomicBool>,
}

impl Changes {
    pub(crate) fn new() -> Self {
        let (sender, events) = mpsc::channel(QUEUE);
        Self {
            events,
            sender,
            lost: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether events were dropped since the last call.
    pub(crate) fn lost(&self) -> bool {
        self.lost.swap(false, Ordering::SeqCst)
    }

    /// Starts watching `root` recursively. Event paths under the directory's canonical form
    /// (macOS reports `/private/var` for `/var`) are mapped back under `root`.
    pub(crate) fn watch(&self, root: &Path) -> notify::Result<Watch> {
        let sender = self.sender.clone();
        let lost = self.lost.clone();
        let canonical = std::fs::canonicalize(root).ok();
        let base = root.to_owned();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            let change = match event {
                Ok(event) if event.need_rescan() => Change::Rescan,
                Ok(Event {
                    kind: EventKind::Access(_),
                    ..
                }) => return,
                Ok(event) => Change::Paths(
                    event
                        .paths
                        .into_iter()
                        .map(|path| rebase(path, &base, canonical.as_deref()))
                        .collect(),
                ),
                Err(_) => Change::Broken,
            };
            if sender.try_send(change).is_err() {
                lost.store(true, Ordering::SeqCst);
                // Wake the scheduler if it is idle: one slot may have freed since.
                let _ = sender.try_send(Change::Rescan);
            }
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Watch { _watcher: watcher })
    }
}

fn rebase(path: PathBuf, root: &Path, canonical: Option<&Path>) -> PathBuf {
    if path.starts_with(root) {
        return path;
    }
    match canonical.and_then(|canonical| path.strip_prefix(canonical).ok()) {
        Some(rest) => root.join(rest),
        None => path,
    }
}
