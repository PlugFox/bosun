//! Parsing and validation of the bosun configuration file (#4, #18, #55, #123).
//!
//! The file is TOML and is the registry of identities and repositories: there is no global
//! database (#3). The daemon reads it at start and again on change ([`reload`]); the CLI reads
//! it too, so `bosun doctor` works with the daemon down (#12). This crate only reads: it opens
//! no database and no credential store, and it never writes the file (`config.set` is deferred,
//! #7 D8).
//!
//! # Location
//!
//! [`default_path`]: the variable `BOSUN_CONFIG` when set, otherwise
//!
//! | Platform | File |
//! | --- | --- |
//! | Windows | `%LOCALAPPDATA%\bosun\config.toml` |
//! | macOS | `~/Library/Application Support/bosun/config.toml` |
//! | Linux and others | `$XDG_CONFIG_HOME/bosun/config.toml`, else `~/.config/bosun/config.toml` |
//!
//! On Windows the file is in the local, not the roaming, application data: it names paths of
//! this machine, and a roaming profile would copy it to others.
//!
//! # Format (stage 1)
//!
//! Only an identity's `claude_config_dir` and `database` and a repository's `path` and
//! `identity` are required; every other key has a default, so a stage-0 file is a valid
//! stage-1 file.
//!
//! ```toml
//! [daemon]
//! auto_pause = true             # pause a repository while an interactive session works in it
//! auto_pause_idle = "30m"       # ... until that session has been idle this long
//! question_grace = "30s"        # a session lives this long after a blocking question
//! kill_grace = "10s"            # between asking a child to stop and killing its tree
//! hello_timeout = "30s"         # bosun-mcp must say hello within this after spawn
//! park_cap = 5                  # uncounted parks and resumes per item, then Needs you
//! mcp_call_cap = 20             # calls of each agent tool per attempt
//! fast_death_sessions = 3       # this many sessions in a row of one repository that crash
//! fast_death_within = "60s"     # ... this soon after their spawn pause it until `bosun resume`
//!
//! [identity.personal]           # the identity bundle (#2)
//! claude_config_dir = 'C:\Users\me\.claude'               # CLAUDE_CONFIG_DIR of the identity
//! database = 'C:\Users\me\AppData\Local\bosun\personal.db' # its own SQLite file
//! github_host = "github.com"    # default
//! github_login = "me"           # the token must resolve to this login before any write
//! gh_config_dir = 'C:\Users\me\.config\gh-personal'        # GH_CONFIG_DIR of the daemon's gh
//! git_config = 'C:\Users\me\.gitconfig-personal'           # GIT_CONFIG_GLOBAL
//! ssh_key = 'C:\Users\me\.ssh\id_personal'                 # GIT_SSH_COMMAND -i
//! codex_home = 'C:\Users\me\.codex-personal'               # CODEX_HOME
//! remote_allowlist = ["me"]     # owners whose repositories this identity may touch
//! author_allowlist = ["me"]     # logins whose text is inlined into prompts
//!
//! [repo.bosun]
//! path = 'C:\git\bosun'
//! identity = 'personal'
//! build_dirs = ["target"]       # new ignored files are expected here only
//! queue = { kind = "projects", owner = "me", number = 7 }  # or kind = "local_file", file = '...'
//! model = "opus"                # the sessions' default model; required with a queue (#133)
//! effort = "high"               # the sessions' default effort level; default: none passed
//! verdict = { timeout = "30m" } # and `shell`, default `sh` (from Git on Windows)
//! landing = { mode = "merge", target = "main", merge = { method = "squash", push = false } }
//! worktree = { root = 'C:\wt\bosun', setup = "cargo fetch", disk_floor = "20GB", shared_build = { dir = 'C:\wt\bosun-target', resource = "target", env = "CARGO_TARGET_DIR" } }
//!
//! [repo.bosun.policy]
//! protected = ["tests/harness", "xtask/src/lint.rs"]  # `.gitignore` is always added
//! push = false                  # default
//!
//! [repo.bosun.policy.census]    # optional (#7): counted at an item's base and judged commit
//! glob = "tests/**"             # a git `:(glob)` pathspec, repository-relative
//! keep = { tests = "fn test_", assertions = "assert" }  # matching lines must not drop
//! cap = { ignored = '#\[ignore' }                      # matching lines must not rise
//!
//! [repo.bosun.mcp_servers.docs] # merged into the per-session MCP configuration
//! command = "docs-server"
//! args = ["--stdio"]
//! env = { LEVEL = "info" }
//!
//! [budget]                      # every cap is off by default
//! item_eqv = 5_000_000          # input-equivalent tokens per item
//! item_wall_clock = "2h"
//! daily_eqv = 40_000_000
//! reserve_eqv = 5_000_000       # headroom kept for interactive work
//!
//! [retention]
//! reingest_window = "30d"       # whole hours, days or weeks: "12h", "30d", "6w"; default 30d
//!
//! [resource.target]             # a leasable resource (#8), leased in worktree mode
//! kind = "build-dir"            # free text: "build-dir", "gpu", "device", ...
//! capacity = 1                  # holders at once, at least 1; default 1 (exclusive)
//! scope = "machine"             # "machine", "identity" or "repository"; default "machine"
//! ```
//!
//! - Paths are absolute; TOML literal strings (`'...'`) keep Windows backslashes as written.
//!   Protected paths and build directories are relative to the repository, `/`-separated,
//!   without wildcards ([`Policy::protected`]).
//! - Durations are a positive whole number and one of `s`, `m`, `h`, `d`, `w`.
//! - Identity, repository and MCP server names are ASCII letters, digits, `-` and `_`.
//! - `repo.<name>.policy.census` (#7): after a passing verdict, the lines matching each counter
//!   (git's extended regex, `git grep -E`) in the files of `glob` are counted in the tree of the
//!   item's base and of the judged commit; a `keep` counter that dropped or a `cap` counter that
//!   rose sends the item to Needs you, unless the card's `allow-census` names it. An unknown key,
//!   an empty or absolute `glob`, a census without counters and a name in both tables are
//!   refused. The policy hash covers the census only when it is set.
//! - An unknown key of a `policy` is refused, so that a misspelt `protected` cannot leave paths
//!   unprotected; so is an unknown key of a `resource`, a `worktree`, a
//!   `worktree.shared_build`, a `landing.merge`, a `landing.pr` or a `sweep`.
//! - `repo.<name>.model` (#133): the model a session asks for (`--model`) when its card names
//!   none; required when the repository has a `queue`.
//! - `repo.<name>.effort` (#7, #9): the effort level a session asks for (`--effort`) when its
//!   card names none; one of `low`, `medium`, `high`, `xhigh`, `max`
//!   ([`bosun_core::EFFORT_LEVELS`]), anything else is refused. Default: no `--effort` is passed.
//! - `repo.<name>.landing` (#8): how an item's commits land. `mode` is `local` (the default),
//!   `merge` or `pr`. `local` is not "stage 1 only": it is the dogfood mode, `merge` with the
//!   method `ff` and no push onto the branch the item was claimed from (E6, E20); without a
//!   worktree the commits stay on the current branch, as in stage 1. `merge` and `pr` need
//!   `repo.<name>.worktree` (E19: every new path is gated on it, and without it stage 1 is
//!   unchanged). `pr` pushes the item's branch and opens a pull request into `target`.
//!   - `target`: the branch the item is claimed from and lands on, whatever the mode; default:
//!     the branch the item was claimed from. It is a bare name (no `refs/heads/`) that git
//!     accepts as a branch (`git check-ref-format --branch`, checked here without running git).
//!     Only worktree mode reads it: without `repo.<name>.worktree` it is ignored with a
//!     [`Notice`] (E19).
//!   - `merge.method`: `ff` (default), `squash` or `no-ff`; `merge.push`: default false, and
//!     `true` is not available yet. A `merge` table with `mode = "local"` is refused.
//!   - `pr.draft` (default true), `pr.reviewers` and `pr.labels` (default empty) are read for
//!     the `pr` mode; `pr.title` and `pr.describe_session` are a later card's, ignored with a
//!     [`Notice`].
//! - `repo.<name>.worktree` (#8): where the repository's item worktrees go. `root` is required
//!   and lies outside the repository's `path`; `setup` and `teardown` are shell commands run in a
//!   new worktree and before one is removed; `disk_floor` is the free space below which no new
//!   worktree is made. Sizes are a positive whole number and one of `B`, `KB`, `MB`, `GB`, `TB`
//!   (powers of 1000) or `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024).
//!   - `shared_build` (E1): the one build directory the repository's worktrees share, behind an
//!     exclusive lease held from the session through the landing re-verdict, and injected as
//!     `env` (default `CARGO_TARGET_DIR`, an ASCII identifier). `dir` is absolute and `resource`
//!     names a `[resource.<name>]`; both are required. `dir` neither holds nor lies under the
//!     repository's `path` or `worktree.root`: cargo writes its own files into it, and a
//!     `cargo clean` deletes it whole. It is the default for Rust: per-worktree
//!     targets cost 12-15 GB each, and are the opt-in (no `shared_build`) of a repository that
//!     raises concurrency.
//! - `repo.<name>.sweep` (#8, E12): the process sweep, opt-in per repository. `processes` names
//!   the known strays of the repository: after each attempt's session has ended, every running
//!   process of the user with one of these executable file names that started at or after the
//!   attempt began is killed, logged and noted on the item. A name is a file name without a
//!   directory: ASCII letters, digits, `.`, `-` and `_`, not empty and not only dots; on
//!   Windows it matches with or without `.exe`, case-insensitively. Each match is killed alone,
//!   by its pid, never its process group. A process inside or descended from another live
//!   session, verdict or setup run of the daemon (any repository, any identity) or from a live
//!   `bosun with` command is spared. Anything else of the user with a listed name that started
//!   during the attempt is killed, so list only names nothing else of yours runs under: the
//!   sweep may kill a process of your own with that name that you started while the attempt
//!   ran. Default: no sweep. The key is its own opt-in, independent of `worktree`. (Not the
//!   worktree sweep, which closes item worktrees nobody owns.)
//! - `[resource.<name>]` (#8): a resource items may name in `needs`. `kind` is required. They
//!   are read at daemon start (an edit takes effect at the next start) and leased in worktree
//!   mode: a card of a repository with `repo.<name>.worktree` may need them, and holds them from
//!   before its first session until it leaves the loop. Without `repo.<name>.worktree` a card
//!   that needs one is refused by lint, as in stage 1 (E19).
//! - `notify`, and every other key of #4 that a later stage reads (`repo.<name>.report`,
//!   `daemon.http_port`, ...), is accepted and ignored with a [`Notice`]. Any other section is
//!   an error.
//!
//! # Validation
//!
//! - A repository binds an identity that is configured, and two repositories never share a
//!   path.
//! - Two identities never share a Claude configuration directory, a database file, a `gh`
//!   configuration directory, a git configuration file, an SSH key or a Codex home (#2). Paths
//!   are compared by their canonical key ([`bosun_core::canonical_path`]), so on Windows case
//!   and separators do not matter. Links and junctions are not resolved for this comparison.
//! - A repository's `worktree.root` is neither its `path` nor under it, compared the same way.
//! - A `worktree.shared_build.dir` neither holds nor lies under the repository's `path` or
//!   `worktree.root` (nor is either), compared the same way; its `resource` exists and has
//!   `capacity = 1`.
//! - A `landing.target` is a valid git branch name: no component begins with `.` or ends with
//!   `.lock`; no `..`, `@{`, `//`, control character, space, `~`, `^`, `:`, `?`, `*`, `[` or
//!   `\`; it does not begin or end with `/`, end with `.` or begin with `-`, and is not `@` or
//!   `HEAD`. A `refs/...` name is refused with a hint to write the bare name.
//! - A `landing.mode` of `merge` or `pr` has a `worktree`; `merge.push = true` is refused as
//!   not available yet.
//! - The directory of each database is on a local disk, as a [`DiskProbe`] sees it.
//! - `budget.reserve_eqv` is less than `budget.daily_eqv` when both are set.
//!
//! Every problem carries its line and column when it has a place in the file.
//!
//! # Hashes
//!
//! [`Policy::hash`] and [`Config::repo_hash`] fingerprint the parsed values, never the text:
//! the daemon records both at claim, and a change of either during a session sends the item to
//! Needs you (#2). Reordering keys or editing a comment changes neither.
//!
//! # Tools
//!
//! [`tools`] finds the programs the configuration relies on (`git`, `gh`, the agent CLI, the
//! verdict shell) on a given `PATH`, for `bosun doctor` and the daemon alike.

mod disk;
mod error;
mod hash;
mod policy;
mod read;
pub mod tools;
mod validate;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use bosun_core::Scope;
pub use disk::{Disk, DiskProbe, SystemProbe};
pub use error::{Error, Location, Notice, Problem};
pub use policy::{ALWAYS_PROTECTED, Census, Policy};

/// The default of `identity.<name>.github_host`.
pub const DEFAULT_GITHUB_HOST: &str = "github.com";

/// The name of the daemon's own MCP server (`bosun-mcp`, #7); a repository cannot declare it.
pub const RESERVED_MCP_SERVER: &str = "bosun";

/// A valid configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// Identities by name.
    pub identities: BTreeMap<String, Identity>,
    /// Repositories by name.
    pub repos: BTreeMap<String, Repo>,
    /// `[retention]`.
    pub retention: Retention,
    /// `[budget]`.
    pub budget: Budget,
    /// `[daemon]`.
    pub daemon: Daemon,
    /// `[resource.<name>]`: leasable resources by name (#8).
    pub resources: BTreeMap<String, Resource>,
    /// What the file holds that this version does not read, in file order.
    pub notices: Vec<Notice>,
}

/// `[identity.<name>]`: an identity bundle (#2). Only the Claude configuration directory and
/// the database are required; a key left out is `None` or empty, and a check that needs it
/// (#54) refuses to act rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Identity {
    /// `claude_config_dir`: the identity's `CLAUDE_CONFIG_DIR`, whose transcripts it owns.
    pub claude_config_dir: PathBuf,
    /// `database`: the identity's SQLite file (#3).
    pub database: PathBuf,
    /// `github_host`: the GitHub host of the identity's account. Default `github.com`.
    pub github_host: String,
    /// `github_login`: the login the identity's token must resolve to before any write.
    pub github_login: Option<String>,
    /// `gh_config_dir`: the `GH_CONFIG_DIR` of the `gh` the daemon runs for this identity.
    pub gh_config_dir: Option<PathBuf>,
    /// `git_config`: the file `GIT_CONFIG_GLOBAL` names (author, committer, signing key).
    pub git_config: Option<PathBuf>,
    /// `ssh_key`: the private key `GIT_SSH_COMMAND` passes with `-i`.
    pub ssh_key: Option<PathBuf>,
    /// `codex_home`: the identity's `CODEX_HOME`.
    pub codex_home: Option<PathBuf>,
    /// `remote_allowlist`: owners or organisations whose repositories this identity may touch.
    pub remote_allowlist: BTreeSet<String>,
    /// `author_allowlist`: logins whose issue bodies and comments are inlined into prompts.
    pub author_allowlist: BTreeSet<String>,
}

/// `[repo.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Repo {
    /// `path`, as written.
    pub path: PathBuf,
    /// The canonical key of `path` on this platform ([`bosun_core::canonical_path`]): the form
    /// working directories are matched against.
    pub key: String,
    /// `identity`: the name of the one identity the repository binds.
    pub identity: String,
    /// `queue`: the board the repository's items come from; `None` for a repository whose
    /// sessions are only ingested.
    pub queue: Option<Queue>,
    /// `model`: the model a session of the repository asks for (`--model`) when its card names
    /// none (#5, #133): a name or an alias the runner accepts, e.g. `opus`. Required when
    /// `queue` is set, so no session runs on whatever the agent CLI defaults to.
    pub model: Option<String>,
    /// `effort`: the effort level a session of the repository asks for (`--effort`) when its
    /// card names none (#7, #9): one of [`bosun_core::EFFORT_LEVELS`]. Default `None`: no
    /// `--effort` is passed, and the agent CLI uses its own.
    pub effort: Option<String>,
    /// `verdict`: the defaults of the item contract's verdict.
    pub verdict: Verdict,
    /// `policy`: the permission policy, effective.
    pub policy: Policy,
    /// `build_dirs`: repository-relative directories where new ignored files are expected
    /// (#7); anywhere else a new ignored file sends the item to Needs you.
    pub build_dirs: BTreeSet<String>,
    /// `landing`.
    pub landing: Landing,
    /// `mcp_servers.<name>`: servers merged into the per-session MCP configuration (#7).
    pub mcp_servers: BTreeMap<String, McpServer>,
    /// `worktree`: where the repository's item worktrees go (#8). Default `None`.
    pub worktree: Option<Worktree>,
    /// `sweep`: the process sweep after each attempt (#8, E12). Default `None`: no sweep.
    pub sweep: Option<Sweep>,
}

/// `repo.<name>.sweep` (#8, E12): what is killed after an attempt's session has ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sweep {
    /// `processes`: executable file names of known strays, without a directory: ASCII letters,
    /// digits, `.`, `-` and `_`. Order and duplicates do not matter. Default empty. The sweep
    /// may kill a process of the user's own with one of these names that the user started
    /// while an attempt ran (see the module documentation).
    pub processes: BTreeSet<String>,
}

/// `repo.<name>.worktree` (#8).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Worktree {
    /// `root`: the directory item worktrees are made in; never the repository's path or under
    /// it.
    pub root: PathBuf,
    /// `setup`: a shell command run in a new worktree. Default `None`.
    pub setup: Option<String>,
    /// `teardown`: a shell command run in a worktree before it is removed. Default `None`.
    pub teardown: Option<String>,
    /// `disk_floor`: in bytes, the free space below which no new worktree is made. Default
    /// `None`: no floor.
    pub disk_floor: Option<u64>,
    /// `shared_build`: the build directory the worktrees share. Default `None`: each worktree
    /// builds in its own.
    pub shared_build: Option<SharedBuild>,
}

impl Worktree {
    /// A worktree configuration with `root` and every other key at its default: no `setup`,
    /// no `teardown`, no disk floor, no shared build. The daemon rebuilds one from what it
    /// recorded when the key is gone from the configuration (#8).
    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        Self {
            root,
            setup: None,
            teardown: None,
            disk_floor: None,
            shared_build: None,
        }
    }
}

/// `[resource.<name>]`: a resource items lease (#8).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resource {
    /// `kind`: free text naming what it is (`build-dir`, `gpu`, `device`, ...).
    pub kind: String,
    /// `capacity`: how many holders may hold it at once, at least 1. Default 1: exclusive.
    pub capacity: u32,
    /// `scope`: what it is shared within. Default [`Scope::Machine`].
    pub scope: Scope,
}

/// `repo.<name>.queue`: where the repository's items come from (#5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Queue {
    /// `kind = "local_file"`, `file = '<absolute path>'`: the local-file backend.
    LocalFile {
        /// The file holding the cards.
        file: PathBuf,
    },
    /// `kind = "projects"`, `owner`, `number`: a GitHub Projects v2 board.
    Projects {
        /// The user or organisation that owns the project.
        owner: String,
        /// The project's number.
        number: u64,
    },
}

/// `repo.<name>.verdict`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Verdict {
    /// `timeout`: the verdict timeout when the card names none. Default 30 minutes.
    pub timeout: Duration,
    /// `shell`: the program that runs a verdict as `<shell> -c <verdict>`. Default `None`:
    /// `sh`, on Windows resolved from the Git installation (#7, D4).
    pub shell: Option<String>,
}

impl Default for Verdict {
    fn default() -> Self {
        Self {
            timeout: Duration::from_mins(30),
            shell: None,
        }
    }
}

/// `repo.<name>.worktree.shared_build` (#8, E1): the one build directory the repository's
/// worktrees share, behind an exclusive lease.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedBuild {
    /// `dir`: the directory; never the repository's path, under it, or under the worktree
    /// root.
    pub dir: PathBuf,
    /// `resource`: the name of the `[resource.<name>]` that guards it; it exists and has
    /// `capacity = 1`.
    pub resource: String,
    /// `env`: the variable the directory is injected as. Default [`DEFAULT_SHARED_BUILD_ENV`].
    pub env: String,
}

/// The default of `repo.<name>.worktree.shared_build.env`.
pub const DEFAULT_SHARED_BUILD_ENV: &str = "CARGO_TARGET_DIR";

/// `repo.<name>.landing` (#8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Landing {
    /// `mode`. Default `local`.
    pub mode: LandingMode,
    /// `target`: the branch the item is claimed from and lands on, a valid bare branch name.
    /// Default `None`: the branch the item was claimed from. Always `None` without
    /// `repo.<name>.worktree` (E19).
    pub target: Option<String>,
    /// `merge`: how `mode = "merge"` lands.
    pub merge: Merge,
    /// `pr`: the pull request `mode = "pr"` opens.
    pub pr: PullRequest,
}

/// `repo.<name>.landing.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LandingMode {
    /// `local`: the dogfood mode (E6, E20): `merge` with the method `ff` and no push, onto the
    /// branch the item was claimed from. Without a worktree the commits stay on the current
    /// branch, as in stage 1.
    #[default]
    Local,
    /// `merge`: the run branch is merged into the target with [`Merge::method`]. Needs
    /// `repo.<name>.worktree`.
    Merge,
    /// `pr`: the item's branch is pushed and a pull request into the target is opened. Needs
    /// `repo.<name>.worktree`.
    Pr,
}

impl LandingMode {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "local" => Some(Self::Local),
            "merge" => Some(Self::Merge),
            "pr" => Some(Self::Pr),
            _ => None,
        }
    }
}

/// `repo.<name>.landing.merge`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Merge {
    /// `method`. Default `ff`.
    pub method: MergeMethod,
    /// `push`: push the target after the merge. Default false; `true` is refused in this
    /// version.
    pub push: bool,
}

/// `repo.<name>.landing.merge.method`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MergeMethod {
    /// `ff`: the target is fast-forwarded to the run branch.
    #[default]
    Ff,
    /// `squash`: the run branch becomes one commit on the target.
    Squash,
    /// `no-ff`: a merge commit, even when a fast-forward is possible.
    NoFf,
}

impl MergeMethod {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "ff" => Some(Self::Ff),
            "squash" => Some(Self::Squash),
            "no-ff" => Some(Self::NoFf),
            _ => None,
        }
    }
}

/// `repo.<name>.landing.pr`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PullRequest {
    /// `draft`: open the pull request as a draft. Default true.
    pub draft: bool,
    /// `reviewers`: logins asked for a review. Default empty.
    pub reviewers: Vec<String>,
    /// `labels`: labels put on it. Default empty.
    pub labels: Vec<String>,
}

impl Default for PullRequest {
    fn default() -> Self {
        Self {
            draft: true,
            reviewers: Vec::new(),
            labels: Vec::new(),
        }
    }
}

/// `repo.<name>.mcp_servers.<name>`: one stdio MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpServer {
    /// `command`: the program.
    pub command: String,
    /// `args`. Default empty.
    pub args: Vec<String>,
    /// `env`: variables for the server. Default empty. Secrets never go here (#4).
    pub env: BTreeMap<String, String>,
}

/// `[budget]`: configured caps, in input-equivalent tokens (eqv) and wall-clock time (#7).
/// Measured limit windows replace the daily cap in #10.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Budget {
    /// `item_eqv`: per item; exceeding it kills the session and sends the item to Needs you.
    /// Default `None`: no cap.
    pub item_eqv: Option<u64>,
    /// `item_wall_clock`: per item, the same. Default `None`.
    pub item_wall_clock: Option<Duration>,
    /// `daily_eqv`: reaching it parks the queue until local midnight. Default `None`.
    pub daily_eqv: Option<u64>,
    /// `reserve_eqv`: headroom kept for interactive work, subtracted from the daily cap.
    /// Default 0; less than `daily_eqv` when both are set.
    pub reserve_eqv: u64,
}

/// `[daemon]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Daemon {
    /// `auto_pause`: pause a repository while an interactive session works in its checkout.
    /// Default true.
    pub auto_pause: bool,
    /// `auto_pause_idle`: how long an interactive session stays idle before the automatic
    /// pause clears. Default 30 minutes.
    pub auto_pause_idle: Duration,
    /// `question_grace`: how long a session lives after a blocking question. Default 30 s.
    pub question_grace: Duration,
    /// `kill_grace`: between asking a child to stop and killing its tree. Default 10 s.
    pub kill_grace: Duration,
    /// `hello_timeout`: how long `bosun-mcp` has to say hello after spawn. Default 30 s.
    pub hello_timeout: Duration,
    /// `park_cap`: uncounted parks and resumes per item before it goes to Needs you. Default 5.
    pub park_cap: u32,
    /// `mcp_call_cap`: calls of each agent tool per attempt. Default 20.
    pub mcp_call_cap: u32,
    /// `fast_death_sessions`: the fast-death breaker (#7) pauses a repository after this many
    /// sessions in a row, across its items, crashed by themselves within
    /// [`Daemon::fast_death_within`] of their spawn. Default 3. Daemon-wide.
    pub fast_death_sessions: u32,
    /// `fast_death_within`: how soon after its spawn a crash counts toward the breaker.
    /// Default 60 s. Daemon-wide.
    pub fast_death_within: Duration,
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            auto_pause: true,
            auto_pause_idle: Duration::from_mins(30),
            question_grace: Duration::from_secs(30),
            kill_grace: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(30),
            park_cap: 5,
            mcp_call_cap: 20,
            fast_death_sessions: 3,
            fast_death_within: Duration::from_mins(1),
        }
    }
}

/// `[retention]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Retention {
    /// `reingest_window`: how far back `bosun reingest` is expected to reach; `bosun doctor`
    /// warns when a provider deletes transcripts sooner (#3).
    pub reingest_window: Duration,
}

/// Parses and validates the text of a configuration file. `probe` tells whether each
/// database directory is on a local disk; [`SystemProbe`] is the real one.
///
/// # Errors
///
/// [`Error::Invalid`] with every problem found: malformed TOML (one problem, with its line
/// and column), a value of the wrong shape, or a failed validation.
pub fn parse(text: &str, probe: &dyn DiskProbe) -> Result<Config, Error> {
    let mut problems = Vec::new();
    let Some(draft) = read::read(text, &mut problems) else {
        return Err(Error::Invalid(problems));
    };
    validate::validate(&draft, probe, &mut problems);
    if !problems.is_empty() {
        problems.sort_by_key(|p| p.location.map(|at| (at.line, at.column)));
        return Err(Error::Invalid(problems));
    }

    let path = |value: Option<read::PathValue>| value.map(|v| v.path);
    let identities = draft
        .identities
        .into_iter()
        .filter_map(|i| {
            Some((
                i.name,
                Identity {
                    claude_config_dir: i.claude_config_dir?.path,
                    database: i.database?.path,
                    github_host: i.github_host,
                    github_login: i.github_login,
                    gh_config_dir: path(i.gh_config_dir),
                    git_config: path(i.git_config),
                    ssh_key: path(i.ssh_key),
                    codex_home: path(i.codex_home),
                    remote_allowlist: i.remote_allowlist,
                    author_allowlist: i.author_allowlist,
                },
            ))
        })
        .collect();
    let repos = draft
        .repos
        .into_iter()
        .filter_map(|r| {
            let path = r.path?;
            Some((
                r.name,
                Repo {
                    path: path.path,
                    key: path.key,
                    identity: r.identity?.0,
                    queue: r.queue,
                    model: r.model,
                    effort: r.effort,
                    verdict: r.verdict,
                    policy: r.policy,
                    build_dirs: r.build_dirs,
                    landing: r.landing.landing,
                    mcp_servers: r.mcp_servers,
                    worktree: r.worktree.and_then(|w| {
                        Some(Worktree {
                            root: w.root?.path,
                            setup: w.setup,
                            teardown: w.teardown,
                            disk_floor: w.disk_floor,
                            shared_build: w.shared_build.and_then(|s| {
                                Some(SharedBuild {
                                    dir: s.dir?.path,
                                    resource: s.resource?.0,
                                    env: s.env,
                                })
                            }),
                        })
                    }),
                    sweep: r.sweep,
                },
            ))
        })
        .collect();
    Ok(Config {
        identities,
        repos,
        retention: Retention {
            reingest_window: draft.reingest_window,
        },
        budget: draft.budget,
        daemon: draft.daemon,
        resources: draft.resources,
        notices: draft.notices,
    })
}

/// Reads and parses the file at `path`; see [`parse`].
///
/// # Errors
///
/// [`Error::Read`] when the file cannot be read, otherwise as [`parse`].
pub fn load(path: &Path, probe: &dyn DiskProbe) -> Result<Config, Error> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&text, probe)
}

/// The outcome of [`reload`]: the configuration in force afterwards, and why the new text was
/// refused, if it was.
#[derive(Debug)]
#[non_exhaustive]
pub struct Reload {
    /// The new configuration when the text is valid; otherwise the previous one, unchanged.
    pub config: Config,
    /// Why the text was refused: every problem, each with its line and column where it has
    /// one. The daemon reports it as an event and a status line (#4).
    pub error: Option<Error>,
}

/// Re-reads the configuration after the file changed. An invalid edit never replaces a valid
/// configuration: a broken hand edit leaves `previous` in force and returns the error (#4).
#[must_use]
pub fn reload(previous: Config, text: &str, probe: &dyn DiskProbe) -> Reload {
    match parse(text, probe) {
        Ok(config) => Reload {
            config,
            error: None,
        },
        Err(error) => {
            let config = if cfg!(mutant_config_reload_adopts_invalid) {
                // Mutant: the refused edit is adopted, as if it named nothing.
                drop(previous);
                Config {
                    identities: BTreeMap::new(),
                    repos: BTreeMap::new(),
                    retention: Retention {
                        reingest_window: read::DEFAULT_REINGEST_WINDOW,
                    },
                    budget: Budget::default(),
                    daemon: Daemon::default(),
                    resources: BTreeMap::new(),
                    notices: Vec::new(),
                }
            } else {
                previous
            };
            Reload {
                config,
                error: Some(error),
            }
        }
    }
}

/// Where the configuration file is on this platform (see the crate documentation), or `None`
/// when the environment names no home directory.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    default_path_in(&|name| std::env::var_os(name))
}

fn default_path_in(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(path) = var("BOSUN_CONFIG") {
        return Some(path);
    }
    let absolute = |name: &str| var(name).filter(|path| path.is_absolute());
    let dir = if cfg!(windows) {
        absolute("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        absolute("HOME")?
            .join("Library")
            .join("Application Support")
    } else {
        absolute("XDG_CONFIG_HOME").or_else(|| Some(absolute("HOME")?.join(".config")))?
    };
    Some(dir.join("bosun").join("config.toml"))
}
