//! The `lease.*` methods (#8, "Leases"; E15, E16): a person's command takes the leases items
//! take, through `bosun with <resource>... -- <command>`.
//!
//! # The holder
//!
//! The holder of a command's lease is its process: [`AcquireParams::pid`] names it (the CLI
//! itself), and the daemon reads that process's start time itself, so a pid reused by another
//! process is another holder. A pid that is not running is refused. The lease is released by
//! `lease.release`, or by the daemon once the process is gone: before it serves either method,
//! and on every tick of a loop.
//!
//! # Polling
//!
//! The first `lease.acquire` of a pid files its request; every later call with the same pid and
//! the same set only reports whether it is granted yet (the CLI polls). A different set for a
//! pid that already holds or waits is refused, and so are unknown resource names (with the
//! names) and a repository-scope resource without [`AcquireParams::repo`]. The wait is written
//! to the store of [`AcquireParams::identity`], with no item.
//!
//! # What a caller learns
//!
//! Machine-scope resources are shared across identities: the reply says only whether the
//! caller's own request is granted, never who holds a resource.
//!
//! # The operator token
//!
//! Both methods take the operator token, as `control.pause` does ([`crate::control`]): an
//! agent session, which does not have it, cannot take a lease this way. Without the current
//! token a call is refused with [`crate::rpc::code::UNAUTHORIZED`].
//!
//! # `lease.list`
//!
//! What the Resources tab shows (#8): every resource instance the identity can lease (the
//! machine-scope ones, its own identity-scope ones, those of its repositories) with its
//! holders and how many requests wait for it, and the identity's worktrees that are on disk or
//! quarantined. It reads only and takes no token.
//!
//! A holder is shown as `item <id>` or `bosun with <pid>` only when it is an item of one of
//! the identity's repositories or a command that named the identity; any other holder is the
//! word [`BUSY`] and nothing else (machine-scope resources are shared across identities, and
//! "busy" is all one identity learns of another through them). The same holds for
//! [`WaitingFor::holders`] in `status.overview` and `item.get`.

/// What another identity's holder is shown as.
pub const BUSY: &str = "busy";

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params of `lease.acquire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcquireParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    /// The identity whose store records the wait, and whose instances of `identity`-scope
    /// resources are asked for.
    pub identity: String,
    /// A repository of the identity, by its name in the configuration: its instances of
    /// `repository`-scope resources. Required when one is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The resources by their names in the configuration (`[resource.<name>]`).
    pub resources: Vec<String>,
    /// The process that holds the lease: the caller itself.
    pub pid: u32,
}

/// Result of `lease.acquire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcquireResult {
    /// The whole set is held; `false`: the request waits.
    pub granted: bool,
}

/// Params of `lease.release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseParams {
    /// The operator token the daemon wrote at start. Missing, it is refused like a wrong one.
    #[serde(default)]
    pub operator: String,
    /// The process whose lease is released and whose request is withdrawn.
    pub pid: u32,
}

/// Result of `lease.release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseResult {
    /// The process held or waited for something; releasing nothing is not an error.
    pub released: bool,
}

/// Result of `lease.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Resources {
    pub identity: String,
    /// Every resource instance the identity can lease, by instance name.
    pub resources: Vec<ResourceView>,
    /// The identity's worktrees that are `created` or `quarantined`, in the order they were
    /// made.
    pub worktrees: Vec<WorktreeView>,
}

/// One resource instance and who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceView {
    /// The instance: `<name>` for the machine scope, `<name>/<identity>`, `<name>/<repo>`.
    pub name: String,
    /// `[resource.<name>] kind`.
    pub kind: String,
    /// How many holders may hold it at once.
    pub capacity: u64,
    /// `machine`, `identity` or `repository`.
    pub scope: String,
    /// Its holders: `item <id>`, `bosun with <pid>`, or [`BUSY`] for another identity's.
    pub holders: Vec<String>,
    /// The requests waiting for it.
    pub waiting: u64,
}

/// A worktree on disk, or quarantined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeView {
    pub item: String,
    pub path: String,
    /// `item` (the item's checkout) or `verdict` (a verdict's throwaway checkout).
    pub kind: String,
    /// `created` or `quarantined`.
    pub state: String,
    /// Its size on disk when it was last measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// A quarantined worktree: the file that blocked its removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<String>,
}

/// A full resource instance an item's request waits for, in `status.overview` and `item.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WaitingFor {
    /// The instance, as in [`ResourceView::name`].
    pub resource: String,
    /// Its holders, as in [`ResourceView::holders`].
    pub holders: Vec<String>,
}
