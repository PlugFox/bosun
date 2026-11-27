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

use crate::{
    Budget, Config, Daemon, Identity, Landing, LandingMode, MergeMethod, Policy, Queue, Repo,
    Verdict, Worktree,
};

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
    /// Neither is `repo.<name>.auto_pause`: it decides when the loop claims, not how an item
    /// runs, so changing it while an item is in flight does not send that item to Needs you.
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
        extensions(&mut canon, repo);
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
    // Only when set, so that a policy without a census keeps the hash it had before (#7).
    if let Some(census) = &policy.census {
        canon
            .str("census")
            .str(&census.glob)
            .map(&census.keep)
            .map(&census.cap);
    }
}

fn repo(canon: &mut Canon, repo: &Repo) {
    canon.str(&repo.key);
    match &repo.queue {
        None => canon.str("none"),
        Some(Queue::LocalFile { file }) => canon.str("local_file").path(file),
        Some(Queue::Projects { owner, number }) => canon.str("projects").str(owner).u64(*number),
    };
    canon.opt_str(repo.model.as_deref());
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

/// The landing mode, at the place and in the form of the version-1 encoding: `local` encodes
/// as before; `merge` and `pr` are new values, so they cannot match a version-1 hash.
fn landing(canon: &mut Canon, landing: &Landing) {
    canon.str(match landing.mode {
        LandingMode::Local => "local",
        LandingMode::Merge => "merge",
        LandingMode::Pr => "pr",
    });
}

/// The keys added after the version-1 encoding (#149, then `sweep` and `effort`), appended after `[daemon]`, one tagged
/// section per group and only when the group is not at its defaults: a repository without
/// `worktree` whose landing keys are at their defaults keeps its version-1 hash, so installing
/// a new daemon sends no item in flight to Needs you (E19). Each tag holds a `/`, which no
/// configured name can contain, so a section cannot be read as another value.
fn extensions(canon: &mut Canon, repo: &Repo) {
    let defaults = Landing::default();
    let landing = &repo.landing;
    // Mutant (`mutant_config_hash_always_extended`): the new values are encoded even at their
    // defaults, so every stage-1 repository's hash changes.
    let always = cfg!(mutant_config_hash_always_extended);
    if always
        || landing.target != defaults.target
        || landing.merge != defaults.merge
        || landing.pr != defaults.pr
    {
        canon.str("landing/2");
        landing_extras(canon, landing);
    }
    if always || repo.worktree.is_some() {
        canon.str("worktree/2");
        worktree(canon, repo.worktree.as_ref());
    }
    // The process sweep (E12), after the worktree: a repository without `sweep` keeps the hash
    // it had before the key existed.
    if always || repo.sweep.is_some() {
        canon.str("sweep/2");
        match &repo.sweep {
            None => canon.bool(false),
            Some(sweep) => canon
                .bool(true)
                .strs(sweep.processes.iter().map(String::as_str)),
        };
    }
    // The sessions' effort level (#7, #9), only when set: a repository without `effort` keeps
    // the hash it had before the key existed.
    if always || repo.effort.is_some() {
        canon.str("effort/2").opt_str(repo.effort.as_deref());
    }
    // The verdict's not-ready keys (#204), only when one is not at its default: a repository
    // that does not use them keeps the hash it had before they existed.
    let verdict = &repo.verdict;
    let unready = Verdict::default();
    if always
        || verdict.unready_exit != unready.unready_exit
        || verdict.unready_backoff != unready.unready_backoff
        || verdict.unready_cap != unready.unready_cap
    {
        canon
            .str("verdict/2")
            .opt_u64(verdict.unready_exit.map(u64::from))
            .duration(verdict.unready_backoff)
            .u64(u64::from(verdict.unready_cap));
    }
    // `auto_pause` is deliberately absent (#7): it decides when the loop claims, not how an
    // item runs, so changing it while an item is in flight must not send it to Needs you.
    // Mutant (`mutant_config_auto_pause_hashed`): the key is encoded when set.
    if cfg!(mutant_config_auto_pause_hashed)
        && let Some(auto_pause) = repo.auto_pause
    {
        canon.str("auto_pause/2").bool(auto_pause);
    }
}

fn landing_extras(canon: &mut Canon, landing: &Landing) {
    canon.opt_str(landing.target.as_deref());
    canon.str(match landing.merge.method {
        MergeMethod::Ff => "ff",
        MergeMethod::Squash => "squash",
        MergeMethod::NoFf => "no-ff",
    });
    canon
        .bool(landing.merge.push)
        .bool(landing.pr.draft)
        .strs(landing.pr.reviewers.iter().map(String::as_str))
        .strs(landing.pr.labels.iter().map(String::as_str));
}

fn worktree(canon: &mut Canon, worktree: Option<&Worktree>) {
    let Some(worktree) = worktree else {
        canon.bool(false);
        return;
    };
    canon
        .bool(true)
        .path(&worktree.root)
        .opt_str(worktree.setup.as_deref())
        .opt_str(worktree.teardown.as_deref())
        .opt_u64(worktree.disk_floor);
    match &worktree.shared_build {
        None => canon.bool(false),
        Some(shared) => canon
            .bool(true)
            .path(&shared.dir)
            .str(&shared.resource)
            .str(&shared.env),
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
    // The fast-death breaker's keys (#7) are encoded only when not at their defaults, behind a
    // tag no other value can read as: a configuration without them keeps the hash it had
    // before they existed, so an upgrade sends no item in flight to Needs you.
    let defaults = Daemon::default();
    // Mutant (`mutant_config_fast_death_always_hashed`): they are encoded even at their
    // defaults, so every existing hash changes.
    if cfg!(mutant_config_fast_death_always_hashed)
        || daemon.fast_death_sessions != defaults.fast_death_sessions
        || daemon.fast_death_within != defaults.fast_death_within
    {
        canon
            .str("fast_death/2")
            .u64(u64::from(daemon.fast_death_sessions))
            .duration(daemon.fast_death_within);
    }
}
