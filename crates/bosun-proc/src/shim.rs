//! The session shim (#8, "Process containment"; #129): a small process between the daemon and an
//! agent, so that the agent's container, its output and its exit outlive the daemon.
//!
//! The daemon writes a [`Spec`] to a file and starts `bosun-shim <spec>`. The shim:
//!
//! 1. creates the container (the crate's Job Object with kill-on-close on Windows, a process
//!    group on Unix) and starts the child in it, held before its first instruction;
//! 2. writes the [`Record`] next to the spec ([`record_path`]): its own pid and start time, the
//!    child's, the output files and the format version, and only then lets the child run, so a
//!    child that runs is always on record;
//! 3. sends the child's stdout and stderr straight to two files in the output directory, which
//!    the daemon tails by offset;
//! 4. enforces the wall-clock cap itself: with the daemon dead nothing else would;
//! 5. when the child exits, writes its exit into the record, kills what is left of the
//!    container and exits with the child's code.
//!
//! Kill-on-close is on the shim's own job handle: if the shim dies, cleanly or not, everything
//! in the container dies with it. On Unix the child dies with the shim through the
//! parent-death signal on Linux; its descendants do not (see the crate documentation).
//!
//! The shim does not detach from the job it runs in (`CREATE_BREAKAWAY_FROM_JOB`): the daemon's
//! own job forbids breakaway, and a plain child already outlives its parent on Windows.
//!
//! # Files
//!
//! The spec and the record are text, one `key=value` per line after a header line that names
//! the format and its version. Values are percent-encoded (`%`, `=`, CR and LF), so any UTF-8
//! value fits on one line; a value that is not UTF-8 is refused. The record is replaced
//! atomically (written to a temporary file, then renamed), so a reader never sees half of it.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::key::{ProcessKey, StartTime};
use crate::sys;

const SPEC_HEADER: &str = "bosun-shim-spec";
const RECORD_HEADER: &str = "bosun-shim-record";

/// The version of the record format, the number on its header line.
pub const RECORD_VERSION: u32 = 1;
/// The version of the spec format.
pub const SPEC_VERSION: u32 = 1;

/// The name of the child's stdout file in the output directory.
pub const STDOUT_FILE: &str = "stdout";
/// The name of the child's stderr file in the output directory.
pub const STDERR_FILE: &str = "stderr";

/// What the shim starts: as [`crate::ChildSpec`], plus where the output goes and how long the
/// child may run. Nothing of the shim's environment is inherited, and stdin is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The program, a path or a name looked up on the shim's `PATH`.
    pub program: PathBuf,
    /// The arguments after the program.
    pub args: Vec<OsString>,
    /// The child's entire environment.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory.
    pub cwd: PathBuf,
    /// The directory of the output files ([`STDOUT_FILE`], [`STDERR_FILE`]); created if
    /// missing.
    pub output_dir: PathBuf,
    /// The wall-clock cap: the container is killed when the child has run this long.
    pub wall_clock: Duration,
}

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this code (on Windows also when it was killed: the kill sets the code).
    Code(i32),
    /// Unix: a signal killed it.
    Signal(i32),
}

impl Exit {
    /// The code the shim exits with: the child's own, or `128 + signal` as a shell reports it.
    #[must_use]
    pub const fn shim_code(self) -> i32 {
        match self {
            Self::Code(code) => code,
            Self::Signal(signal) => 128 + signal,
        }
    }
}

/// What the shim writes next to the spec: before the child runs, and again when it has exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The format version, [`RECORD_VERSION`] for a record this build writes.
    pub version: u32,
    /// The shim's pid and start time.
    pub shim: ProcessKey,
    /// The child's pid and start time.
    pub child: ProcessKey,
    /// The child's stdout file.
    pub stdout: PathBuf,
    /// The child's stderr file.
    pub stderr: PathBuf,
    /// How the child ended; `None` while it runs (or when the shim died first).
    pub exit: Option<Exit>,
    /// Whether the shim killed the child at the wall-clock cap.
    pub capped: bool,
}

/// The record of the spec at `spec`: the spec's file name with `.record` appended, in the same
/// directory.
#[must_use]
pub fn record_path(spec: &Path) -> PathBuf {
    let mut name = spec.file_name().map_or_else(OsString::new, OsString::from);
    name.push(".record");
    spec.with_file_name(name)
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '%' | '=' | '\r' | '\n' => {
                write!(out, "%{:02X}", u32::from(c)).expect("writing to a string");
            }
            c => out.push(c),
        }
    }
    out
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

fn decode(value: &str) -> io::Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = value
                .get(i + 1..i + 3)
                .ok_or_else(|| invalid("a truncated escape"))?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| invalid("a malformed escape"))?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid("an escape that is not UTF-8"))
}

fn utf8(value: &std::ffi::OsStr) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a value that is not UTF-8"))
}

/// The `key=value` lines of a file after its header, which must name `header` and `version`;
/// the values still encoded.
fn lines<'a>(text: &'a str, header: &str, version: u32) -> io::Result<Vec<(&'a str, &'a str)>> {
    let mut lines = text.lines();
    let first = lines.next().ok_or_else(|| invalid("an empty file"))?;
    let found = first
        .strip_prefix(header)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or_else(|| invalid(format!("not a {header} file")))?;
    if found != version.to_string() {
        return Err(invalid(format!("{header} version {found}, not {version}")));
    }
    lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| invalid(format!("a line without '=': {line}")))?;
            Ok((key, value))
        })
        .collect()
}

fn number<T: std::str::FromStr>(key: &str, value: &str) -> io::Result<T> {
    value
        .parse()
        .map_err(|_| invalid(format!("{key} is not a number: {value}")))
}

/// Writes `text` to `path` through a temporary file in the same directory and a rename.
fn replace(path: &Path, text: &str) -> io::Result<()> {
    let mut name = path.file_name().map_or_else(OsString::new, OsString::from);
    name.push(".tmp");
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

impl Spec {
    /// The spec as the text of its file.
    ///
    /// # Errors
    ///
    /// When a path, an argument or a variable is not UTF-8.
    pub fn encode(&self) -> io::Result<String> {
        let mut out = format!("{SPEC_HEADER} {SPEC_VERSION}\n");
        let mut line = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&encode(value));
            out.push('\n');
        };
        line("program", utf8(self.program.as_os_str())?);
        for arg in &self.args {
            line("arg", utf8(arg)?);
        }
        for (key, value) in &self.env {
            // Both halves are encoded, so the only bare '=' is the one between them.
            let pair = format!("{}={}", encode(utf8(key)?), encode(utf8(value)?));
            out.push_str("env=");
            out.push_str(&pair);
            out.push('\n');
        }
        let mut line = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&encode(value));
            out.push('\n');
        };
        line("cwd", utf8(self.cwd.as_os_str())?);
        line("output", utf8(self.output_dir.as_os_str())?);
        line("wall-clock-ms", &self.wall_clock.as_millis().to_string());
        Ok(out)
    }

    /// A spec from the text of its file.
    ///
    /// # Errors
    ///
    /// When the text is not a spec of [`SPEC_VERSION`], or a field is missing or malformed.
    pub fn decode(text: &str) -> io::Result<Self> {
        let (mut program, mut cwd, mut output_dir, mut wall_clock) = (None, None, None, None);
        let (mut args, mut env) = (Vec::new(), Vec::new());
        for (key, value) in lines(text, SPEC_HEADER, SPEC_VERSION)? {
            match key {
                "program" => program = Some(PathBuf::from(decode(value)?)),
                "arg" => args.push(decode(value)?.into()),
                "env" => {
                    // Both halves are encoded, so the only bare '=' is the one between them.
                    let (name, value) = value
                        .split_once('=')
                        .ok_or_else(|| invalid("an env line without '='"))?;
                    env.push((decode(name)?.into(), decode(value)?.into()));
                }
                "cwd" => cwd = Some(PathBuf::from(decode(value)?)),
                "output" => output_dir = Some(PathBuf::from(decode(value)?)),
                "wall-clock-ms" => {
                    wall_clock = Some(Duration::from_millis(number(key, &decode(value)?)?));
                }
                other => return Err(invalid(format!("an unknown spec key: {other}"))),
            }
        }
        let missing = |key: &str| invalid(format!("the spec has no {key}"));
        Ok(Self {
            program: program.ok_or_else(|| missing("program"))?,
            args,
            env,
            cwd: cwd.ok_or_else(|| missing("cwd"))?,
            output_dir: output_dir.ok_or_else(|| missing("output"))?,
            wall_clock: wall_clock.ok_or_else(|| missing("wall-clock-ms"))?,
        })
    }

    /// Writes the spec to `path`.
    ///
    /// # Errors
    ///
    /// As [`Spec::encode`], or when the file cannot be written.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        std::fs::write(path, self.encode()?)
    }

    /// Reads the spec at `path`.
    ///
    /// # Errors
    ///
    /// As [`Spec::decode`], or when the file cannot be read.
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::decode(&std::fs::read_to_string(path)?)
    }
}

impl Record {
    /// The record as the text of its file.
    ///
    /// # Errors
    ///
    /// When an output path is not UTF-8.
    pub fn encode(&self) -> io::Result<String> {
        let mut out = format!("{RECORD_HEADER} {}\n", self.version);
        let mut line = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&encode(value));
            out.push('\n');
        };
        line("shim-pid", &self.shim.pid.to_string());
        line("shim-start", &self.shim.start.raw().to_string());
        line("child-pid", &self.child.pid.to_string());
        line("child-start", &self.child.start.raw().to_string());
        line("stdout", utf8(self.stdout.as_os_str())?);
        line("stderr", utf8(self.stderr.as_os_str())?);
        match self.exit {
            Some(Exit::Code(code)) => line("exit-code", &code.to_string()),
            Some(Exit::Signal(signal)) => line("exit-signal", &signal.to_string()),
            None => {}
        }
        line("capped", if self.capped { "true" } else { "false" });
        Ok(out)
    }

    /// A record from the text of its file.
    ///
    /// # Errors
    ///
    /// When the text is not a record of [`RECORD_VERSION`], or a field is missing or malformed.
    pub fn decode(text: &str) -> io::Result<Self> {
        let (mut shim_pid, mut shim_start, mut child_pid, mut child_start) =
            (None, None, None, None);
        let (mut stdout, mut stderr, mut exit, mut capped) = (None, None, None, false);
        for (key, value) in lines(text, RECORD_HEADER, RECORD_VERSION)? {
            let value = decode(value)?;
            match key {
                "shim-pid" => shim_pid = Some(number(key, &value)?),
                "shim-start" => shim_start = Some(number(key, &value)?),
                "child-pid" => child_pid = Some(number(key, &value)?),
                "child-start" => child_start = Some(number(key, &value)?),
                "stdout" => stdout = Some(PathBuf::from(value)),
                "stderr" => stderr = Some(PathBuf::from(value)),
                "exit-code" => exit = Some(Exit::Code(number(key, &value)?)),
                "exit-signal" => exit = Some(Exit::Signal(number(key, &value)?)),
                "capped" => capped = value == "true",
                // Keys a later version of the same format adds are skipped.
                _ => {}
            }
        }
        let missing = |key: &str| invalid(format!("the record has no {key}"));
        let key = |pid: Option<u32>, start: Option<u64>, what: &str| {
            Ok::<_, io::Error>(ProcessKey {
                pid: pid.ok_or_else(|| missing(what))?,
                start: StartTime::from_raw(start.ok_or_else(|| missing(what))?),
            })
        };
        Ok(Self {
            version: RECORD_VERSION,
            shim: key(shim_pid, shim_start, "shim")?,
            child: key(child_pid, child_start, "child")?,
            stdout: stdout.ok_or_else(|| missing("stdout"))?,
            stderr: stderr.ok_or_else(|| missing("stderr"))?,
            exit,
            capped,
        })
    }

    /// Replaces the record at `path` atomically.
    ///
    /// # Errors
    ///
    /// As [`Record::encode`], or when the file cannot be written.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        replace(path, &self.encode()?)
    }

    /// Reads the record at `path`.
    ///
    /// # Errors
    ///
    /// As [`Record::decode`], or when the file cannot be read.
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::decode(&std::fs::read_to_string(path)?)
    }
}

fn exit_of(status: ExitStatus) -> io::Result<Exit> {
    if let Some(code) = status.code() {
        return Ok(Exit::Code(code));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Ok(Exit::Signal(signal));
        }
    }
    Err(io::Error::other(format!(
        "an exit without a code: {status}"
    )))
}

/// Runs the shim on the spec at `spec_path` (see the module documentation) and returns the code
/// the shim exits with, the child's.
///
/// # Errors
///
/// When the spec cannot be read, the child cannot be started, or a record cannot be written.
/// A child that started is killed with its container before the error returns.
pub fn run(spec_path: &Path) -> io::Result<i32> {
    let spec = Spec::read(spec_path)?;
    let record_path = record_path(spec_path);
    std::fs::create_dir_all(&spec.output_dir)?;
    let stdout = spec.output_dir.join(STDOUT_FILE);
    let stderr = spec.output_dir.join(STDERR_FILE);
    let own_pid = std::process::id();
    let own_start =
        sys::start_time(own_pid)?.ok_or_else(|| io::Error::other("the shim has no start time"))?;

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?);

    let mut record = None;
    let (mut child, container) = sys::spawn_gated(&mut command, |pid, start| {
        let first = Record {
            version: RECORD_VERSION,
            shim: ProcessKey {
                pid: own_pid,
                start: StartTime::from_raw(own_start),
            },
            child: ProcessKey {
                pid,
                start: StartTime::from_raw(start),
            },
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            exit: None,
            capped: false,
        };
        // The record is on disk before the child runs its first instruction.
        first.write(&record_path)?;
        record = Some(first);
        Ok(())
    })?;
    // The command holds the shim's handles of the output files; the child has its own.
    drop(command);
    let mut record = record.ok_or_else(|| io::Error::other("the child started unrecorded"))?;

    // The child is waited on by a thread, so this one can enforce the cap meanwhile.
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        sender.send(child.wait()).ok();
    });
    let status = match receiver.recv_timeout(spec.wall_clock) {
        Ok(status) => status,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            record.capped = true;
            container.kill_all()?;
            receiver
                .recv()
                .map_err(|_| io::Error::other("the waiter thread ended without a status"))?
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("the waiter thread ended without a status"))
        }
    };
    waiter.join().ok();
    let exit = match status.and_then(exit_of) {
        Ok(exit) => exit,
        Err(e) => {
            container.kill();
            return Err(e);
        }
    };
    record.exit = Some(exit);
    if cfg!(mutant_shim_no_exit_record) {
        // Mutant (`mutant_shim_no_exit_record`): the exit is never written; the record keeps
        // saying the child runs.
    } else {
        record.write(&record_path)?;
    }
    // What the child left behind dies now (on Windows also when the job's handle closes).
    container.kill();
    Ok(exit.shim_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            program: PathBuf::from("/bin/agent"),
            args: vec![
                "--prompt".into(),
                "a=b%c\r\nnext".into(),
                String::new().into(),
            ],
            env: vec![
                ("=C:".into(), "C:\\work".into()),
                ("KEY".into(), "x=y\n".into()),
            ],
            cwd: PathBuf::from("/work dir"),
            output_dir: PathBuf::from("/out"),
            wall_clock: Duration::from_secs(90),
        }
    }

    #[test]
    fn a_spec_survives_its_file_whatever_its_values_hold() {
        let spec = spec();
        let text = spec.encode().expect("encode");
        assert_eq!(text.lines().count(), 1 + 1 + 3 + 2 + 3, "{text}");
        assert_eq!(Spec::decode(&text).expect("decode"), spec);
    }

    #[test]
    fn a_spec_of_another_version_or_without_a_field_is_refused() {
        let text = spec().encode().expect("encode");
        let other = text.replacen("bosun-shim-spec 1", "bosun-shim-spec 2", 1);
        assert!(Spec::decode(&other).is_err());
        let partial = text
            .lines()
            .filter(|l| !l.starts_with("cwd="))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(Spec::decode(&partial).is_err());
    }

    #[test]
    fn a_record_survives_its_file_with_and_without_an_exit() {
        let mut record = Record {
            version: RECORD_VERSION,
            shim: ProcessKey {
                pid: 10,
                start: StartTime::from_raw(11),
            },
            child: ProcessKey {
                pid: 20,
                start: StartTime::from_raw(u64::MAX),
            },
            stdout: PathBuf::from("/out/stdout"),
            stderr: PathBuf::from("/out/stderr"),
            exit: None,
            capped: false,
        };
        let decoded = Record::decode(&record.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, record);
        record.exit = Some(Exit::Code(-1));
        record.capped = true;
        let decoded = Record::decode(&record.encode().expect("encode")).expect("decode");
        assert_eq!(decoded, record);
        record.exit = Some(Exit::Signal(9));
        assert_eq!(record.exit.map(Exit::shim_code), Some(137));
    }

    #[test]
    fn the_record_lies_next_to_the_spec() {
        assert_eq!(
            record_path(Path::new("dir/session.spec")),
            Path::new("dir/session.spec.record")
        );
    }
}
