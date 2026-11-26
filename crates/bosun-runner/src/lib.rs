//! The runners (#7, #52, #58): the runner trait, the provider runners as modules behind
//! features (Claude first), the prompt builder and the verdict executor ([`verdict`]).
//!
//! A [`Runner`] turns a [`SessionContext`] into a [`Spawn`]
//! (argv, environment additions, working directory, standard input), reads the session id and
//! the live tail from the child's standard output line by line, and classifies the exit from
//! the process status and the last lines of that output ([`Tail`]). `bosund` does the spawning
//! through `bosun-proc` (#50) and owns the bookkeeping. The verdict executor is the one part
//! that spawns, through a `bosun-proc` [`bosun_proc::Spawner`] the caller passes in.
//!
//! Of the workspace crates this one may reach `bosun-proc` only, never `bosun-store` (#12).
//!
//! # What the classification trusts
//!
//! The exit classes are those of #7: [`Exit::Ok`], [`Exit::Question`], [`Exit::Limit`],
//! [`Exit::Auth`], [`Exit::Crash`]. A runner never reads [`Exit::Question`] from output:
//! a question is the transition a blocking `ask_question` call of `bosun-mcp` records, and
//! nothing the agent writes in its messages ("I am done", "QUESTION: ...") changes the class
//! (invariant 2 of #1). Usage is never read from the stream either: the transcript ingested by
//! stage 0 is the single source of the bill.
#![cfg_attr(
    test,
    allow(clippy::disallowed_methods, reason = "a test's own children")
)]

#[cfg(feature = "claude")]
pub mod claude;
pub mod prompt;
pub mod verdict;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::time::SystemTime;

/// A session id: a UUID in its canonical lowercase text form.
///
/// The daemon assigns it before spawn and persists it with the attempt, so a crash right after
/// spawn still knows which session to resume.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

/// The text given to [`SessionId::parse`] is not a UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAUuid(pub String);

impl fmt::Display for NotAUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a UUID", self.0)
    }
}

impl std::error::Error for NotAUuid {}

impl SessionId {
    /// Parses the 8-4-4-4-12 hexadecimal form, either case; stores it lowercase.
    ///
    /// # Errors
    ///
    /// [`NotAUuid`] when the text has another shape.
    pub fn parse(text: &str) -> Result<Self, NotAUuid> {
        let groups: Vec<&str> = text.split('-').collect();
        let shaped = groups.len() == 5
            && groups
                .iter()
                .zip([8, 4, 4, 4, 12])
                .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
        if shaped {
            Ok(Self(text.to_ascii_lowercase()))
        } else {
            Err(NotAUuid(text.to_owned()))
        }
    }

    /// A version 4 UUID from 16 random bytes the caller draws (this crate has no random
    /// source): the version and variant bits are set, the other 122 bits are the input's.
    #[must_use]
    pub fn from_random_bytes(mut bytes: [u8; 16]) -> Self {
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = bytes.iter().fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
        Self(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        ))
    }

    /// The canonical text form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What to spawn: the program, its arguments, the environment additions, the working directory
/// and the bytes for standard input.
///
/// The environment here is only what the runner adds; the base environment is the agent
/// environment of #54, built from an allowlist by `bosund`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The program, as resolved by the caller.
    pub program: PathBuf,
    /// The arguments, without the program.
    pub args: Vec<OsString>,
    /// Variables added to the agent environment.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory.
    pub cwd: PathBuf,
    /// Written to the child's standard input, which is then closed. The prompt goes here rather
    /// than in argv: it can exceed the Windows command-line limit (32 767 characters).
    pub stdin: Option<String>,
}

/// The tool permissions of a session, from the repository's permission policy (#7).
///
/// The lists are passed to the agent CLI; they are not the enforcement. The post-hoc checks of
/// the verdict and the credential-free agent environment (#54) are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Tool rules the session may use without asking, in the provider's rule syntax.
    pub allow: Vec<String>,
    /// Tool rules the session may never use.
    pub deny: Vec<String>,
    /// Whether `git push` is allowed. Off by default: push is denied (#7).
    pub allow_push: bool,
    /// Repository-relative, `/`-separated paths the session may not edit: the repository's
    /// protected paths less the card's `allow-protected` (#7). A runner turns them into deny
    /// rules; the guard before the verdict is the enforcement.
    pub protected: Vec<String>,
}

impl Default for ToolPolicy {
    /// The built-in tools that read, search and edit the working tree, and the shells; push
    /// denied.
    fn default() -> Self {
        Self {
            allow: [
                "Read",
                "Glob",
                "Grep",
                "Edit",
                "Write",
                "NotebookEdit",
                "Bash",
                "PowerShell",
            ]
            .map(str::to_owned)
            .to_vec(),
            deny: Vec::new(),
            allow_push: false,
            protected: Vec::new(),
        }
    }
}

/// An MCP server of a session, in the provider's server configuration syntax (for Claude Code,
/// the value of one entry of `mcpServers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The server's name; its tools are `mcp__<name>__<tool>`.
    pub name: String,
    /// The server's configuration as JSON text, such as
    /// `{"command": "bosun-mcp", "args": ["--item", "..."]}`.
    pub config: String,
}

/// Everything a runner needs to build the command of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    /// The working directory: the repository's checkout.
    pub cwd: PathBuf,
    /// The pre-assigned session id.
    pub session_id: SessionId,
    /// The user prompt ([`prompt::Prompt::user`]), sent on standard input.
    pub prompt: String,
    /// The file holding [`prompt::Prompt::system`], which the daemon writes before spawn.
    pub system_prompt_file: PathBuf,
    /// The per-session MCP configuration file, which the daemon writes before spawn (for
    /// Claude, from [`claude::mcp_config`]).
    pub mcp_config_file: PathBuf,
    /// The names of the servers in that file other than `bosun`: the repository's
    /// `repo.<name>.mcp_servers` (#4). Their tools are pre-allowed.
    pub repo_mcp_servers: Vec<String>,
    /// The tool permissions.
    pub policy: ToolPolicy,
    /// The model, when the repository names one; otherwise the CLI's default.
    pub model: Option<String>,
    /// The effort level (`--effort`), when the card or the repository names one (one of
    /// `bosun_core::EFFORT_LEVELS`); otherwise the CLI's default.
    pub effort: Option<String>,
}

/// How the child process ended, as the spawner saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    /// The exit code; `None` when the process was ended by a signal or killed without one.
    pub code: Option<i32>,
}

/// The class of a session's end (#7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// The session ended normally; the daemon runs the verdict.
    Ok,
    /// The agent asked a blocking question. Never returned by [`Runner::classify_exit`]: it is
    /// the transition `ask_question` records, never something read from output.
    Question,
    /// A usage limit: park until `reset` (or a default backoff when `None`), then resume.
    Limit {
        /// When the limit resets, if the output said.
        reset: Option<SystemTime>,
    },
    /// Authentication failed or expired: Needs you, retrying cannot help.
    Auth,
    /// Anything else.
    Crash {
        /// What was seen, for the attempt record.
        reason: String,
    },
}

/// The last lines of a session's standard output, bounded.
#[derive(Debug, Clone)]
pub struct Tail {
    lines: VecDeque<String>,
    cap: usize,
}

impl Tail {
    /// The default bound: the end of a session (a limit or error event, the result) is a few
    /// lines, so this keeps far more than classification reads.
    pub const DEFAULT_LINES: usize = 256;

    /// An empty tail that keeps the last `cap` lines (at least one).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Appends one line of output (without its line break), dropping the oldest beyond the
    /// bound.
    pub fn push(&mut self, line: impl Into<String>) {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    /// A tail of the lines of `text`, with [`Tail::DEFAULT_LINES`].
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut tail = Self::new(Self::DEFAULT_LINES);
        for line in text.lines() {
            tail.push(line);
        }
        tail
    }

    /// The kept lines, oldest first.
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }
}

impl Default for Tail {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LINES)
    }
}

/// One entry of the live tail (#4: the Session view and the live-tail event), parsed from a line
/// of output. Tool calls are collapsed to their names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEntry {
    /// The session started.
    Started {
        /// The model the CLI reports.
        model: Option<String>,
    },
    /// Text the agent wrote.
    Text(String),
    /// A tool call.
    Tool {
        /// The tool's name.
        name: String,
    },
    /// A tool call the CLI refused under the permission lists.
    Denied {
        /// The tool's name.
        tool: String,
    },
    /// An error the provider reported.
    Error(String),
    /// The session's end, as the CLI reported it.
    Ended {
        /// Whether the CLI reported an error.
        is_error: bool,
    },
}

/// A tool call the agent CLI refused under the permission lists (#7: refusals per attempt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// The tool's name (`Bash`, `Edit`, `mcp__docs__search`).
    pub tool: String,
    /// The call's subject (a command line, a path, a URL), capped at [`Denial::TARGET_CAP`]
    /// characters; `None` when the stream did not say.
    pub target: Option<String>,
    /// Why the CLI refused it (for Claude Code, the event's `decision_reason_type`, such as
    /// `rule`), when the stream still says.
    pub reason: Option<String>,
}

impl Denial {
    /// The longest target kept, in characters.
    pub const TARGET_CAP: usize = 500;
}

/// A coding-agent CLI driven unattended (#7).
pub trait Runner {
    /// The command of a new session under the pre-assigned [`SessionContext::session_id`].
    fn command(&self, ctx: &SessionContext) -> Spawn;

    /// The session id one line of output reports, if it reports one. The daemon persists it as
    /// soon as it is known; for a runner that pre-assigns ids it confirms the assignment.
    fn session_id(&self, line: &str) -> Option<SessionId>;

    /// The live-tail entries of one line of output, in order; none for a line that carries
    /// nothing to show.
    fn live_entries(&self, line: &str) -> Vec<LiveEntry>;

    /// The class of the session's end. Never [`Exit::Question`].
    fn classify_exit(&self, exit: ProcessExit, tail: &Tail) -> Exit;

    /// The tool calls the CLI refused during the session, in the order the stream lists them,
    /// read from the session's tail; none when the tail says nothing about refusals.
    fn denials(&self, tail: &Tail) -> Vec<Denial>;

    /// The command that resumes session `id` (after a limit, a sleep, a crash).
    fn resume_command(&self, id: &SessionId, ctx: &SessionContext) -> Spawn;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_parses_and_normalises() {
        let id = SessionId::parse("00000000-0000-4000-8000-0000000000AB").unwrap();
        assert_eq!(id.as_str(), "00000000-0000-4000-8000-0000000000ab");
        for bad in [
            "",
            "00000000-0000-4000-8000",
            "00000000-0000-4000-8000-00000000000g",
            "000000000-000-4000-8000-000000000000",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_random_session_id_is_version_4() {
        let id = SessionId::from_random_bytes([0xff; 16]);
        assert_eq!(id.as_str(), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(SessionId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        let mut tail = Tail::new(2);
        for line in ["a", "b", "c"] {
            tail.push(line);
        }
        assert_eq!(tail.lines().collect::<Vec<_>>(), ["b", "c"]);
    }
}
