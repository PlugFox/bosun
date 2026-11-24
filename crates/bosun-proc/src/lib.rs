//! Child processes (#7, #50): spawning into a container, tree kill, reaping by pid and start
//! time, the power request, and on Windows the security descriptor of the local endpoint (D2,
//! #71), and the session shim of stage 2 (#8).
//!
//! - [`Spawner`] starts a child from a [`ChildSpec`] (argv, a constructed environment, a working
//!   directory) inside a container, with separate stdout and stderr pipes, and hands back a
//!   [`Tree`]: the child's [`ProcessKey`] (pid and start time), [`Tree::kill_tree`] and an async
//!   wait. [`NativeSpawner`] is the in-process implementation; the session shim of #8 becomes a
//!   second one behind the same trait.
//! - [`reap`] kills a recorded process after a restart only if its pid and its start time both
//!   still match, so a reused pid is never killed.
//! - [`Power`] holds the machine awake while work runs: [`PowerGuard`] issues one request on
//!   acquire and one release on drop. [`SystemPower`] is the platform's, [`FakePower`] counts.
//! - [`next_local_midnight`] and [`last_local_midnight`] read the machine's time zone, which
//!   the standard library does not: the daily cap parks until local midnight (#67).
//! - [`disk::free_space`] reads the bytes available on the volume that holds a path, for the
//!   disk floor of item worktrees (#8).
//! - [`shim`] is the session shim (#129), run as the `bosun-shim` binary: it holds the child's
//!   container, writes its output to files and its pid and exit to a record, and enforces the
//!   wall-clock cap, so all of that outlives the daemon.
//! - `pipe` (Windows): `pipe::OwnerOnly` creates the daemon's named pipe with a DACL that admits
//!   only the user it runs as, and `pipe::dacl` reads a handle's DACL back.
//!
//! # Containers
//!
//! On Windows the container is a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The child
//! is created suspended, assigned to the job and only then resumed, so nothing it starts can
//! run outside the job. Killing the job kills every process in it, and when the last handle to
//! the job closes (the daemon exits, cleanly or not) Windows kills them too. The job does not
//! allow breakaway, so a descendant that asks for `CREATE_BREAKAWAY_FROM_JOB` is refused, and
//! [`Tree::members`] lists the job's processes exactly.
//!
//! On Unix the container is a process group: the child leads a new group and its descendants
//! inherit it, so `kill(-pgid, SIGKILL)` kills the tree. A descendant that calls `setsid` or
//! `setpgid` leaves the group and escapes; cgroups (#8) close that gap. [`Tree::members`] lists
//! the group from the process table, best effort: from `/proc` on Linux, and on macOS from a
//! `proc_listallpids` snapshot, weaker still. On Linux the child also
//! gets `PR_SET_PDEATHSIG(SIGKILL)`, so it dies with the daemon; its own descendants do not
//! (the signal is not inherited), and they stay in the group for [`Tree::kill_tree`] only while
//! the daemon lives. The signal fires when the *thread* that spawned the child exits, so the
//! daemon spawns from a thread that lives as long as it does. macOS has no parent-death signal:
//! a child of a daemon that dies without warning lives on, and the daemon's [`reap`] at the next
//! start is the only cleanup.
//!
//! The only crate where `unsafe` is allowed (#12): every block carries its justification in a
//! `// SAFETY:` comment. `cargo xtask check-deps` fails when any other crate lifts the
//! workspace's `unsafe_code = "deny"`.
#![allow(unsafe_code)]

pub mod disk;
mod key;
mod local_time;
#[cfg(windows)]
pub mod pipe;
mod power;
pub mod shim;
mod spawn;
mod sys;

#[cfg(test)]
mod helper;

pub use key::{ProcessKey, StartTime, is_running, reap, start_time};
pub use local_time::{last_local_midnight, next_local_midnight, utc_offset_ms};
pub use power::{FakePower, Power, PowerGuard, SystemPower};
pub use spawn::{ChildSpec, Input, NativeSpawner, Output, Spawned, Spawner, Tree, WaitFuture};
