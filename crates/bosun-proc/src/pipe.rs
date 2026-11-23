//! The security descriptor of the local endpoint on Windows (#4, #71, D2 of #7): a named pipe
//! whose DACL admits only the user the daemon runs as.
//!
//! [`OwnerOnly`] is the descriptor `D:P(A;;GA;;;<user SID>)`: a protected DACL (no inherited
//! entries) with one entry, generic all for the current process's token user. SYSTEM, the
//! Administrators group and other users get nothing. Remote clients are refused by the pipe's
//! own flag (`PIPE_REJECT_REMOTE_CLIENTS`), which the caller sets on its `ServerOptions`.
//! [`dacl`] reads a handle's DACL back, for tests and for checks.

use std::ffi::OsStr;
use std::io;
use std::os::windows::io::AsRawHandle;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::sys;

/// One entry of a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    /// Allow, deny or another type.
    pub kind: AceKind,
    /// The trustee's SID in string form (`S-1-5-…`); empty for [`AceKind::Other`].
    pub sid: String,
    /// The access mask, after the system mapped generic rights; 0 for [`AceKind::Other`].
    pub mask: u32,
}

/// The type of an [`Ace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceKind {
    /// `ACCESS_ALLOWED_ACE_TYPE`.
    Allow,
    /// `ACCESS_DENIED_ACE_TYPE`.
    Deny,
    /// Any other type (audit, object, callback entries), by its number.
    Other(u8),
}

/// The SID of the user this process runs as, in string form.
///
/// # Errors
///
/// When the process token cannot be queried.
pub fn current_user_sid() -> io::Result<String> {
    sys::current_user_sid()
}

/// A security descriptor that admits only the current user.
pub struct OwnerOnly {
    sid: String,
    descriptor: sys::Descriptor,
}

impl std::fmt::Debug for OwnerOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerOnly")
            .field("sddl", &self.sddl())
            .finish()
    }
}

impl OwnerOnly {
    /// The descriptor for the user this process runs as.
    ///
    /// # Errors
    ///
    /// When the process token cannot be queried or the descriptor cannot be built.
    pub fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let descriptor = sys::Descriptor::from_sddl(&sddl(&sid))?;
        Ok(Self { sid, descriptor })
    }

    /// The SID the descriptor admits.
    #[must_use]
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// The descriptor in SDDL.
    #[must_use]
    pub fn sddl(&self) -> String {
        sddl(&self.sid)
    }

    /// Creates a pipe instance at `addr` with `options` and this descriptor.
    ///
    /// # Errors
    ///
    /// What `CreateNamedPipeW` fails with: `PermissionDenied` when `options` asks for the first
    /// instance and one exists.
    pub fn create_pipe(
        &self,
        options: &ServerOptions,
        addr: impl AsRef<OsStr>,
    ) -> io::Result<NamedPipeServer> {
        self.descriptor.create_pipe(options, addr.as_ref())
    }

    /// Checks that the DACL of `handle` is this descriptor's: one allow entry, for [`sid`].
    ///
    /// # Errors
    ///
    /// When the DACL cannot be read, or holds anything else (an error of kind `Other` that
    /// lists the entries).
    ///
    /// [`sid`]: OwnerOnly::sid
    pub fn verify(&self, handle: &impl AsRawHandle) -> io::Result<()> {
        let aces = dacl(handle)?;
        match aces.as_slice() {
            [only] if only.kind == AceKind::Allow && only.sid == self.sid => Ok(()),
            _ => Err(io::Error::other(format!(
                "the DACL admits more than {}: {aces:?}",
                self.sid
            ))),
        }
    }
}

fn sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

/// The entries of the DACL of the kernel object behind `handle` (a pipe, a file, a process).
///
/// # Errors
///
/// When the handle lacks `READ_CONTROL` or the descriptor cannot be read.
pub fn dacl(handle: &impl AsRawHandle) -> io::Result<Vec<Ace>> {
    sys::dacl(handle.as_raw_handle())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!(r"\\.\pipe\bosun-proc-test-{}-{nanos}", std::process::id())
    }

    #[tokio::test]
    async fn an_owner_only_pipe_has_one_allow_entry_for_the_user() {
        let owner = OwnerOnly::new().unwrap();
        assert!(owner.sid().starts_with("S-1-5-"), "{owner:?}");
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(true)
            .reject_remote_clients(true);
        let pipe = owner.create_pipe(&options, unique()).unwrap();
        let aces = dacl(&pipe).unwrap();
        assert_eq!(aces.len(), 1, "{aces:?}");
        assert_eq!(aces[0].kind, AceKind::Allow, "{aces:?}");
        assert_eq!(aces[0].sid, owner.sid(), "{aces:?}");
        assert_ne!(aces[0].mask, 0, "{aces:?}");
        owner.verify(&pipe).unwrap();
    }

    #[tokio::test]
    async fn a_default_pipe_admits_more_than_the_user() {
        // What the descriptor replaces: the default DACL also admits SYSTEM and Administrators.
        let pipe = ServerOptions::new().create(unique()).unwrap();
        let aces = dacl(&pipe).unwrap();
        let user = current_user_sid().unwrap();
        assert!(aces.iter().any(|a| a.sid != user), "{aces:?}");
        let refused = OwnerOnly::new().unwrap().verify(&pipe).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Other, "{refused}");
    }
}
