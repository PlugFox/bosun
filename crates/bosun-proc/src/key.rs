//! Which process a pid names: the pid and the start time together, so a reused pid is told
//! apart from the process that was recorded.

use std::io;

use crate::sys;

/// When a process started, in the platform's own unit: opaque, only ever compared for
/// equality with another start time read on the same machine since the same boot.
///
/// Windows: the creation time (100 ns since 1601). Linux: clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). macOS: microseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StartTime(u64);

impl StartTime {
    /// A start time from its stored form, [`StartTime::raw`].
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The stored form, for the database.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A process as the daemon records it: pid and start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessKey {
    /// The operating system's process id.
    pub pid: u32,
    /// When it started; tells the process apart from a later one with the same pid.
    pub start: StartTime,
}

/// The start time of the live process `pid`, or `None` when no process the user can inspect
/// runs under that pid (none at all, one that has exited and waits to be reaped, or another
/// user's).
///
/// # Errors
///
/// When the operating system fails otherwise, or on a Unix that is neither Linux nor macOS.
pub fn start_time(pid: u32) -> io::Result<Option<StartTime>> {
    Ok(sys::start_time(pid)?.map(StartTime))
}

/// Whether the process `key` names is still running: its pid is live and started at
/// `key.start`.
#[must_use]
pub fn is_running(key: ProcessKey) -> bool {
    matches!(start_time(key.pid), Ok(Some(start)) if start == key.start)
}

/// Kills the process a previous daemon recorded, with what is left of its tree, only if its pid
/// and start time both still match. Returns whether it killed anything.
///
/// Used at start-up for children of a daemon that died. On Windows the job's kill-on-close has
/// already taken the tree, so this terminates the one recorded process, through the same handle
/// its start time was read from (the pid cannot be reused in between). On Unix the recorded
/// child led its own process group, so the group is killed; the check and the kill are two
/// system calls, and a process that exits and is replaced in between is the remaining race.
///
/// # Errors
///
/// When the operating system refuses the inspection or the kill.
pub fn reap(recorded: ProcessKey) -> io::Result<bool> {
    sys::reap(recorded.pid, recorded.start.raw())
}

#[cfg(test)]
mod tests {
    use std::process::Child;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::helper;

    fn exits_within(child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_live_process_has_a_start_time_and_an_exited_one_has_none() {
        let mut child = helper::std_command(helper::Role::Sleep)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start = start_time(pid).expect("start time").expect("live");
        assert!(is_running(ProcessKey { pid, start }));
        assert_eq!(start_time(pid).expect("start time"), Some(start), "stable");
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert_eq!(start_time(pid).expect("start time"), None);
        assert!(!is_running(ProcessKey { pid, start }));
    }

    #[test]
    fn reap_kills_a_process_whose_pid_and_start_time_match() {
        let mut child = helper::std_command(helper::Role::Sleep)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start = start_time(pid).expect("start time").expect("live");
        let killed = reap(ProcessKey { pid, start }).expect("reap");
        let exited = exits_within(&mut child, Duration::from_secs(5));
        if !exited {
            child.kill().ok();
        }
        assert!(killed && exited, "killed {killed}, exited {exited}");
    }

    /// The pid-reuse case: the pid is live, but the process started at another time than the
    /// recorded one, so it is not the recorded process and must survive.
    #[test]
    fn reap_spares_a_process_whose_start_time_differs() {
        let mut child = helper::std_command(helper::Role::Sleep)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start = start_time(pid).expect("start time").expect("live");
        let recorded = ProcessKey {
            pid,
            start: StartTime::from_raw(start.raw() ^ 1),
        };
        let killed = reap(recorded).expect("reap");
        let exited = exits_within(&mut child, Duration::from_secs(1));
        child.kill().ok();
        child.wait().ok();
        assert!(!killed && !exited, "killed {killed}, exited {exited}");
    }
}
