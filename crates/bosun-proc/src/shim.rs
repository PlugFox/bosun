//! The session shim (#8): a process that runs one child in a container, records it, drains its
//! output to files and enforces the child's wall-clock cap, so that the child is ended and its
//! exit recorded even when the daemon that asked for it is dead.
//!
//! The shim is the `bosun-shim` binary, run as `bosun-shim <spec>`. For one spec it:
//!
//! 1. starts the child held back in a new container (a kill-on-close Job Object on Windows, a
//!    process group on Unix), with its whole environment from the spec and no breakaway;
//! 2. writes the record next to the spec: the shim's pid and start time, the child's, and the
//!    paths of its output files, all before the child runs;
//! 3. resumes the child and drains its stdout and stderr to `stdout.log` and `stderr.log` in the
//!    output directory as they arrive, so the daemon can tail them by offset;
//! 4. kills the container when the cap elapses, and when the child has exited writes its exit
//!    code into the record and exits with the same code.
//!
//! The shim holds the container's only handle, so when the shim dies the container dies with
//! it (Windows: the job's last handle closes). Nothing else enforces the cap.
//!
//! The spec and the record are text: a version line, then one field per line. Every string is
//! hex-encoded UTF-8, so no value needs escaping. The record is replaced by a rename, so a
//! reader sees the old record or the new one, never a part of either.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::key::{ProcessKey, StartTime};
use crate::{start_time, sys};

/// The format version of the spec and the record. A reader refuses any other.
pub const FORMAT_VERSION: u32 = 1;

/// How long the drains may outlive the child. A grandchild that keeps a pipe open must not
/// hold the record: after this, the shim records the exit and leaves the rest to the container.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// What the shim starts, as the daemon writes it to the spec file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The program, a path or a name looked up on the shim's `PATH`.
    pub program: String,
    /// The arguments after the program.
    pub args: Vec<String>,
    /// The child's entire environment.
    pub env: Vec<(String, String)>,
    /// The working directory of the child.
    pub cwd: PathBuf,
    /// The directory that receives `stdout.log` and `stderr.log`; created when missing.
    pub output: PathBuf,
    /// The wall-clock cap: the container is killed when the child has run this long.
    pub cap: Duration,
}

impl Spec {
    /// Writes the spec to `path`.
    ///
    /// # Errors
    ///
    /// When the file cannot be written, or a path is not valid UTF-8.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        let mut lines = vec![format!("bosun-shim-spec {FORMAT_VERSION}")];
        lines.push(format!("program {}", hex(&self.program)));
        lines.push(format!("cwd {}", hex(path_str(&self.cwd)?)));
        lines.push(format!("output {}", hex(path_str(&self.output)?)));
        lines.push(format!("cap-ms {}", self.cap.as_millis()));
        for arg in &self.args {
            lines.push(format!("arg {}", hex(arg)));
        }
        for (name, value) in &self.env {
            lines.push(format!("env {} {}", hex(name), hex(value)));
        }
        fs::write(path, lines.join("\n") + "\n")
    }

    /// Reads the spec at `path`.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a spec of this version, or lacks a field.
    pub fn read_from(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines();
        if lines.next() != Some(format!("bosun-shim-spec {FORMAT_VERSION}").as_str()) {
            return Err(invalid("not a spec of this version"));
        }
        let (mut program, mut cwd, mut output, mut cap) = (None, None, None, None);
        let (mut args, mut env) = (Vec::new(), Vec::new());
        for line in lines {
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "program" => program = Some(unhex(rest)?),
                "cwd" => cwd = Some(PathBuf::from(unhex(rest)?)),
                "output" => output = Some(PathBuf::from(unhex(rest)?)),
                "cap-ms" => {
                    let ms = rest.parse().map_err(|_| invalid("a cap in milliseconds"))?;
                    cap = Some(Duration::from_millis(ms));
                }
                "arg" => args.push(unhex(rest)?),
                "env" => {
                    let (name, value) = rest.split_once(' ').ok_or_else(|| invalid("an env"))?;
                    env.push((unhex(name)?, unhex(value)?));
                }
                _ => return Err(invalid("an unknown field of the spec")),
            }
        }
        Ok(Self {
            program: program.ok_or_else(|| invalid("a spec without a program"))?,
            args,
            env,
            cwd: cwd.ok_or_else(|| invalid("a spec without a working directory"))?,
            output: output.ok_or_else(|| invalid("a spec without an output directory"))?,
            cap: cap.ok_or_else(|| invalid("a spec without a cap"))?,
        })
    }
}

/// What the shim writes once the child is started: who runs, the output files, and, once the
/// child has ended, its exit code and whether the cap killed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The shim's pid and start time.
    pub shim: ProcessKey,
    /// The child's pid and start time.
    pub child: ProcessKey,
    /// The child's stdout file.
    pub stdout: PathBuf,
    /// The child's stderr file.
    pub stderr: PathBuf,
    /// The child's exit code, once it has ended; `None` while it runs.
    pub exit: Option<i32>,
    /// Whether the wall-clock cap killed the child.
    pub capped: bool,
}

impl Record {
    /// Replaces the record at `path` with this one, atomically.
    ///
    /// # Errors
    ///
    /// When the file cannot be written or renamed, or a path is not valid UTF-8.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        let mut lines = vec![
            format!("bosun-shim-record {FORMAT_VERSION}"),
            format!("shim {} {}", self.shim.pid, self.shim.start.raw()),
            format!("child {} {}", self.child.pid, self.child.start.raw()),
            format!("stdout {}", hex(path_str(&self.stdout)?)),
            format!("stderr {}", hex(path_str(&self.stderr)?)),
        ];
        if let Some(code) = self.exit {
            // Mutant (`mutant_shim_no_exit_record`): the exit code never reaches the record, so a
            // child that ended is indistinguishable from one still running.
            if !cfg!(mutant_shim_no_exit_record) {
                lines.push(format!("exit {code}"));
            }
        }
        lines.push(format!("capped {}", if self.capped { "yes" } else { "no" }));
        let tmp = path.with_extension("record.tmp");
        fs::write(&tmp, lines.join("\n") + "\n")?;
        fs::rename(&tmp, path)
    }

    /// Reads the record at `path`.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a record of this version, or is malformed.
    pub fn read_from(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines();
        if lines.next() != Some(format!("bosun-shim-record {FORMAT_VERSION}").as_str()) {
            return Err(invalid("not a record of this version"));
        }
        let (mut shim, mut child, mut stdout, mut stderr) = (None, None, None, None);
        let (mut exit, mut capped) = (None, None);
        for line in lines {
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "shim" => shim = Some(process_key(rest)?),
                "child" => child = Some(process_key(rest)?),
                "stdout" => stdout = Some(PathBuf::from(unhex(rest)?)),
                "stderr" => stderr = Some(PathBuf::from(unhex(rest)?)),
                "exit" => exit = Some(rest.parse().map_err(|_| invalid("an exit code"))?),
                "capped" => capped = Some(rest == "yes"),
                _ => return Err(invalid("an unknown field of the record")),
            }
        }
        Ok(Self {
            shim: shim.ok_or_else(|| invalid("a record without a shim"))?,
            child: child.ok_or_else(|| invalid("a record without a child"))?,
            stdout: stdout.ok_or_else(|| invalid("a record without stdout"))?,
            stderr: stderr.ok_or_else(|| invalid("a record without stderr"))?,
            exit,
            capped: capped.unwrap_or(false),
        })
    }
}

/// Runs the shim for the spec at `spec_path`, and returns the exit code of the child, which the
/// shim exits with. The record goes next to the spec, with the extension `record`.
///
/// # Errors
///
/// When the spec cannot be read, the output files cannot be created, the child cannot be
/// started, or the record cannot be written. The child is ended when it had been started.
pub fn run(spec_path: &Path) -> io::Result<i32> {
    let spec = Spec::read_from(spec_path)?;
    let record_path = spec_path.with_extension("record");
    fs::create_dir_all(&spec.output)?;
    let stdout_path = spec.output.join("stdout.log");
    let stderr_path = spec.output.join("stderr.log");
    let stdout_file = File::create(&stdout_path)?;
    let stderr_file = File::create(&stderr_path)?;

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let suspended = sys::Suspended::spawn(&mut command)?;
    let pid = suspended.pid();
    let child_start =
        start_time(pid)?.ok_or_else(|| io::Error::other("the held child is not running"))?;
    let mut record = Record {
        shim: own_key()?,
        child: ProcessKey {
            pid,
            start: child_start,
        },
        stdout: stdout_path,
        stderr: stderr_path,
        exit: None,
        capped: false,
    };
    // The record is written while the child is still held: a shim that dies from here on
    // leaves a record that names the child, and the container dies with the shim.
    record.write_to(&record_path)?;
    let (mut child, container) = suspended.resume()?;
    let container = Arc::new(container);

    let (drained_tx, drained_rx) = mpsc::channel();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("no stderr"))?;
    drain(stdout, stdout_file, drained_tx.clone());
    drain(stderr, stderr_file, drained_tx);

    // The cap: the watchdog kills the container when the cap elapses before the child is done.
    // Dropping its sender after the wait tells it the child ended first.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let killer = Arc::clone(&container);
    let cap = spec.cap;
    let watchdog = thread::spawn(move || match done_rx.recv_timeout(cap) {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            killer.kill();
            true
        }
        _ => false,
    });
    let status = child.wait()?;
    drop(done_tx);
    let capped = watchdog.join().unwrap_or(false);

    // The drains end at the end of the output. Wait for them, but not beyond the grace.
    let deadline = Instant::now() + DRAIN_GRACE;
    for _ in 0..2 {
        if drained_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_err()
        {
            break;
        }
    }

    let code = exit_code(status);
    record.exit = Some(code);
    record.capped = capped;
    record.write_to(&record_path)?;
    Ok(code)
}

/// The shim's own pid and start time.
fn own_key() -> io::Result<ProcessKey> {
    let pid = std::process::id();
    let start = start_time(pid)?.ok_or_else(|| io::Error::other("the shim is not running"))?;
    Ok(ProcessKey { pid, start })
}

/// Copies `from` into `to` until the end of the stream, then reports on `done`. A failed write
/// stops the copy to the file but not the reading: a child that blocks on a full pipe would
/// never end.
fn drain(mut from: impl Read + Send + 'static, to: File, done: mpsc::Sender<()>) {
    thread::spawn(move || {
        let mut sink = Some(to);
        let mut buf = [0_u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(file) = sink.as_mut()
                        && file.write_all(&buf[..n]).is_err()
                    {
                        sink = None;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        done.send(()).ok();
    });
}

/// The exit code the record holds: the child's own, or `128 + signal` when a signal ended it.
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn path_str(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("a path that is not UTF-8"))
}

fn process_key(text: &str) -> io::Result<ProcessKey> {
    let (pid, start) = text
        .split_once(' ')
        .ok_or_else(|| invalid("a process as pid and start time"))?;
    Ok(ProcessKey {
        pid: pid.parse().map_err(|_| invalid("a pid"))?,
        start: StartTime::from_raw(start.parse().map_err(|_| invalid("a start time"))?),
    })
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex(text: &str) -> String {
    text.bytes()
        .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xf)]])
        .map(char::from)
        .collect()
}

fn unhex(text: &str) -> io::Result<String> {
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for i in (0..text.len()).step_by(2) {
        let pair = text
            .get(i..i + 2)
            .ok_or_else(|| invalid("a hex string of even length"))?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|_| invalid("a hex digit"))?);
    }
    String::from_utf8(bytes).map_err(|_| invalid("hex text that is not UTF-8"))
}
