//! Counters of an ingest: what it parsed, and the high-water marks of its bounds (#22), so
//! that tests assert the bounds on what was actually held rather than on resident memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A level that goes up and down, and the highest it has been.
#[derive(Debug, Default)]
pub(crate) struct Gauge {
    now: AtomicU64,
    max: AtomicU64,
}

impl Gauge {
    /// Raises the level by `n` until the returned hold is dropped.
    pub(crate) fn hold(gauge: &Arc<Self>, n: u64) -> Hold {
        let now = gauge.now.fetch_add(n, Ordering::SeqCst) + n;
        gauge.max.fetch_max(now, Ordering::SeqCst);
        Hold {
            gauge: gauge.clone(),
            n,
        }
    }

    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn max(&self) -> u64 {
        self.max.load(Ordering::SeqCst)
    }
}

/// A part of a [`Gauge`]'s level, given back on drop.
#[derive(Debug)]
pub(crate) struct Hold {
    gauge: Arc<Gauge>,
    n: u64,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.gauge.now.fetch_sub(self.n, Ordering::SeqCst);
    }
}

/// The live counters of one ingest, shared by its clones and its task.
#[derive(Debug, Default)]
pub(crate) struct Meters {
    /// Bytes of complete lines parsed.
    pub(crate) parsed: AtomicU64,
    /// Tails that opened a transcript to read past its offset.
    pub(crate) tails: AtomicU64,
    /// Files held open by tailers: transcripts, meta files, and on Windows the handle that
    /// reads a file id.
    pub(crate) open: Arc<Gauge>,
    /// Bytes read into memory and not yet committed.
    pub(crate) in_flight: Arc<Gauge>,
    /// Files waiting in each lane.
    pub(crate) queued_live: AtomicU64,
    pub(crate) queued_backfill: AtomicU64,
}

impl Meters {
    pub(crate) fn stats(&self) -> Stats {
        Stats {
            parsed_bytes: self.parsed.load(Ordering::Relaxed),
            tails: self.tails.load(Ordering::Relaxed),
            open_files: self.open.now(),
            max_open_files: self.open.max(),
            in_flight_bytes: self.in_flight.now(),
            max_in_flight_bytes: self.in_flight.max(),
            queued_live: self.queued_live.load(Ordering::Relaxed),
            queued_backfill: self.queued_backfill.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of an ingest's counters since it was created.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Bytes of complete lines parsed, over every pass and tail.
    pub parsed_bytes: u64,
    /// Tails that opened a transcript to read past its stored offset.
    pub tails: u64,
    /// Files held open by tailers now, and the most ever held at once. Bounded by
    /// [`crate::Options::max_open_files`].
    pub open_files: u64,
    pub max_open_files: u64,
    /// Bytes read and not yet committed now, and the most ever at once. Bounded by
    /// [`crate::Options::max_in_flight_bytes`].
    pub in_flight_bytes: u64,
    pub max_in_flight_bytes: u64,
    /// Files waiting to be tailed: changed files with priority (watch events, recently
    /// modified files), and the backfill.
    pub queued_live: u64,
    pub queued_backfill: u64,
}

/// Failpoints of the backfill-kill test: the process aborts at the `n`-th time one point is
/// reached. Compiled out of every other build.
#[cfg(test)]
pub(crate) mod failpoint {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Point {
        /// A chunk was read and parsed; its batch was not sent.
        Read,
        /// A batch was handed to the writer, which may be inside its transaction.
        Write,
        /// A batch was committed; the tailer has not moved on.
        Commit,
    }

    static ARMED: Mutex<Option<(Point, u64)>> = Mutex::new(None);
    static REACHED: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn arm(point: Point, nth: u64) {
        *ARMED.lock().unwrap() = Some((point, nth));
        REACHED.store(0, Ordering::SeqCst);
    }

    /// Whether this is the armed point's `n`-th time. The caller aborts.
    pub(crate) fn fires(point: Point) -> bool {
        let armed = *ARMED.lock().unwrap();
        match armed {
            Some((armed, nth)) if armed == point => {
                REACHED.fetch_add(1, Ordering::SeqCst) + 1 == nth
            }
            _ => false,
        }
    }

    pub(crate) fn abort(point: Point) -> ! {
        eprintln!("failpoint: abort at {point:?}");
        std::process::abort();
    }
}
