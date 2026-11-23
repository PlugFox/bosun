//! Holding the machine awake while work runs.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::sys;

/// Requests that the machine stay awake.
pub trait Power: Send + Sync + fmt::Debug {
    /// Issues one request, with `reason` as the text the system shows for it; it holds until
    /// the guard is dropped.
    ///
    /// # Errors
    ///
    /// When the system refuses the request.
    fn acquire(&self, reason: &str) -> io::Result<PowerGuard>;
}

/// A held power request: dropping it issues the one release.
#[must_use = "the request is released when the guard is dropped"]
pub struct PowerGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl PowerGuard {
    /// A guard that runs `release` once, on drop: for other implementations of [`Power`].
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl fmt::Debug for PowerGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PowerGuard").finish_non_exhaustive()
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// The platform's request. Windows: a `PowerRequestSystemRequired` power request
/// (`PowerCreateRequest`, `PowerSetRequest`), listed by `powercfg /requests` under `SYSTEM`
/// while held; the display may still turn off. Unix: a no-op for now; `systemd-inhibit` on
/// Linux and `caffeinate` on macOS are follow-ups.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPower;

impl Power for SystemPower {
    fn acquire(&self, reason: &str) -> io::Result<PowerGuard> {
        let request = sys::PowerRequest::acquire(reason)?;
        Ok(PowerGuard::new(move || drop(request)))
    }
}

/// A [`Power`] that only counts, for tests. Clones share the counts.
#[derive(Debug, Default, Clone)]
pub struct FakePower {
    counts: Arc<Counts>,
}

#[derive(Debug, Default)]
struct Counts {
    acquired: AtomicUsize,
    released: AtomicUsize,
}

impl FakePower {
    /// How many requests were issued.
    #[must_use]
    pub fn acquired(&self) -> usize {
        self.counts.acquired.load(Ordering::SeqCst)
    }

    /// How many requests were released.
    #[must_use]
    pub fn released(&self) -> usize {
        self.counts.released.load(Ordering::SeqCst)
    }

    /// Requests issued and not yet released.
    #[must_use]
    pub fn held(&self) -> usize {
        self.acquired() - self.released()
    }
}

impl Power for FakePower {
    fn acquire(&self, _reason: &str) -> io::Result<PowerGuard> {
        self.counts.acquired.fetch_add(1, Ordering::SeqCst);
        let counts = Arc::clone(&self.counts);
        Ok(PowerGuard::new(move || {
            counts.released.fetch_add(1, Ordering::SeqCst);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_issues_one_request_and_one_release() {
        let power = FakePower::default();
        let guard = power.acquire("a run").expect("acquire");
        assert_eq!((power.acquired(), power.released()), (1, 0));
        drop(guard);
        assert_eq!((power.acquired(), power.released()), (1, 1));
        assert_eq!(power.held(), 0);
    }

    #[test]
    fn guards_are_counted_independently() {
        let power = FakePower::default();
        let first = power.acquire("one").expect("acquire");
        let second = power.clone().acquire("two").expect("acquire");
        assert_eq!(power.held(), 2);
        drop(first);
        assert_eq!(power.held(), 1);
        drop(second);
        assert_eq!((power.acquired(), power.released()), (2, 2));
    }

    #[test]
    fn the_system_request_is_set_and_cleared() {
        let guard = SystemPower.acquire("bosun-proc test").expect("acquire");
        drop(guard);
    }
}
