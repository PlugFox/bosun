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
//! verdict = { timeout = "30m" } # and `shell`, default `sh` (from Git on Windows)
//! landing = { mode = "local" }  # the only mode of this version; merge and pr are #8
//! worktree = { root = 'C:\wt\bosun', setup = "cargo fetch", disk_floor = "20GB" }  # #8
//!
//! [repo.bosun.policy]
//! protected = ["tests/harness", "xtask/src/lint.rs"]  # `.gitignore` is always added
//! push = false                  # default
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
//! [resource.target]             # a leasable resource (#8); read, not yet leased
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
//! - `landing.mode` other than `local` is refused. An unknown key of a `policy` is refused, so
//!   that a misspelt `protected` cannot leave paths unprotected; so is an unknown key of a
//!   `resource` or of a `worktree`.
//! - `repo.<name>.worktree` (#8): where the repository's item worktrees go. `root` is required
//!   and lies outside the repository's `path`; `setup` and `teardown` are shell commands run in a
//!   new worktree and before one is removed; `disk_floor` is the free space below which no new
//!   worktree is made. Sizes are a positive whole number and one of `B`, `KB`, `MB`, `GB`, `TB`
//!   (powers of 1000) or `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024). Nothing acts on these keys
//!   yet: items still run in the main checkout.
//! - `[resource.<name>]` (#8): a resource items may name in `needs`. `kind` is required. Nothing
//!   leases them yet: a card that needs one keeps being refused until the lease manager exists.
//! - `notify`, and every other key of #4 that a later stage reads (`repo.<name>.report`,
//!   `landing.target`, `daemon.http_port`, ...), is accepted and ignored with a [`Notice`]. Any
//!   other section is an error.
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
pub use policy::{ALWAYS_PROTECTED, Policy};

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

/// `repo.<name>.landing` (#8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Landing {
    /// `mode`. Default `local`.
    pub mode: LandingMode,
}

/// `repo.<name>.landing.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LandingMode {
    /// Commits stay on the current branch; nothing is pushed. The only mode of this version:
    /// `merge` and `pr` are refused until #8.
    #[default]
    Local,
}

impl LandingMode {
    fn parse(text: &str) -> Option<Self> {
        (text == "local").then_some(Self::Local)
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
                    verdict: r.verdict,
                    policy: r.policy,
                    build_dirs: r.build_dirs,
                    landing: r.landing,
                    mcp_servers: r.mcp_servers,
                    worktree: r.worktree.and_then(|w| {
                        Some(Worktree {
                            root: w.root?.path,
                            setup: w.setup,
                            teardown: w.teardown,
                            disk_floor: w.disk_floor,
                        })
                    }),
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
