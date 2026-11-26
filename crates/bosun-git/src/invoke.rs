//! The one `git` invoker (#7 "Git's own configuration", D3 of #7): every git command of the
//! daemon is built here.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The hooks directory every command is given: the null device, which is not a directory and
/// so holds no hook, and which nothing (an agent included) can plant a file in.
pub const NO_HOOKS: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// A repository that git commands run in. See the crate documentation for what every command
/// is run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    repo: PathBuf,
}

impl Git {
    /// Commands in the repository (or worktree) at `repo`.
    #[must_use]
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self { repo: repo.into() }
    }

    /// The repository commands run in.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The hardened `git` command, with no arguments of its own yet: `-C <repo>`, hooks and
    /// fsmonitor off, no terminal prompt, no optional locks, none of the inherited variables
    /// that redirect git to another repository, standard input closed and both outputs piped.
    /// Callers that need more than [`Git::run`] and [`Git::optional`] start from this.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = crate::child::command("git");
        command.arg("-C").arg(&self.repo);
        #[cfg(not(mutant_git_repo_hooks))]
        command
            .args(["-c", &format!("core.hooksPath={NO_HOOKS}")])
            .args(["-c", "core.fsmonitor=false"]);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs `git args`, feeding it `stdin`; returns its standard output.
    ///
    /// # Errors
    ///
    /// `git` cannot be started, or it exits unsuccessfully.
    pub fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, Error> {
        self.run_in(self.command(), args, stdin)
    }

    /// [`Git::run`] with `command`, a [`Git::command`] the caller added to (an environment
    /// variable, say).
    pub(crate) fn run_in(
        &self,
        command: Command,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, Error> {
        let out = Self::exec_in(command, args, stdin)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(self.failed(args, &out.stderr))
        }
    }

    /// [`Git::run`] with bytes in and out: for output that need not be text (`cat-file
    /// --batch`).
    pub(crate) fn run_bytes(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, Error> {
        let out = Self::exec_bytes(self.command(), args, stdin)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(self.failed(args, &out.stderr))
        }
    }

    /// Runs a query that exits 1 with no output when there is nothing to report (`config
    /// --get`, `symbolic-ref --quiet`, `rev-parse --verify --quiet`): its trimmed output, or
    /// `None` on exit 1 or empty output.
    ///
    /// # Errors
    ///
    /// `git` cannot be started, or it exits with a status other than 0 and 1.
    pub fn optional(&self, args: &[&str]) -> Result<Option<String>, Error> {
        let out = self.exec(args, None)?;
        match out.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                Ok((!text.is_empty()).then_some(text))
            }
            Some(1) => Ok(None),
            _ => Err(self.failed(args, &out.stderr)),
        }
    }

    /// Runs a yes-or-no query that exits 0 for yes and 1 for no (`merge-base --is-ancestor`).
    ///
    /// # Errors
    ///
    /// `git` cannot be started, or it exits with a status other than 0 and 1.
    pub fn test(&self, args: &[&str]) -> Result<bool, Error> {
        let out = self.exec(args, None)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.failed(args, &out.stderr)),
        }
    }

    /// An [`Error::Output`] for this repository.
    #[must_use]
    pub fn unexpected(&self, what: String) -> Error {
        Error::Output {
            repo: self.repo.clone(),
            what,
        }
    }

    fn failed(&self, args: &[&str], stderr: &[u8]) -> Error {
        Error::Git {
            repo: self.repo.clone(),
            command: args.first().copied().unwrap_or_default().to_owned(),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }
    }

    fn exec(&self, args: &[&str], stdin: Option<&str>) -> Result<std::process::Output, Error> {
        Self::exec_in(self.command(), args, stdin)
    }

    fn exec_in(
        command: Command,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<std::process::Output, Error> {
        Self::exec_bytes(command, args, stdin.map(str::as_bytes))
    }

    fn exec_bytes(
        mut command: Command,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<std::process::Output, Error> {
        command.args(args);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().map_err(|source| Error::Spawn { source })?;
        // Written from another thread: a large input and a large output would otherwise block
        // each other on the pipes.
        let writer = match (stdin, child.stdin.take()) {
            (Some(text), Some(mut pipe)) => {
                let text = text.to_owned();
                Some(std::thread::spawn(move || pipe.write_all(&text)))
            }
            _ => None,
        };
        let out = child
            .wait_with_output()
            .map_err(|source| Error::Spawn { source })?;
        if let Some(writer) = writer {
            // A failed write shows up as a failed command; its own error adds nothing.
            let _ = writer.join();
        }
        Ok(out)
    }
}

/// What can go wrong running git.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// `git` could not be started (it is not on `PATH`, for example).
    Spawn {
        /// Why the process did not start.
        source: std::io::Error,
    },
    /// A `git` command failed.
    Git {
        /// The repository it ran in.
        repo: PathBuf,
        /// The git subcommand.
        command: String,
        /// What it printed on standard error.
        stderr: String,
    },
    /// A `git` command printed something the caller does not understand.
    Output {
        /// The repository it ran in.
        repo: PathBuf,
        /// What was not understood.
        what: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { source } => write!(f, "cannot run git: {source}"),
            Self::Git {
                repo,
                command,
                stderr,
            } => write!(
                f,
                "{}: git {command} failed: {}",
                repo.display(),
                stderr.trim()
            ),
            Self::Output { repo, what } => {
                write!(f, "{}: unexpected git output: {what}", repo.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source } => Some(source),
            _ => None,
        }
    }
}
