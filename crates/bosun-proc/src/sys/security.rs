//! Windows: the owner-only security descriptor of the local endpoint, and reading a handle's
//! DACL back, through `windows-sys`.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, GENERIC_WRITE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::pipe::{Ace, AceKind};

/// `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE` of `winnt.h`.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;

/// Memory the system allocated with `LocalAlloc`, freed once on drop.
struct Local(HLOCAL);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a call documented to allocate with `LocalAlloc`,
            // and nothing else frees it.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// The string form (`S-1-5-…`) of `sid`.
///
/// # Safety
///
/// `sid` points to a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid (the caller's promise); the call allocates `text` with
    // `LocalAlloc`, which `Local` frees.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let text = Local(text.cast());
    let start: *const u16 = text.0.cast();
    let mut len = 0;
    // SAFETY: the system returned a NUL-terminated string; reading up to the NUL stays inside
    // its allocation.
    while unsafe { *start.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` elements from `start` are initialised, as counted above.
    let units = unsafe { std::slice::from_raw_parts(start, len) };
    OsString::from_wide(units)
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a SID that is not Unicode"))
}

/// The SID of the user this process runs as, in string form.
pub(crate) fn current_user_sid() -> io::Result<String> {
    // SAFETY: the pseudo handle of the current process is always live and needs no closing.
    process_user_sid(unsafe { GetCurrentProcess() })
}

/// The SID of the user the process `process` runs as, in string form. `process` is a live
/// handle with at least `PROCESS_QUERY_LIMITED_INFORMATION`; the token of another user's
/// process is usually refused (an error).
pub(crate) fn process_user_sid(process: RawHandle) -> io::Result<String> {
    let mut raw = ptr::null_mut();
    // SAFETY: the caller holds `process` live with query access; `raw` receives a new token
    // handle that `OwnedHandle` closes.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call just opened `raw` for us and nothing else owns it.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut size = 0u32;
    // SAFETY: a null buffer of length 0 asks for the size only; the call fails with
    // `ERROR_INSUFFICIENT_BUFFER` and writes `size`.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &raw mut size,
        )
    };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    // `u64` elements keep the buffer aligned for `TOKEN_USER` (pointer-aligned).
    let mut buffer = vec![0u64; (size as usize).div_ceil(mem::size_of::<u64>())];
    // SAFETY: `buffer` holds at least `size` writable bytes, as the call asked for.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the buffer starts with a `TOKEN_USER`, suitably aligned (see above),
    // whose SID points into the same buffer, which lives until the end of this function.
    unsafe {
        let user: *const TOKEN_USER = buffer.as_ptr().cast();
        sid_string((*user).User.Sid)
    }
}

/// A self-relative security descriptor built from SDDL; freed on drop.
pub(crate) struct Descriptor(Local);

// SAFETY: the descriptor is plain memory the process owns; after construction nothing writes
// it, and the system only reads it during the calls it is passed to.
unsafe impl Send for Descriptor {}
// SAFETY: as above: shared references only ever read it.
unsafe impl Sync for Descriptor {}

impl Descriptor {
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(OsStr::new(sddl));
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `text` is NUL-terminated and outlives the call; the call allocates `sd` with
        // `LocalAlloc`, which `Local` frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut sd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Local(sd)))
    }

    /// Creates a pipe instance at `addr` with `options` and this descriptor.
    pub(crate) fn create_pipe(
        &self,
        options: &ServerOptions,
        addr: &OsStr,
    ) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(mem::size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES is a few bytes"),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` is a valid `SECURITY_ATTRIBUTES` whose descriptor is a valid
        // self-relative descriptor; both outlive the call, and the system copies the descriptor
        // into the pipe object.
        unsafe {
            options
                .create_with_security_attributes_raw(addr, (&raw mut attributes).cast::<c_void>())
        }
    }
}

impl Descriptor {
    /// Creates the new file `path` for writing with this descriptor; fails if it exists.
    pub(crate) fn create_file(&self, path: &Path) -> io::Result<File> {
        let name = wide(path.as_os_str());
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(mem::size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES is a few bytes"),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        };
        // SAFETY: `name` is NUL-terminated, `attributes` is a valid `SECURITY_ATTRIBUTES` whose
        // descriptor is a valid self-relative descriptor; all outlive the call. No template.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE | READ_CONTROL,
                0,
                &raw const attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a valid file handle this call just opened and nothing else owns.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

/// The entries of the DACL of the kernel object behind `handle`.
pub(crate) fn dacl(handle: RawHandle) -> io::Result<Vec<Ace>> {
    let mut list: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `handle` is a live handle the caller holds; the out pointers are valid. On
    // success `sd` is allocated with `LocalAlloc` (freed by `Local`) and `list` points into it.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut list,
            ptr::null_mut(),
            &raw mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).unwrap_or(i32::MAX),
        ));
    }
    let _sd = Local(sd);
    if list.is_null() {
        // A null DACL admits everyone: report it as one allow entry for Everyone.
        return Ok(vec![Ace {
            kind: AceKind::Allow,
            sid: "S-1-1-0".to_owned(),
            mask: u32::MAX,
        }]);
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `list` is a valid ACL inside `sd`; `info` is a writable buffer of the given size.
    let ok = unsafe {
        GetAclInformation(
            list,
            (&raw mut info).cast(),
            u32::try_from(mem::size_of::<ACL_SIZE_INFORMATION>()).expect("a few bytes"),
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut aces = Vec::new();
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACL's entry count; `ace` receives a pointer into it.
        if unsafe { GetAce(list, index, &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let kind = match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE => AceKind::Allow,
            ACCESS_DENIED_ACE_TYPE => AceKind::Deny,
            other => AceKind::Other(other),
        };
        let (sid, mask) = if matches!(kind, AceKind::Other(_)) {
            (String::new(), 0)
        } else {
            // SAFETY: allowed and denied ACEs share `ACCESS_ALLOWED_ACE`'s layout: the mask,
            // then the SID starting at `SidStart`, inside the ACE.
            unsafe {
                let typed = ace.cast::<ACCESS_ALLOWED_ACE>();
                let sid = (&raw mut (*typed).SidStart).cast::<c_void>();
                (sid_string(sid)?, (*typed).Mask)
            }
        };
        aces.push(Ace { kind, sid, mask });
    }
    Ok(aces)
}
