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

    /// The pids of the live (not zombie) processes in the group. Best effort: the process
    /// table is read one process at a time, so a process that starts or exits meanwhile may be
    /// missed or listed; a process that left the group with `setsid` is not listed at all.
    pub(crate) fn members(&self) -> io::Result<Vec<u32>> {
        let pgid = u32::try_from(self.pgid).map_err(|_| io::Error::other("a negative pgid"))?;
        group_members(pgid)
    }
}

/// Linux: the processes of `/proc` whose group (field 5 of `stat`) is `pgid`.
#[cfg(target_os = "linux")]
pub(crate) fn group_members(group: u32) -> io::Result<Vec<u32>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        // The process may exit between the listing and the read.
        let Ok(text) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some((state, found)) = parse_stat_group(&text)
            && found == group
            && !matches!(state, 'Z' | 'X' | 'x')
        {
            members.push(pid);
        }
    }
    Ok(members)
}

/// The state (field 3) and the process group (field 5) of a `/proc/<pid>/stat` line, counted
/// from the command name's last `)` as in [`parse_stat`].
#[cfg(any(target_os = "linux", test))]
fn parse_stat_group(line: &str) -> Option<(char, u32)> {
    let rest = &line[line.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    // Field 4, the parent, lies between the state and the group.
    let group = fields.nth(1)?.parse().ok()?;
    Some((state, group))
}

/// macOS: every pid of `proc_listallpids` whose BSD info names `pgid` as its group. Weaker
/// than Linux: the listing is a snapshot sized by a first call, and a process the user cannot
/// inspect is skipped.
#[cfg(target_os = "macos")]
pub(crate) fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    // SAFETY: a null buffer of size zero asks for the number of pids only.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
    // Room for processes started since the first call.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let bytes = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::pid_t>())
        .map_err(|_| io::Error::other("too many processes"))?;
    // SAFETY: the buffer is `pids`, writable, and `bytes` is its length in bytes.
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let listed = usize::try_from(listed).map_err(|_| io::Error::last_os_error())?;
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .expect("proc_bsdinfo is small");
    let mut members = Vec::new();
    for &pid in &pids[..listed.min(pids.len())] {
        if pid <= 0 {
            continue;
        }
        // SAFETY: `proc_bsdinfo` is plain C data; all zeros is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer is `info`, writable, and `size` is its length.
        let written = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if written == size && info.pbi_pgid == pgid && info.pbi_status != libc::SZOMB {
            members.extend(u32::try_from(pid).ok());
        }
    }
    Ok(members)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn group_members(_pgid: u32) -> io::Result<Vec<u32>> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
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

/// A pipe whose two ends are closed on `exec`.
fn pipe() -> io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds: [libc::c_int; 2] = [-1; 2];
    // SAFETY: `fds` is a writable array of two descriptors, as `pipe` requires.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` just opened both descriptors and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is live and owned above; `F_SETFD` takes an integer flag.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// Spawns `command` as the leader of a new process group (on Linux with the parent-death
/// signal, as [`spawn_contained`]), calls `before_run` with its pid and start time, and only
/// then lets it `exec`: the program runs no instruction before `before_run` has returned `Ok`.
/// When it fails, the child exits before `exec` and the spawn fails.
///
/// Unix has no suspended start, so the forked child itself holds the gate: before `exec` it
/// writes its pid to one pipe and blocks reading a byte from another. A second thread reads the
/// pid, calls `before_run` and writes the byte (or closes the pipe, which fails the `exec`),
/// while this thread blocks in `spawn`, which waits for the `exec`. The spawn happens on the
/// calling thread, the one the parent-death signal is tied to. `command` is spent: the gate's
/// descriptors are closed when this returns.
pub(crate) fn spawn_gated(
    command: &mut std::process::Command,
    before_run: impl FnOnce(u32, u64) -> io::Result<()> + Send,
) -> io::Result<(std::process::Child, Container)> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    // The child reports its pid on `report`; the shim opens `gate` once the pid is recorded.
    let (report_read, report_write) = pipe()?;
    let (gate_read, gate_write) = pipe()?;
    let (report_fd, gate_read_fd, gate_write_fd) = (
        report_write.as_raw_fd(),
        gate_read.as_raw_fd(),
        gate_write.as_raw_fd(),
    );
    command.process_group(0);
    #[cfg(target_os = "linux")]
    let parent = pid_t(std::process::id())?;
    // SAFETY: the closure runs in the forked child before `exec` and calls only `prctl`,
    // `getppid`, `getpid`, `close`, `write` and `read`, all async-signal-safe; it allocates
    // nothing and takes no lock. The descriptors it names are open in the child: they are
    // owned by this function until `spawn` has returned.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            // The child's copy of the gate's write end: without it, the gate reads end of file
            // when the shim closes its own, or dies.
            libc::close(gate_write_fd);
            let pid = libc::getpid().to_ne_bytes();
            let written = libc::write(report_fd, pid.as_ptr().cast(), pid.len());
            if usize::try_from(written).ok() != Some(pid.len()) {
                return Err(io::Error::last_os_error());
            }
            libc::close(report_fd);
            let mut go = 0_u8;
            loop {
                match libc::read(gate_read_fd, (&raw mut go).cast(), 1) {
                    1 => return Ok(()),
                    -1 if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => {}
                    _ => return Err(io::Error::from_raw_os_error(libc::ECANCELED)),
                }
            }
        });
    }
    let (spawned, gated) = std::thread::scope(|scope| {
        let gate = scope.spawn(move || -> io::Result<()> {
            let mut report = std::fs::File::from(report_read);
            let mut pid = [0_u8; std::mem::size_of::<libc::pid_t>()];
            // End of file when the fork failed: `report_write` is dropped below.
            report.read_exact(&mut pid)?;
            let pid = u32::try_from(libc::pid_t::from_ne_bytes(pid))
                .map_err(|_| io::Error::other("a negative pid"))?;
            let start =
                read_start(pid, true)?.ok_or_else(|| io::Error::other("the child has no start"))?;
            before_run(pid, start)?;
            std::fs::File::from(gate_write).write_all(&[1])
        });
        let spawned = command.spawn();
        drop(report_write);
        let gated = gate
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the gate thread panicked")));
        (spawned, gated)
    });
    drop(gate_read);
    let mut child = spawned?;
    if let Err(e) = gated {
        child.kill().ok();
        child.wait().ok();
        return Err(e);
    }
    let pgid = pid_t(child.id())?;
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

/// `statvfs`: `f_bavail * f_frsize`, the bytes available to an unprivileged user on the
/// volume that holds `path`.
pub(crate) fn free_space(path: &std::path::Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `statvfs` is plain old data, so all zeroes is a valid value.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and outlives the call; `stat` is a live, writable
    // `statvfs`.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // The field types differ across platforms (32-bit on macOS, 64-bit on Linux).
    #[allow(clippy::useless_conversion, reason = "u64 already on some targets")]
    let (blocks, size) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
    Ok(blocks.saturating_mul(size))
}

#[cfg(test)]
mod tests {
    use super::{parse_stat, parse_stat_group};

    #[test]
    fn the_group_is_field_five_counted_from_the_last_parenthesis() {
        let stat = "4242 (a (b) c) S 1 4200 4242 0\n";
        assert_eq!(parse_stat_group(stat), Some(('S', 4200)));
        assert_eq!(parse_stat_group("4242 (x) S 1"), None);
    }

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        let stat = "4242 (a (b) c) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                    987654 1000 200 18446744073709551615\n";
        assert_eq!(parse_stat(stat), Some(('S', 987_654)));
        assert_eq!(parse_stat("4242 (x"), None);
    }
}
