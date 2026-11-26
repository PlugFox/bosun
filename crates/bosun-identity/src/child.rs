//! The one way this crate starts a console program (#131).
//!
//! The daemon may outlive the console it was started in: the logon task hosts it in a
//! `conhost.exe --headless`, and whatever ends that `conhost` leaves the daemon running
//! without it. A console program started plainly then attaches to the console that is gone and
//! fails at load with `0xC0000142` (`STATUS_DLL_INIT_FAILED`), whatever its standard input is.
//! `CREATE_NO_WINDOW` gives each console child a hidden console of its own, which its own
//! children inherit.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

/// `CREATE_NO_WINDOW` of the Windows process creation flags: a console child gets a new,
/// hidden console instead of its parent's.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `program` as a command with standard input closed (a caller that pipes it sets its own)
/// and, on Windows, a hidden console of its own.
#[allow(
    clippy::disallowed_methods,
    reason = "the one place this crate creates a command"
)]
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    // Mutant (`mutant_proc_children_inherit_console`): the child shares this process's console.
    #[cfg(windows)]
    if !cfg!(mutant_proc_children_inherit_console) {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
