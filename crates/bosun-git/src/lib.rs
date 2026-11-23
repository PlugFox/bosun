//! Git (#7, #53, #59): the one hooks-disabled git invoker, the verdict preconditions, the
//! ignored-file and configuration snapshots and the trailers.
//!
//! # The invoker
//!
//! Every git command the daemon runs, in any crate, is built by [`Git::command`] (D3 of #7):
//!
//! - `core.hooksPath` points at the null device and `core.fsmonitor` is off, so a hook or an
//!   fsmonitor program an agent planted in the repository never runs, least of all with the
//!   daemon's credentials;
//! - no terminal prompt (`GIT_TERMINAL_PROMPT=0`) and no optional locks
//!   (`GIT_OPTIONAL_LOCKS=0`, so a read does not refresh the index of a checkout an agent may be
//!   working in);
//! - none of the inherited variables that point git at another repository (`GIT_DIR`,
//!   `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`);
//! - standard input closed unless the caller feeds one.
//!
//! # The verdict preconditions
//!
//! [`Base::capture`] records the repository at claim. Before a verdict runs, [`check`] (or each
//! of [`clean_tree`], [`ancestry`], [`refs`] and [`messages`]) compares the repository with it
//! and returns a [`Reason`] for every precondition that fails. The ignored-file and
//! configuration snapshots are #59.
//!
//! It never reaches `bosun-store` or the other side-effect crates (#12).

mod base;
mod invoke;
mod message;
mod precondition;
#[cfg(test)]
mod tests;

pub use base::Base;
pub use invoke::{Error, Git, NO_HOOKS};
pub use message::Expected;
pub use precondition::{Reason, ancestry, check, clean_tree, messages, refs};
