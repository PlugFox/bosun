//! The platform layer: every system call of the crate, and so every `unsafe` block.

use std::io;
use std::process::{Child, Command};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(windows)]
mod security;
#[cfg(windows)]
pub(crate) use security::*;

/// A child started held back in its container, not yet running. [`Suspended::resume`] lets it
/// run; the session shim (#8) writes its record in between.
#[derive(Debug)]
pub(crate) struct Suspended {
    child: Child,
    container: Container,
}

impl Suspended {
    /// Starts `command` held back, in a new container. See [`spawn_stopped`].
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        let (child, container) = spawn_stopped(command)?;
        Ok(Self { child, container })
    }

    /// The child's pid, known before the child runs.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Lets the child run. On failure both parts are dropped, which ends the child.
    pub(crate) fn resume(self) -> io::Result<(Child, Container)> {
        resume_pid(self.child.id())?;
        Ok((self.child, self.container))
    }
}
