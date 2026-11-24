//! Windows: Job Objects, process times and power requests through `windows-sys`.

use std::io;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
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
    CREATE_NO_WINDOW, CREATE_SUSPENDED, GetProcessTimes, OpenProcess, OpenThread,
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, REASON_CONTEXT, REASON_CONTEXT_0, ResumeThread,
    THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
};

/// `POWER_REQUEST_CONTEXT_VERSION` of `minwinbase.h`; `windows-sys` does not export it.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// The exit code of a process bosun kills.
const KILLED: u32 = 1;

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
        if !cfg!(mutant_proc_no_kill_on_close) {
            // SAFETY: the structure is plain C data (integers and nested integer structures);
            // all zeros is a valid value, meaning "no limits".
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // Mutant (`mutant_proc_allow_breakaway`): the job lets its processes break away, so a
            // grandchild that asks to leave it (CREATE_BREAKAWAY_FROM_JOB) leaves it.
            if cfg!(mutant_proc_allow_breakaway) {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            }
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

    /// The pids of the processes in the job that have not exited.
    pub(crate) fn members(&self) -> io::Result<Vec<u32>> {
        // The list is the header followed by the pids. When the buffer is too short the call
        // fails with `ERROR_MORE_DATA`, and the header still says how many processes there are.
        let mut slots = 16_usize;
        loop {
            // Words rather than bytes, so the buffer is aligned for the header. The header
            // holds one pid slot already, so `slots` pids take `slots` words after its start.
            let bytes = mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList)
                + slots * mem::size_of::<usize>();
            let mut buf = vec![0_usize; bytes.div_ceil(mem::size_of::<usize>())];
            let capacity = u32::try_from(bytes).expect("a small buffer");
            let mut returned = 0_u32;
            // SAFETY: `buf` is writable for `capacity` bytes, and `returned` is a writable u32;
            // the information class is the one whose structure the buffer starts with.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    capacity,
                    &raw mut returned,
                )
            };
            // SAFETY: `buf` holds at least the header and is aligned for it (a `usize` is at
            // least as aligned as its `u32` fields).
            let header = unsafe { &*buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            let (assigned, listed) = (
                header.NumberOfAssignedProcesses as usize,
                header.NumberOfProcessIdsInList as usize,
            );
            if ok == 0 {
                let e = io::Error::last_os_error();
                #[expect(clippy::cast_possible_wrap, reason = "Win32 error codes fit an i32")]
                if e.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                    return Err(e);
                }
                slots = assigned.max(slots * 2);
                continue;
            }
            if listed < assigned || listed > slots {
                // Processes joined the job between the two calls, or the pids overrun the buffer.
                slots = assigned.max(slots * 2);
                continue;
            }
            // The two u32 counts fill exactly one word, so the pids start at word 1 of `buf`.
            let first_word = mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList)
                / mem::size_of::<usize>();
            let first = buf.as_ptr().wrapping_add(first_word);
            // SAFETY: `buf` holds `slots + 1` words and `listed` ≤ `slots` (checked above), so
            // the `listed` pids from word 1 lie inside `buf`; they are initialised by the call.
            let ids = unsafe { std::slice::from_raw_parts(first, listed) };
            return ids
                .iter()
                .map(|&id| {
                    u32::try_from(id).map_err(|_| io::Error::other("a pid of a job is large"))
                })
                .collect();
        }
    }
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

pub(crate) fn start_time(pid: u32) -> io::Result<Option<u64>> {
    match open_live(pid, PROCESS_QUERY_LIMITED_INFORMATION)? {
        Some(process) => creation_time(process.as_raw_handle()).map(Some),
        None => Ok(None),
    }
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

/// `GetDiskFreeSpaceExW` of the volume holding `path`: the bytes available to the caller. The
/// path must exist, because the call names a volume from any path, existing or not.
pub(crate) fn free_space(path: &Path) -> io::Result<u64> {
    std::fs::metadata(path)?;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = 0_u64;
    // SAFETY: `wide` is NUL-terminated and lives across the call; `available` is a writable
    // u64; the total counts are optional and passed as null.
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
