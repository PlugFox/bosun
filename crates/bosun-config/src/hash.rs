//! Stable hashes of a repository's policy and of its effective configuration (#2, #7).
//!
//! The daemon records both at claim; a change of either during a session sends the item to
//! Needs you. A hash is taken over a canonical encoding of the parsed values, never over the
//! text, so reordering keys, reformatting or editing a comment does not change it; maps and
//! sets are encoded in sorted order. Every string is length-prefixed, so two different values
//! never encode the same.
//!
//! The digest is SHA-256 (the `sha2` crate, already in the dependency graph through `sqlx`, so
//! nothing new is compiled). It is not used as a defence against a chosen collision, only as a
//! fingerprint; a cryptographic digest makes that a non-question.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::{Budget, Config, Daemon, Identity, Landing, LandingMode, Policy, Queue, Repo};

/// The canonical encoding being built.
struct Canon(Vec<u8>);

impl Canon {
    fn new(domain: &str) -> Self {
        let mut canon = Self(Vec::new());
        canon.str(domain);
        canon
    }

    fn str(&mut self, value: &str) -> &mut Self {
        self.u64(value.len() as u64);
        self.0.extend_from_slice(value.as_bytes());
        self
    }

    fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn bool(&mut self, value: bool) -> &mut Self {
        self.0.push(u8::from(value));
        self
    }

    fn duration(&mut self, value: Duration) -> &mut Self {
        self.u64(value.as_secs())
    }

    fn opt_u64(&mut self, value: Option<u64>) -> &mut Self {
        match value {
            Some(value) => self.bool(true).u64(value),
            None => self.bool(false),
        }
    }

    fn opt_str(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(value) => self.bool(true).str(value),
            None => self.bool(false),
        }
    }

    fn path(&mut self, value: &Path) -> &mut Self {
        self.str(&value.to_string_lossy())
    }

    fn opt_path(&mut self, value: Option<&Path>) -> &mut Self {
        match value {
            Some(value) => self.bool(true).path(value),
            None => self.bool(false),
        }
    }

    fn strs<'a>(&mut self, values: impl ExactSizeIterator<Item = &'a str>) -> &mut Self {
        self.u64(values.len() as u64);
        for value in values {
            self.str(value);
        }
        self
    }

    fn map(&mut self, values: &BTreeMap<String, String>) -> &mut Self {
        self.u64(values.len() as u64);
        for (key, value) in values {
            self.str(key).str(value);
        }
        self
    }

    fn finish(&self) -> String {
        let digest = Sha256::digest(&self.0);
        let mut hex = String::with_capacity(64);
        for byte in digest {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

impl Policy {
    /// The policy's hash: 64 lowercase hex digits, stable across key order, formatting and
    /// comments, and changed by any change of a protected path or of `push`.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut canon = Canon::new("bosun-config/policy/1");
        policy(&mut canon, self);
        canon.finish()
    }
}

impl Config {
    /// The hash of repository `name`'s effective configuration, or `None` when no such
    /// repository is configured: 64 lowercase hex digits over everything that governs its items:
    /// the repository's own keys (its policy included), the identity it binds, `[budget]` and
    /// `[daemon]`. Other repositories, other identities, `[retention]` and the notices are not
    /// part of it, so editing them during a session does not disturb this repository's item.
    #[must_use]
    pub fn repo_hash(&self, name: &str) -> Option<String> {
        let repo = self.repos.get(name)?;
        let identity = self.identities.get(&repo.identity)?;
        let mut canon = Canon::new("bosun-config/repo/1");
        canon.str(name);
        self::repo(&mut canon, repo);
        canon.str(&repo.identity);
        self::identity(&mut canon, identity);
        budget(&mut canon, &self.budget);
        daemon(&mut canon, &self.daemon);
        Some(canon.finish())
    }
}

fn policy(canon: &mut Canon, policy: &Policy) {
    if cfg!(mutant_config_hash_ignores_policy) {
        // Mutant: the protected paths are left out of the encoding.
        canon.strs(std::iter::empty());
    } else {
        canon.strs(policy.protected.iter().map(String::as_str));
    }
    canon.bool(policy.push);
}

fn repo(canon: &mut Canon, repo: &Repo) {
    canon.str(&repo.key);
    match &repo.queue {
        None => canon.str("none"),
        Some(Queue::LocalFile { file }) => canon.str("local_file").path(file),
        Some(Queue::Projects { owner, number }) => canon.str("projects").str(owner).u64(*number),
    };
    canon
        .duration(repo.verdict.timeout)
        .opt_str(repo.verdict.shell.as_deref());
    policy(canon, &repo.policy);
    canon.strs(repo.build_dirs.iter().map(String::as_str));
    landing(canon, &repo.landing);
    canon.u64(repo.mcp_servers.len() as u64);
    for (name, server) in &repo.mcp_servers {
        canon
            .str(name)
            .str(&server.command)
            .strs(server.args.iter().map(String::as_str))
            .map(&server.env);
    }
}

fn landing(canon: &mut Canon, landing: &Landing) {
    match landing.mode {
        LandingMode::Local => canon.str("local"),
    };
}

fn identity(canon: &mut Canon, identity: &Identity) {
    canon
        .path(&identity.claude_config_dir)
        .path(&identity.database)
        .str(&identity.github_host)
        .opt_str(identity.github_login.as_deref())
        .opt_path(identity.gh_config_dir.as_deref())
        .opt_path(identity.git_config.as_deref())
        .opt_path(identity.ssh_key.as_deref())
        .opt_path(identity.codex_home.as_deref())
        .strs(identity.remote_allowlist.iter().map(String::as_str))
        .strs(identity.author_allowlist.iter().map(String::as_str));
}

fn budget(canon: &mut Canon, budget: &Budget) {
    canon
        .opt_u64(budget.item_eqv)
        .opt_u64(budget.item_wall_clock.map(|d| d.as_secs()))
        .opt_u64(budget.daily_eqv)
        .u64(budget.reserve_eqv);
}

fn daemon(canon: &mut Canon, daemon: &Daemon) {
    canon
        .bool(daemon.auto_pause)
        .duration(daemon.auto_pause_idle)
        .duration(daemon.question_grace)
        .duration(daemon.kill_grace)
        .duration(daemon.hello_timeout)
        .u64(u64::from(daemon.park_cap))
        .u64(u64::from(daemon.mcp_call_cap));
}
