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
//! and returns a [`Reason`] for every precondition that fails.
//!
//! # Needs you before the verdict (#59)
//!
//! These send the item to Needs you with a [`Refusal`] instead of failing the attempt:
//!
//! - [`ignored`]: ignored files that appeared during the session (compared with an [`Ignored`]
//!   recorded before it) outside the repository's declared build directories; nothing is
//!   deleted;
//! - [`config`]: a change to git's own configuration since the [`GitConfig`] recorded at claim
//!   (hooks, `.git/config`, `config.worktree`, `include.path` targets, `info/exclude`,
//!   `info/attributes`);
//! - [`protected`]: protected paths the item's commits touched that the item's snapshot does
//!   not allow, with the diff stat.
//!
//! # Trailers (D6 of #7)
//!
//! [`Hooks`] is the hooks directory injected into the agent's git that adds the `Item:`,
//! `Run:` and `Session:` trailers and chains to the repository's own hooks; [`missing`] lists
//! the commits without them for the report; a trailer with a wrong value is forged and fails
//! [`messages`]. Nothing is installed in the repository and nothing is ever amended.
//!
//! The hooks reach only the agent's git, through command-scope configuration in its
//! environment (`GIT_CONFIG_COUNT`/`KEY`/`VALUE`, `core.hooksPath`), which overrides a
//! repository-local `core.hooksPath` (confirmed with Git for Windows by
//! `tests::the_injected_hooks_path_overrides_a_repository_local_one`). [`Hooks::git_config`] is
//! data: `bosun-identity`'s `Env::add_git_config` appends it after the identity's own entries
//! (the `credential.helper` reset is entry 0), and `Env::add_session_var` sets [`Hooks::env`].
//!
//! # Worktrees (#8)
//!
//! [`worktree`] makes an item's worktree on its own branch above a disk floor, and removes it
//! or, when a held-open file blocks removal, quarantines and reports it.
//!
//! # Landing (#8)
//!
//! [`land`] rebases an item's branch onto its target (a conflict is aborted and reported) and
//! fast-forwards the target, deferring when its checkout is dirty or mid-operation.
//!
//! # Landing modes (#8)
//!
//! [`merge::merge_commit`] builds the commit an item lands as (`ff`, `squash` or `no-ff`) with
//! plumbing, refusing a tree other than the verified one; a new commit gets the trailers.
//!
//! # Git LFS (#8)
//!
//! [`lfs`] finds the Git LFS pointers of a commit or a range and carries their objects from
//! one repository's store to another's, each checked against its name; it never runs git-lfs.
//!
//! # The census (#7)
//!
//! [`census::count`] counts the lines matching named regexes in the files a glob names, in a
//! commit's tree; [`census::compare`] lists the kept counters that dropped and the capped ones
//! that rose between the item's base and the judged commit.
//!
//! # Tracked files (#11 D)
//!
//! [`tracked::files`] lists the files of a revision's tree with given suffixes, read-only.
//!
//! It never reaches `bosun-store` or the other side-effect crates (#12).
#![cfg_attr(
    test,
    allow(clippy::disallowed_methods, reason = "a test's own children")
)]

mod base;
pub mod census;
mod child;
mod invoke;
pub mod land;
pub mod lfs;
pub mod merge;
mod message;
mod precondition;
mod protect;
mod refusal;
mod snapshot;
#[cfg(test)]
mod tests;
pub mod tracked;
mod trailer;
pub mod worktree;

pub use base::Base;
pub use invoke::{Error, Git, NO_HOOKS};
pub use message::Expected;
pub use precondition::{Reason, ancestry, check, clean_tree, messages, refs};
pub use protect::protected;
pub use refusal::{KNOWN_BUILD_NAMES, Refusal};
pub use snapshot::{GitConfig, Ignored, config, ignored};
pub use trailer::{
    CHECKOUT_VAR, HOOK_NAMES, Hooks, ITEM_VAR, Ident, MARKER_VAR, Missing, RUN_VAR, SESSION_VAR,
    Trailers, missing,
};
