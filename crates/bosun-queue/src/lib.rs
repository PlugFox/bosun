//! The work queue (#5, #7, #51): the [`Queue`] trait, the item contract, the static card lint,
//! the snapshot, and the backends as modules behind features.
//!
//! - [`Queue`]: `poll`, `project`, `comment`, `upsert_comment`, `lint_target`. A backend never
//!   decides anything; every write is idempotent. [`Changes::since`] computes the deltas of a
//!   poll the same way for every backend.
//! - [`contract`]: the fenced `bosun` block of a card's body, parsed into a [`Contract`].
//! - [`lint`](mod@lint): the static checks of #5 (the "already passes on the base commit" check
//!   runs the verdict and lives with the claim, D7 of #7), with the #44 B warnings. Program
//!   resolution is injectable ([`Resolve`]); [`ShellResolver`] asks the verdict shell (D4).
//! - [`snapshot`]: what an item is claimed with, and its hash.
//! - [`marker`]: the hidden key that makes a comment idempotent on a backend with plain-text
//!   comments.
//! - [`local`] (feature `local_file`, on by default): the board as one TOML file the user edits
//!   while the daemon runs; its module docs give the format and the concurrency rules.
//! - [`projects_v2`] (feature `projects_v2`, on by default): the GitHub Projects v2 backend,
//!   GraphQL through a [`projects_v2::GraphQl`] transport; its module docs give the polling,
//!   the start-up resolution and the author allowlist.
//! - [`testsuite`] (feature `testsuite`): the backend-agnostic trait test suite, for the
//!   Projects v2 backend (#60, #61).
//!
//! This crate never reaches `bosun-store` or the other side-effect crates (#12).

mod card;
pub mod contract;
pub mod lint;
pub mod marker;
mod queue;
mod shell;
pub mod snapshot;

#[cfg(feature = "local_file")]
pub mod local;
#[cfg(feature = "projects_v2")]
pub mod projects_v2;
#[cfg(any(test, feature = "testsuite"))]
pub mod testsuite;

pub use card::{Card, Column, Comment, ItemId, Owned};
pub use contract::{Budget, Contract, ContractError};
pub use lint::{
    Code, Finding, FnResolver, Lint, LintContext, Resolve, Severity, ShellResolver, lint,
};
#[cfg(feature = "local_file")]
pub use local::LocalFile;
#[cfg(feature = "projects_v2")]
pub use projects_v2::ProjectsV2;
pub use queue::{Changes, Cursor, Queue, QueueError};
pub use snapshot::Snapshot;

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
