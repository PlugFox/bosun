//! The platform layer: every system call of the crate, and so every `unsafe` block.

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
