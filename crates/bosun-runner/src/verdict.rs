//! The verdict executor (#7, #58): one run of a snapshot's verdict command, its outcome and its
//! bounded output.
//!
//! [`run`] starts `<shell> -c <command>` through a `bosun-proc` [`Spawner`] in the environment
//! the caller builds (the agent environment of #54), drains stdout and stderr concurrently so
//! the child never blocks on a full pipe, and kills the whole tree when the timeout passes. It
//! runs once and reports: the re-run after a failing run, and `flaky`, are the attempt machine's
//! (`bosun-core`).
//!
//! The shell is an input. D4 resolves it (`sh` of the Git installation on Windows, or
//! `repo.<name>.verdict.shell`) in `bosun-queue`'s `verdict_shell`, the same resolution lint
//! uses; `bosund` passes the result here, since this crate may not depend on `bosun-queue`.
//!
//! # Streams and the verdict line (#44 B)
//!
//! Stdout and stderr are kept apart, each as a bounded tail, and so is the interleaved stream
//! as received (for the attempt comment). The verdict line on the card and in the database is
//! stdout's last non-blank line; stderr's only when stdout printed nothing. A tool's progress on
//! stderr that arrives after the verdict's own match on stdout does not hide the match.
//!
//! # Failing test names (#44 D)
//!
//! A [`FailureParser`] reads the failing test names of a failed run, per test runner; [`Libtest`]
//! is the first. When the re-run of a failed verdict passes, `bosund` stores the names of the
//! failed run as `flaky_test` rows.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use bosun_proc::{ChildSpec, Output, Spawned, Spawner, WaitFuture};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;

use crate::ProcessExit;

/// How long the executor waits, after a kill, for the tree to exit and the pipes to close.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// The size of one read from a pipe.
const CHUNK: usize = 64 * 1024;

/// The bounds of the captured output, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The tail kept of stdout, and of stderr, each.
    pub stream: usize,
    /// The tail kept of the interleaved stream.
    pub interleaved: usize,
}

impl Default for Limits {
    /// 64 KiB each: far more than a verdict line or libtest's failure sections need, and small
    /// enough to keep per attempt.
    fn default() -> Self {
        Self {
            stream: 64 * 1024,
            interleaved: 64 * 1024,
        }
    }
}

/// One verdict run to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictSpec {
    /// The shell, as D4 resolves it; run as `<shell> -c <command>`.
    pub shell: PathBuf,
    /// The verdict command, from the item's snapshot.
    pub command: String,
    /// The child's entire environment (the agent environment of #54).
    pub env: Vec<(OsString, OsString)>,
    /// The working directory: the attempt's checkout.
    pub cwd: PathBuf,
    /// The snapshot's `verdict-timeout`. When it passes, the whole tree is killed.
    pub timeout: Duration,
    /// The bounds of the captured output.
    pub limits: Limits,
    /// The repository's not-ready exit code (`repo.<name>.verdict.unready_exit`, #204): a shell
    /// that exits by itself with exactly this code is [`Outcome::Unready`], not a failure.
    /// `None` (the default) makes every non-zero code a failure.
    pub unready_exit: Option<u8>,
}

/// The outcome of one verdict run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The command exited 0.
    Pass,
    /// The command exited non-zero (other than [`VerdictSpec::unready_exit`]), or was ended by
    /// a signal.
    Fail,
    /// The timeout passed; the tree was killed. Whatever code the tree would have exited with.
    Timeout,
    /// The command exited by itself with [`VerdictSpec::unready_exit`]: a resource it needs
    /// was not ready (#204). Neither a pass nor a failure: the attempt machine runs the verdict
    /// again after a backoff, uncounted, up to a cap.
    Unready,
}

impl Outcome {
    /// The outcome's name, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Timeout => "timeout",
            Self::Unready => "unready",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// The line quoted as the verdict's result (#44 B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictLine {
    /// The stream it came from: stderr only when stdout printed nothing.
    pub stream: Stream,
    /// The line, without trailing whitespace.
    pub text: String,
}

/// The kept tail of one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTail {
    /// The kept text, lossily decoded. When the stream was longer than the bound, the tail
    /// starts at the first line break inside it, so no partial line leads.
    pub text: String,
    /// Every byte the stream carried, kept or not.
    pub total_bytes: u64,
}

impl StreamTail {
    /// Whether bytes were dropped from the front.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total_bytes > self.text.len() as u64
    }

    /// The last line of the kept text that is not blank, without trailing whitespace.
    #[must_use]
    pub fn last_line(&self) -> Option<&str> {
        self.text
            .lines()
            .rev()
            .map(str::trim_end)
            .find(|l| !l.trim_start().is_empty())
    }
}

/// The report of one verdict run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRun {
    /// Pass, fail, timeout or not ready.
    pub outcome: Outcome,
    /// How the shell exited; no code after a timeout or a signal.
    pub exit: ProcessExit,
    /// From spawn to the shell's exit, or to the timeout.
    pub duration: Duration,
    /// The tail of stdout.
    pub stdout: StreamTail,
    /// The tail of stderr.
    pub stderr: StreamTail,
    /// The tail of both streams interleaved as received, for the attempt comment.
    pub interleaved: StreamTail,
}

impl VerdictRun {
    /// The verdict line: stdout's last non-blank line, stderr's only when stdout printed
    /// nothing (#44 B). `None` when neither printed a non-blank line.
    #[must_use]
    pub fn verdict_line(&self) -> Option<VerdictLine> {
        if cfg!(mutant_runner_merge_streams) {
            // The merged tail's last line, attributed to stdout.
            return self.interleaved.last_line().map(|text| VerdictLine {
                stream: Stream::Stdout,
                text: text.to_owned(),
            });
        }
        let line = |stream, tail: &StreamTail| {
            tail.last_line().map(|text| VerdictLine {
                stream,
                text: text.to_owned(),
            })
        };
        line(Stream::Stdout, &self.stdout).or_else(|| line(Stream::Stderr, &self.stderr))
    }

    /// The failing test names `parser` reads from this run's stdout, then its stderr, without
    /// duplicates, in order of first appearance. Only the kept tails are read.
    #[must_use]
    pub fn failing_tests(&self, parser: &dyn FailureParser) -> Vec<String> {
        let mut names = parser.failing_tests(&self.stdout.text);
        for name in parser.failing_tests(&self.stderr.text) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

/// Reads the names of the failing tests from a test runner's output (#44 D).
pub trait FailureParser {
    /// The failing test names in `output`, without duplicates, in order of first appearance.
    fn failing_tests(&self, output: &str) -> Vec<String>;
}

/// The parser of Rust's libtest output: `test <name> ... FAILED` and `---- <name> stdout ----`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Libtest;

impl Libtest {
    fn name(line: &str) -> Option<&str> {
        let line = line.trim();
        let name = if let Some(rest) = line.strip_prefix("test ") {
            rest.strip_suffix(" ... FAILED")?
        } else {
            line.strip_prefix("---- ")?.strip_suffix(" stdout ----")?
        };
        let name = name.strip_suffix(" - should panic").unwrap_or(name).trim();
        (!name.is_empty()).then_some(name)
    }
}

impl FailureParser for Libtest {
    fn failing_tests(&self, output: &str) -> Vec<String> {
        if cfg!(mutant_runner_no_flaky_names) {
            return Vec::new();
        }
        let mut names: Vec<String> = Vec::new();
        for name in output.lines().filter_map(Self::name) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
        names
    }
}

/// The last `cap` bytes of a stream, and how many it carried.
#[derive(Debug)]
struct ByteTail {
    buf: Vec<u8>,
    cap: usize,
    total: u64,
}

impl ByteTail {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        self.buf.extend_from_slice(bytes);
        // Amortised: drop the front only once the buffer holds twice the bound.
        if self.buf.len() > self.cap.saturating_mul(2).max(CHUNK) {
            let cut = self.buf.len() - self.cap;
            self.buf.drain(..cut);
        }
    }

    fn finish(self) -> StreamTail {
        let kept = &self.buf[self.buf.len().saturating_sub(self.cap)..];
        let kept = if self.total > kept.len() as u64 {
            kept.iter()
                .position(|&b| b == b'\n')
                .map_or(kept, |i| &kept[i + 1..])
        } else {
            kept
        };
        StreamTail {
            text: String::from_utf8_lossy(kept).into_owned(),
            total_bytes: self.total,
        }
    }
}

#[derive(Debug)]
struct Capture {
    stdout: ByteTail,
    stderr: ByteTail,
    interleaved: ByteTail,
}

impl Capture {
    fn push(&mut self, stream: Stream, bytes: &[u8]) {
        match stream {
            Stream::Stdout => self.stdout.push(bytes),
            Stream::Stderr => self.stderr.push(bytes),
        }
        self.interleaved.push(bytes);
    }
}

/// Both pipes of the child, each until its end of file.
struct Pipes {
    stdout: Option<Output>,
    stderr: Option<Output>,
    out_buf: Vec<u8>,
    err_buf: Vec<u8>,
}

/// Why [`Pipes::pump`] returned.
enum Pumped {
    /// The child exited.
    Exited(io::Result<ExitStatus>),
    /// Both pipes closed and no wait was asked for.
    Closed,
    /// The deadline passed.
    Deadline,
}

impl Pipes {
    /// Reads both pipes into `capture` until the child exits (when `wait` is given), both pipes
    /// close (when it is not), or `deadline` passes.
    async fn pump(
        &mut self,
        capture: &mut Capture,
        mut wait: Option<&mut WaitFuture<'_>>,
        deadline: Instant,
    ) -> Pumped {
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        loop {
            if wait.is_none() && self.stdout.is_none() && self.stderr.is_none() {
                return Pumped::Closed;
            }
            tokio::select! {
                read = read(self.stdout.as_mut(), &mut self.out_buf), if self.stdout.is_some() => {
                    match read {
                        Ok(n) if n > 0 => capture.push(Stream::Stdout, &self.out_buf[..n]),
                        _ => self.stdout = None,
                    }
                }
                read = read(self.stderr.as_mut(), &mut self.err_buf), if self.stderr.is_some() => {
                    match read {
                        Ok(n) if n > 0 => capture.push(Stream::Stderr, &self.err_buf[..n]),
                        _ => self.stderr = None,
                    }
                }
                status = async { wait.as_mut().expect("guarded").as_mut().await }, if wait.is_some() => {
                    return Pumped::Exited(status);
                }
                () = &mut sleep => return Pumped::Deadline,
            }
        }
    }
}

async fn read(pipe: Option<&mut Output>, buf: &mut [u8]) -> io::Result<usize> {
    match pipe {
        Some(pipe) => pipe.read(buf).await,
        None => Ok(0),
    }
}

/// Runs the verdict of `spec` once through `spawner` and reports it.
///
/// When the timeout passes, the whole tree is killed and the run is [`Outcome::Timeout`]. When
/// the shell exits by itself, whatever it left running in its container is killed too, and the
/// pipes are drained to their end: the verdict is the shell's exit status ([`Outcome::Unready`]
/// for [`VerdictSpec::unready_exit`], checked first).
///
/// # Errors
///
/// When the shell cannot be started, waiting for it fails, or the kill after a timeout is
/// refused. The tree is killed before the error returns.
pub async fn run(spawner: &dyn Spawner, spec: &VerdictSpec) -> io::Result<VerdictRun> {
    let child = ChildSpec {
        program: spec.shell.clone(),
        args: vec!["-c".into(), spec.command.clone().into()],
        env: spec.env.clone(),
        cwd: spec.cwd.clone(),
        stdin: None,
    };
    let timeout = if cfg!(mutant_runner_no_verdict_timeout) {
        // Never: a verdict that hangs holds the attempt forever.
        Duration::from_hours(365 * 24)
    } else {
        spec.timeout
    };
    let started = Instant::now();
    let Spawned {
        mut tree,
        stdout,
        stderr,
        ..
    } = spawner.spawn(&child)?;
    let mut capture = Capture {
        stdout: ByteTail::new(spec.limits.stream),
        stderr: ByteTail::new(spec.limits.stream),
        interleaved: ByteTail::new(spec.limits.interleaved),
    };
    let mut pipes = Pipes {
        stdout: Some(stdout),
        stderr: Some(stderr),
        out_buf: vec![0; CHUNK],
        err_buf: vec![0; CHUNK],
    };
    let ended = {
        let mut wait = tree.wait();
        pipes
            .pump(&mut capture, Some(&mut wait), started + timeout)
            .await
    };
    let duration = started.elapsed();
    let killed = tree.kill_tree();
    let (outcome, exit) = match ended {
        Pumped::Exited(status) => {
            let code = status?.code();
            let outcome = if spec
                .unready_exit
                .is_some_and(|u| code == Some(i32::from(u)))
            {
                Outcome::Unready
            } else if code == Some(0) {
                Outcome::Pass
            } else {
                Outcome::Fail
            };
            (outcome, ProcessExit { code })
        }
        Pumped::Deadline => {
            killed?;
            let mut wait = tree.wait();
            pipes
                .pump(&mut capture, Some(&mut wait), Instant::now() + KILL_GRACE)
                .await;
            (Outcome::Timeout, ProcessExit { code: None })
        }
        Pumped::Closed => unreachable!("a pump with a wait returns on the exit or the deadline"),
    };
    // The tree is gone, or going: the pipes close once every holder of them has exited.
    pipes
        .pump(&mut capture, None, Instant::now() + KILL_GRACE)
        .await;
    Ok(VerdictRun {
        outcome,
        exit,
        duration,
        stdout: capture.stdout.finish(),
        stderr: capture.stderr.finish(),
        interleaved: capture.interleaved.finish(),
    })
}

#[cfg(test)]
mod tests;
