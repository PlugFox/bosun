//! Identities (#2, #7, #54): the identity bundle, the constructed environments of the processes
//! the daemon starts, and the fail-closed checks.
//!
//! # Two environments
//!
//! Nothing is inherited by default (#2, rule 1). [`Env`] is built from an allowlist:
//!
//! - [`Env::tool`], for the git and `gh` the daemon itself runs (board writes, push, pull
//!   requests): the system [`ESSENTIALS`] and the full [`Bundle`] (`GH_CONFIG_DIR`,
//!   `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM`, the SSH key).
//! - [`Env::agent`], for an agent session: the essentials, the toolchain variables the
//!   repository declares, the runners' directories, and git's author and signing configuration
//!   only. It holds no GitHub credential and closes the ambient ones: an empty `GH_CONFIG_DIR`
//!   with [`NO_TOKEN`] shadowing the OS credential store, `credential.helper` reset and the
//!   system configuration (Git Credential Manager on Windows) dropped, no `SSH_AUTH_SOCK`,
//!   [`NO_SSH`] as `GIT_SSH_COMMAND`, `GIT_TERMINAL_PROMPT=0`.
//!
//! A variable that carries a credential ([`credential`]) or belongs to git, `gh` or SSH
//! ([`reserved`]) is denied even when a repository declares it.
//!
//! # Fail closed
//!
//! [`before_spawn`] and [`before_write`] compare the remote's owner, the token's login and the
//! resolved git author with the bundle (#2, rule 2). The facts come through [`RemoteUrl`],
//! [`TokenLogin`] and [`ResolvedAuthor`]; `bosund` wires them to `bosun-git`'s invoker and to
//! [`GhLogin`] (D5 of #7). A mismatch, or a probe that cannot answer, is a [`Refusal`], whose
//! text is the Needs-you reason.
//!
//! It never reaches `bosun-store` or the other side-effect crates (#12).

mod bundle;
mod check;
mod env;
#[cfg(test)]
mod tests;

pub use bundle::{Author, Bundle};
pub use check::{
    GhLogin, ProbeError, Refusal, RemoteUrl, ResolvedAuthor, TokenLogin, before_spawn,
    before_write, check_author, check_login, check_remote, host_and_owner,
};
pub use env::{
    CREDENTIAL_SUFFIXES, CREDENTIALS, ESSENTIALS, Env, NO_SSH, NO_TOKEN, RESERVED_PREFIXES,
    credential, inherited, reserved,
};
