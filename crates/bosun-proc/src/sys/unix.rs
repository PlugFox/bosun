//! Unix: process groups, the Linux parent-death signal and process start times through `libc`.

use std::io;

/// A process group led by the child: its pgid is the child's pid.
#[derive(Debug)]
pub(crate) struct Container {
    pgid: libc::pid_t,
}

fn pid_t(pid: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// `kill(target, SIGKILL)`; `Ok(false)` when no process matches.
fn sigkill(target: libc::pid_t) -> io::Result<bool> {
    // SAFETY: `kill` has no memory preconditions; any target and signal are valid arguments.
    if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Kills the group `pgid`; when no such group exists, the process `pgid` alone.
fn kill_group(pgid: u32) -> io::Result<bool> {
    let pgid = pid_t(pgid)?;
    if sigkill(-pgid)? {
        return Ok(true);
    }
    sigkill(pgid)
}

impl Container {
    /// Kills every process left in the group.
    pub(crate) fn kill_all(&self) -> io::Result<()> {
        sigkill(-self.pgid).map(|_| ())
    }

    /// [`Container::kill_all`], best effort.
    pub(crate) fn kill(&self) {
        self.kill_all().ok();
    }
}

/// Spawns `command` as the leader of a new process group; on Linux it also dies with the
/// spawning thread of this process.
pub(crate) fn spawn_contained(
    command: &mut tokio::process::Command,
) -> io::Result<(tokio::process::Child, Container)> {
    command.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = pid_t(std::process::id())?;
        // SAFETY: the closure runs in the forked child before `exec` and calls only `prctl`
        // and `getppid`, both async-signal-safe; it allocates nothing and takes no lock.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // The parent may have died before the signal was armed.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    let child = command.spawn()?;
    let Some(pgid) = child.id().and_then(|pid| pid_t(pid).ok()) else {
        return Err(io::Error::other("a fresh child has no pid"));
    };
    Ok((child, Container { pgid }))
}

/// The start time of a child this process spawned and has not reaped: even if it has
/// already exited, its pid stays reserved until the wait.
pub(crate) fn child_start_time(child: &tokio::process::Child) -> io::Result<u64> {
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the child has been reaped"))?;
    read_start(pid, true)?.ok_or_else(|| io::Error::other("the child has no start time"))
}

pub(crate) fn start_time(pid: u32) -> io::Result<Option<u64>> {
    read_start(pid, false)
}

pub(crate) fn reap(pid: u32, start: u64) -> io::Result<bool> {
    match start_time(pid)? {
        None => Ok(false),
        Some(found) if found != start && !cfg!(mutant_proc_skip_start_time) => Ok(false),
        Some(_) => kill_group(pid),
    }
}

/// The start time of `pid`; `None` when there is no such process, or it is a zombie and
/// `zombie` is false.
#[cfg(target_os = "linux")]
fn read_start(pid: u32, zombie: bool) -> io::Result<Option<u64>> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let (state, start) =
        parse_stat(&text).ok_or_else(|| io::Error::other("malformed /proc/<pid>/stat"))?;
    if !zombie && matches!(state, 'Z' | 'X' | 'x') {
        return Ok(None);
    }
    Ok(Some(start))
}

/// The state (field 3) and the start time (field 22) of a `/proc/<pid>/stat` line. The command
/// name (field 2) is in parentheses and may itself hold spaces and parentheses, so the fields
/// are counted from its last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(line: &str) -> Option<(char, u64)> {
    let rest = &line[line.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    // Fields 4 to 21 lie between the state and the start time.
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

#[cfg(target_os = "macos")]
fn read_start(pid: u32, zombie: bool) -> io::Result<Option<u64>> {
    let pid = pid_t(pid)?;
    // SAFETY: `proc_bsdinfo` is plain C data; all zeros is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .expect("proc_bsdinfo is small");
    // SAFETY: the buffer is `info`, writable, and `size` is its length.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if written != size {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ESRCH | libc::EPERM | 0) | None => Ok(None),
            _ => Err(e),
        };
    }
    if !zombie && info.pbi_status == libc::SZOMB {
        return Ok(None);
    }
    Ok(Some(
        info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_start(_pid: u32, _zombie: bool) -> io::Result<Option<u64>> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Holding the machine awake is not implemented on Unix yet (`systemd-inhibit` and
/// `caffeinate` are follow-ups): the request is a no-op.
#[derive(Debug)]
pub(crate) struct PowerRequest;

impl Drop for PowerRequest {
    fn drop(&mut self) {
        // Nothing was requested, so nothing is released.
    }
}

impl PowerRequest {
    #[expect(clippy::unnecessary_wraps, reason = "the Windows request can fail")]
    pub(crate) fn acquire(_reason: &str) -> io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_stat;

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        let stat = "4242 (a (b) c) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                    987654 1000 200 18446744073709551615\n";
        assert_eq!(parse_stat(stat), Some(('S', 987_654)));
        assert_eq!(parse_stat("4242 (x"), None);
    }
}
