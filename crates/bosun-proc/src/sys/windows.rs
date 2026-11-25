//! Windows: Job Objects, process times and power requests through `windows-sys`.

use std::io;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, GetCurrentProcess, GetProcessTimes, OpenProcess,
    OpenThread, POWER_REQUEST_CONTEXT_SIMPLE_STRING, PROCESS_ACCESS_RIGHTS, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, REASON_CONTEXT,
    REASON_CONTEXT_0, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
};

/// `POWER_REQUEST_CONTEXT_VERSION` of `minwinbase.h`; `windows-sys` does not export it.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// The exit code of a process bosun kills.
const KILLED: u32 = 1;

/// `JOB_OBJECT_QUERY` of `winnt.h`: the right to query a job. `windows-sys` has it under
/// `Win32_System_SystemServices`, a large feature for one constant.
const JOB_OBJECT_QUERY: u32 = 4;

/// Takes ownership of a handle a call just returned; `None` for a null one.
fn owned(raw: HANDLE) -> Option<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: `raw` is a valid handle the call just opened for us; nothing else owns it, so the
    // `OwnedHandle` may close it exactly once.
    Some(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn size_of_u32<T>() -> u32 {
    u32::try_from(mem::size_of::<T>()).expect("a Win32 structure is far below 4 GiB")
}

/// A Job Object that kills its processes when its last handle closes.
#[derive(Debug)]
pub(crate) struct Container {
    job: OwnedHandle,
}

impl Container {
    fn new() -> io::Result<Self> {
        // SAFETY: null attributes and a null name create an anonymous job with the default
        // security descriptor; the call has no other preconditions.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let job = owned(raw).ok_or_else(io::Error::last_os_error)?;
        // Breakaway is never allowed: without `JOB_OBJECT_LIMIT_BREAKAWAY_OK` a process that
        // asks for `CREATE_BREAKAWAY_FROM_JOB` is refused, so nothing leaves the job.
        let mut flags = 0;
        if !cfg!(mutant_proc_no_kill_on_close) {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if cfg!(mutant_proc_allow_breakaway) {
            // Mutant (`mutant_proc_allow_breakaway`): a grandchild may break away and outlive
            // the tree.
            flags |= JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        }
        if flags != 0 {
            // SAFETY: the structure is plain C data (integers and nested integer structures);
            // all zeros is a valid value, meaning "no limits".
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = flags;
            // SAFETY: `job` is a live job handle with all access; the pointer and length
            // describe `info`, which outlives the call, and match the information class.
            let ok = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { job })
    }

    fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: both handles are live: the job is owned by `self`, the process handle by the
        // child that the caller holds for the duration of the call.
        let ok = unsafe { AssignProcessToJobObject(self.job.as_raw_handle(), process) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminates every process in the job.
    pub(crate) fn kill_all(&self) -> io::Result<()> {
        // SAFETY: the job handle is live and owned by `self`.
        let ok = unsafe { TerminateJobObject(self.job.as_raw_handle(), KILLED) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// [`Container::kill_all`], best effort. The handle's close kills the rest regardless.
    pub(crate) fn kill(&self) {
        self.kill_all().ok();
    }

    /// The pids of the processes in the job (`JobObjectBasicProcessIdList`). The list is read
    /// into a buffer that grows while processes join between reads, at most [`MEMBER_READS`]
    /// times: a job that keeps outgrowing it is an error, never a hang.
    pub(crate) fn members(&self) -> io::Result<Vec<u32>> {
        job_members(self.job.as_raw_handle())
    }
}

/// How many times a job's id list is read before [`Container::members`] gives up. Each retry
/// makes room for at least twice the processes the last read counted, so only a job that more
/// than doubles between two reads, again and again, exhausts it.
const MEMBER_READS: usize = 8;

/// Calls `read` with a growing capacity until it answers `Ok(Ok(list))`, at most `reads` times.
/// `Ok(Err(counted))` asks for another read: the capacity becomes at least twice `counted`.
///
/// # Errors
///
/// The first error of `read`, or when `reads` reads all asked for more room.
fn read_growing<T>(
    reads: usize,
    mut read: impl FnMut(usize) -> io::Result<Result<T, usize>>,
) -> io::Result<T> {
    let mut capacity = 16;
    for _ in 0..reads {
        match read(capacity)? {
            Ok(list) => return Ok(list),
            Err(counted) => capacity = counted.max(capacity) * 2,
        }
    }
    Err(io::Error::other(format!(
        "the job's process list kept growing over {reads} reads"
    )))
}

/// The pids of the processes in `job`, a live job handle with at least `JOB_OBJECT_QUERY`.
fn job_members(job: RawHandle) -> io::Result<Vec<u32>> {
    let header = mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
    let word = mem::size_of::<usize>();
    read_growing(MEMBER_READS, |capacity| {
        // A `usize` buffer, so the list and its id array are aligned.
        let mut buffer = vec![0_usize; header / word + capacity];
        let bytes = u32::try_from(buffer.len() * word)
            .map_err(|_| io::Error::other("too many processes in the job"))?;
        // SAFETY: the caller holds `job` live with `JOB_OBJECT_QUERY`; the pointer and length
        // describe `buffer`, writable, aligned for the structure and at least as large as its
        // header; the return length may be null.
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                bytes,
                ptr::null_mut(),
            )
        };
        let mut more = false;
        if ok == 0 {
            let e = io::Error::last_os_error();
            #[expect(clippy::cast_possible_wrap, reason = "Win32 error codes fit an i32")]
            if e.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(e);
            }
            more = true;
        }
        let list: *const JOBOBJECT_BASIC_PROCESS_ID_LIST = buffer.as_ptr().cast();
        // SAFETY: `list` points to the start of `buffer`, aligned and large enough for the
        // header, which the call filled in.
        let (assigned, listed) = unsafe {
            (
                (*list).NumberOfAssignedProcesses,
                (*list).NumberOfProcessIdsInList,
            )
        };
        let (assigned, listed) = (assigned as usize, listed as usize);
        if more || listed < assigned {
            // Processes joined between the calls, or the buffer was short: grow and retry.
            return Ok(Err(assigned));
        }
        // The id array is an array of `usize`, so its offset is a whole number of words.
        let ids = buffer
            .get(header / word..)
            .and_then(|ids| ids.get(..listed))
            .ok_or_else(|| io::Error::other("the job listed more ids than fit"))?;
        Ok(Ok(ids
            .iter()
            .filter_map(|&id| u32::try_from(id).ok())
            .collect()))
    })
}

/// Resumes every thread of the suspended process `pid`: just the main one, for a process
/// created with `CREATE_SUSPENDED`.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: a thread snapshot of the whole system; the flags are valid and the process id is
    // ignored for `TH32CS_SNAPTHREAD`.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    let snapshot = owned(raw).ok_or_else(io::Error::last_os_error)?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of_u32::<THREADENTRY32>(),
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut resumed = 0;
    // SAFETY: the snapshot handle is live; `entry` is writable and its `dwSize` is set, as the
    // call requires.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening a thread by id has no preconditions; the result is checked.
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            let thread = owned(raw).ok_or_else(io::Error::last_os_error)?;
            // SAFETY: the thread handle is live and was opened with `THREAD_SUSPEND_RESUME`.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("the suspended child has no thread"));
    }
    Ok(())
}

/// Spawns `command` suspended, puts it in a new job, then resumes it: the child runs no
/// instruction outside the job, so nothing it starts escapes. No console window is created.
pub(crate) fn spawn_contained(
    command: &mut tokio::process::Command,
) -> io::Result<(tokio::process::Child, Container)> {
    let container = Container::new()?;
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let started = match (child.raw_handle(), child.id()) {
        (Some(handle), Some(pid)) => container.assign(handle).and_then(|()| resume(pid)),
        _ => Err(io::Error::other("a fresh child has no handle")),
    };
    if let Err(e) = started {
        child.start_kill().ok();
        return Err(e);
    }
    Ok((child, container))
}

/// Spawns `command` suspended in a new job, calls `before_run` with its pid, its start time and
/// the value of this process's handle to the job ([`job_members_of`] lists the job through
/// it), and only then resumes it: the child runs no instruction before `before_run` has
/// returned `Ok`. When it fails, the child is killed and the error returned. No console window
/// is created.
pub(crate) fn spawn_gated(
    command: &mut std::process::Command,
    before_run: impl FnOnce(u32, u64, Option<u64>) -> io::Result<()>,
) -> io::Result<(std::process::Child, Container)> {
    use std::os::windows::process::CommandExt;

    let container = Container::new()?;
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let pid = child.id();
    // A handle is an opaque value of this process; it is recorded, never dereferenced here.
    let job = container.job.as_raw_handle() as usize as u64;
    let started = container
        .assign(child.as_raw_handle())
        .and_then(|()| creation_time(child.as_raw_handle()))
        .and_then(|start| before_run(pid, start, Some(job)))
        .and_then(|()| resume(pid));
    if let Err(e) = started {
        child.kill().ok();
        child.wait().ok();
        return Err(e);
    }
    Ok((child, container))
}

fn creation_time(process: RawHandle) -> io::Result<u64> {
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    // SAFETY: the handle is live and has at least `PROCESS_QUERY_LIMITED_INFORMATION`; the four
    // pointers are to writable locals.
    let ok = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// The start time of a child this process spawned and still holds.
pub(crate) fn child_start_time(child: &tokio::process::Child) -> io::Result<u64> {
    let handle = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("the child has exited"))?;
    creation_time(handle)
}

/// Opens the live process `pid`; `None` when there is none, or it has exited, or it belongs to
/// someone the user cannot open (then it is not one bosun started).
fn open_live(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: opening a process by id has no preconditions; the result is checked.
    let raw = unsafe { OpenProcess(access | PROCESS_SYNCHRONIZE, 0, pid) };
    let Some(process) = owned(raw) else {
        let e = io::Error::last_os_error();
        #[expect(clippy::cast_possible_wrap, reason = "Win32 error codes fit an i32")]
        return match e.raw_os_error() {
            Some(code)
                if code == ERROR_INVALID_PARAMETER as i32 || code == ERROR_ACCESS_DENIED as i32 =>
            {
                Ok(None)
            }
            _ => Err(e),
        };
    };
    // SAFETY: the handle is live and was opened with `SYNCHRONIZE`; a zero timeout only polls.
    let exited = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0;
    Ok((!exited).then_some(process))
}

/// The members of the job that the live process `holder` (pid and start time) holds under the
/// handle value `job`, as [`spawn_gated`] reports it: the handle is duplicated into this
/// process with query access only, the job listed, and the duplicate closed. `None` when no
/// process with that pid and start time runs: the holder is gone, and its job died with it.
///
/// While the duplicate is open the job has a second handle, so a holder that dies in that
/// window takes its container down only when the duplicate closes, microseconds later.
pub(crate) fn job_members_of(holder: u32, start: u64, job: u64) -> io::Result<Option<Vec<u32>>> {
    let Some(process) = open_live(
        holder,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE,
    )?
    else {
        return Ok(None);
    };
    // The start time is read through the handle that duplicates: the pid cannot change hands
    // in between.
    if creation_time(process.as_raw_handle())? != start {
        return Ok(None);
    }
    let source =
        usize::try_from(job).map_err(|_| io::Error::other("a job handle out of range"))? as HANDLE;
    let mut duplicate: HANDLE = ptr::null_mut();
    // SAFETY: `process` is live and was opened with `PROCESS_DUP_HANDLE`; `source` is a value
    // in that process's handle table, which the call validates (a stale or foreign value fails
    // or yields a handle the job query refuses); the current-process pseudo handle needs no
    // closing; `duplicate` is a writable local.
    let ok = unsafe {
        DuplicateHandle(
            process.as_raw_handle(),
            source,
            GetCurrentProcess(),
            &raw mut duplicate,
            JOB_OBJECT_QUERY,
            0,
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let duplicate = owned(duplicate).ok_or_else(|| io::Error::other("a null duplicate"))?;
    job_members(duplicate.as_raw_handle()).map(Some)
}

pub(crate) fn start_time(pid: u32) -> io::Result<Option<u64>> {
    match open_live(pid, PROCESS_QUERY_LIMITED_INFORMATION)? {
        Some(process) => creation_time(process.as_raw_handle()).map(Some),
        None => Ok(None),
    }
}

/// 100 ns intervals between 1601-01-01 and the Unix epoch.
const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;

/// A creation time (100 ns since 1601) as Unix milliseconds, rounded down.
pub(crate) fn start_unix_ms(start: u64) -> i64 {
    let start = i64::try_from(start).unwrap_or(i64::MAX);
    (start - UNIX_EPOCH_AS_FILETIME).div_euclid(10_000)
}

/// The live processes of the current user whose executable's file name is `name`, from a
/// Toolhelp process snapshot, each with its creation time read through a handle that is
/// checked to belong to the user. A process that cannot be opened, or whose token cannot be
/// read, is skipped.
pub(crate) fn processes_named(name: &str) -> io::Result<Vec<(u32, u64)>> {
    use std::os::windows::ffi::OsStringExt;

    // SAFETY: a process snapshot of the whole system; the flags are valid and the process id
    // is ignored for `TH32CS_SNAPPROCESS`.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let snapshot = owned(raw).ok_or_else(io::Error::last_os_error)?;
    let user = super::current_user_sid()?;
    // SAFETY: `PROCESSENTRY32W` is plain C data; all zeros is a valid value.
    let mut entry: PROCESSENTRY32W = unsafe { mem::zeroed() };
    entry.dwSize = size_of_u32::<PROCESSENTRY32W>();
    let mut found = Vec::new();
    // SAFETY: the snapshot handle is live; `entry` is writable and its `dwSize` is set, as the
    // call requires.
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let exe = std::ffi::OsString::from_wide(&entry.szExeFile[..len]);
        let pid = entry.th32ProcessID;
        if pid != 0
            && exe
                .to_str()
                .is_some_and(|exe| crate::named::file_name_is(exe, name))
        {
            // A process that exited, or that the user cannot open, is not the user's stray.
            if let Ok(Some(process)) = open_live(pid, PROCESS_QUERY_LIMITED_INFORMATION)
                && super::process_user_sid(process.as_raw_handle()).is_ok_and(|sid| sid == user)
                && let Ok(start) = creation_time(process.as_raw_handle())
            {
                found.push((pid, start));
            }
        }
        // SAFETY: as for `Process32FirstW`.
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    Ok(found)
}

pub(crate) fn reap(pid: u32, start: u64) -> io::Result<bool> {
    let Some(process) = open_live(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE)?
    else {
        return Ok(false);
    };
    // The start time is read through the handle that will kill: the pid cannot change hands
    // in between.
    if !cfg!(mutant_proc_skip_start_time) && creation_time(process.as_raw_handle())? != start {
        return Ok(false);
    }
    // SAFETY: the handle is live and was opened with `PROCESS_TERMINATE`.
    if unsafe { TerminateProcess(process.as_raw_handle(), KILLED) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// A `PowerRequestSystemRequired` request, set until the value is dropped.
#[derive(Debug)]
pub(crate) struct PowerRequest {
    request: OwnedHandle,
}

impl PowerRequest {
    pub(crate) fn acquire(reason: &str) -> io::Result<Self> {
        let mut wide: Vec<u16> = std::ffi::OsStr::new(reason).encode_wide().collect();
        wide.push(0);
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: wide.as_mut_ptr(),
            },
        };
        // SAFETY: `context` is a valid simple-string reason context whose string is
        // NUL-terminated and outlives the call (the system copies it).
        let raw = unsafe { PowerCreateRequest(&raw const context) };
        let request = owned(raw).ok_or_else(io::Error::last_os_error)?;
        // SAFETY: the request handle is live; the request type is a valid constant.
        if unsafe { PowerSetRequest(request.as_raw_handle(), PowerRequestSystemRequired) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { request })
    }
}

impl Drop for PowerRequest {
    fn drop(&mut self) {
        // SAFETY: the request handle is live and the request was set in `acquire`. Closing the
        // handle afterwards (the `OwnedHandle` drop) would clear it as well.
        unsafe { PowerClearRequest(self.request.as_raw_handle(), PowerRequestSystemRequired) };
    }
}

/// `GetDiskFreeSpaceExW`: the bytes available to the calling user (quotas included) on the
/// volume that holds `path`.
pub(crate) fn free_space(path: &std::path::Path) -> io::Result<u64> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    wide.push(0);
    let mut available: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the one out pointer is a live
    // `u64`, and the two optional ones are null, which the function accepts.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_creation_time_is_unix_milliseconds_rounded_down() {
        assert_eq!(start_unix_ms(116_444_736_000_000_000), 0);
        assert_eq!(start_unix_ms(116_444_736_000_019_999), 1);
        assert_eq!(start_unix_ms(116_444_735_999_999_999), -1);
    }

    /// A job that outgrows every buffer is an error after the bound, not an endless loop.
    #[test]
    fn a_list_that_keeps_growing_is_read_a_bounded_number_of_times() {
        let mut capacities = Vec::new();
        let answer = read_growing::<()>(MEMBER_READS, |capacity| {
            capacities.push(capacity);
            // Always one more process than there was room for.
            Ok(Err(capacity + 1))
        });
        assert!(answer.is_err(), "{answer:?}");
        assert_eq!(capacities.len(), MEMBER_READS);
        assert!(
            capacities.windows(2).all(|w| w[1] >= w[0] * 2),
            "{capacities:?}"
        );
    }

    /// A list that settles on a later read is returned; an error of a read ends the reading.
    #[test]
    fn a_list_that_settles_is_returned_and_an_error_is_passed_on() {
        let mut reads = 0;
        let answer = read_growing(MEMBER_READS, |capacity| {
            reads += 1;
            Ok(if reads < 3 { Err(capacity) } else { Ok(reads) })
        });
        assert_eq!(answer.expect("settled"), 3);

        let mut reads = 0;
        let answer = read_growing::<()>(MEMBER_READS, |_| {
            reads += 1;
            Err(io::Error::other("refused"))
        });
        assert!(answer.is_err());
        assert_eq!(reads, 1);
    }
}
