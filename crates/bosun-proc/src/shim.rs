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

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, ready};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::Sleep;

use crate::key::{ProcessKey, StartTime, is_running, reap};
use crate::spawn::{ChildSpec, Spawned, Spawner, Tree, WaitFuture};
use crate::{start_time, sys};

/// The format version of the spec and the record. A reader refuses any other.
pub const FORMAT_VERSION: u32 = 1;

/// How often a reader of a session polls its record and its output file.
const POLL: Duration = Duration::from_millis(20);

/// How long the shim may take to record its child before the spawn is given up.
const STARTUP: Duration = Duration::from_secs(10);

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

/// How many bytes of each output stream a reader has consumed, so that [`attach`] resumes after
/// them. The daemon keeps these counts; [`Offsets::default`] reads from the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offsets {
    /// Bytes of the child's stdout already consumed.
    pub stdout: u64,
    /// Bytes of the child's stderr already consumed.
    pub stderr: u64,
}

/// Starts children through the session shim (#8): [`Spawner::spawn`] writes a [`Spec`] into a
/// new session directory and starts the `bosun-shim` binary on it, then reads the child's output
/// and exit back from the shim's files and record through [`attach`].
///
/// The shim binary's path is given, not found: the daemon copies it to a versioned path under
/// its state directory before it launches. Each session gets its own directory under `sessions`,
/// which is created when missing.
#[derive(Debug, Clone)]
pub struct ShimSpawner {
    shim: PathBuf,
    sessions: PathBuf,
    cap: Duration,
}

impl ShimSpawner {
    /// A spawner that runs `shim` for each child, keeps sessions under `sessions`, and kills a
    /// child that runs longer than `cap`.
    #[must_use]
    pub fn new(shim: impl Into<PathBuf>, sessions: impl Into<PathBuf>, cap: Duration) -> Self {
        Self {
            shim: shim.into(),
            sessions: sessions.into(),
            cap,
        }
    }
}

impl Spawner for ShimSpawner {
    /// # Errors
    ///
    /// When the spec cannot be written, the shim cannot be started or does not record its child
    /// within [`STARTUP`], or `spec.stdin` asks for a pipe, which a session does not carry yet.
    /// A child the shim had started is ended then.
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned> {
        if spec.stdin {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a session has no standard input pipe",
            ));
        }
        fs::create_dir_all(&self.sessions)?;
        let dir = self.sessions.join(session_name());
        fs::create_dir(&dir)?;
        let spec_path = dir.join("spec.json");
        let session = Spec {
            program: utf8(spec.program.as_os_str())?,
            args: spec
                .args
                .iter()
                .map(|a| utf8(a))
                .collect::<io::Result<_>>()?,
            env: spec
                .env
                .iter()
                .map(|(k, v)| Ok((utf8(k)?, utf8(v)?)))
                .collect::<io::Result<_>>()?,
            cwd: spec.cwd.clone(),
            output: dir.join("out"),
            cap: self.cap,
        };
        session.write_to(&spec_path)?;

        let mut command = Command::new(&self.shim);
        command
            .arg(&spec_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group, so that killing the shim by its pid cannot reach the daemon.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut shim = command.spawn()?;
        let record_path = spec_path.with_extension("record");
        let record = match await_record(&record_path, &mut shim) {
            Ok(record) => record,
            Err(e) => {
                shim.kill().ok();
                if let Ok(record) = Record::read_from(&record_path) {
                    reap(record.child).ok();
                }
                return Err(e);
            }
        };
        debug_assert_eq!(record.shim.pid, shim.id());
        // Dropping the std child neither kills the shim nor waits for it: the session outlives
        // this spawn and its holder.
        drop(shim);
        attach(&record_path, Offsets::default())
    }
}

/// Waits until the shim has written the record that names the child, and returns it. A shim
/// that exits first failed to start the child.
fn await_record(record_path: &Path, shim: &mut Child) -> io::Result<Record> {
    let deadline = Instant::now() + STARTUP;
    loop {
        if let Ok(record) = Record::read_from(record_path) {
            return Ok(record);
        }
        if let Some(status) = shim.try_wait()? {
            return Err(io::Error::other(format!(
                "the shim exited ({status}) before it recorded the child"
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the shim did not record its child in time",
            ));
        }
        thread::sleep(POLL);
    }
}

/// Re-attaches to the session of the record at `record_path`, running or finished, from the
/// record alone: the stdout and stderr from `from` on, the exit once it is written, and kill
/// through the shim. A session whose shim is gone and whose exit is recorded is still attached,
/// so that its output and exit can be read.
///
/// # Errors
///
/// When the record cannot be read or is not of this version, or its shim's pid now belongs to a
/// process that started at another time: that is another process, and nothing of the session is
/// attached. Also when an output file cannot be opened.
pub fn attach(record_path: &Path, from: Offsets) -> io::Result<Spawned> {
    let record = Record::read_from(record_path)?;
    if let Some(now) = start_time(record.shim.pid)? {
        // Mutant (`mutant_shim_attach_by_pid`): the pid alone names the shim, so a pid that has
        // changed hands is attached, and its output and kill go to another process.
        if now != record.shim.start && !cfg!(mutant_shim_attach_by_pid) {
            return Err(io::Error::other(
                "the shim's pid belongs to another process; the record is refused",
            ));
        }
    }
    let stdout = Tail::open(record_path, record.shim, &record.stdout, from.stdout)?;
    let stderr = Tail::open(record_path, record.shim, &record.stderr, from.stderr)?;
    Ok(Spawned {
        tree: Box::new(ShimTree {
            record: record_path.to_owned(),
            shim: record.shim,
            child: record.child,
        }),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        stdin: None,
    })
}

/// The container of a session, as the record names it: the child, and the shim that holds the
/// container. Holds no handle, so dropping it kills nothing.
#[derive(Debug)]
struct ShimTree {
    record: PathBuf,
    shim: ProcessKey,
    child: ProcessKey,
}

impl Tree for ShimTree {
    fn key(&self) -> ProcessKey {
        self.child
    }

    /// Kills the child, and leaves the rest to the shim: it sees its child end, records the exit
    /// and ends, and on Windows the job goes with it, which kills the descendants. The shim is not
    /// killed here, so that its exit is recorded rather than lost. On Unix the child's group is
    /// the container, and the child's pid names it, so the group goes with the child.
    fn kill_tree(&mut self) -> io::Result<()> {
        reap(self.child).map(|_| ())
    }

    fn wait(&mut self) -> WaitFuture<'_> {
        let (record, shim) = (self.record.clone(), self.shim);
        Box::pin(async move {
            loop {
                // Read the shim's liveness before the record: a shim that is gone has written
                // everything it will, so the record read after that is the final one.
                let alive = is_running(shim);
                if let Some(code) = Record::read_from(&record)?.exit {
                    return Ok(status_of(code));
                }
                if !alive {
                    return Err(io::Error::other("the session ended without an exit record"));
                }
                tokio::time::sleep(POLL).await;
            }
        })
    }

    /// Not available for a session: the container's members are read through its holder, and an
    /// attached session has none.
    fn members(&self) -> io::Result<Vec<ProcessKey>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a session's members are held by its shim",
        ))
    }
}

/// One output stream of a session, read from the shim's file as it grows. It ends when the
/// session has ended and the file holds no more bytes: the exit is recorded, or the shim is gone.
struct Tail {
    record: PathBuf,
    shim: ProcessKey,
    file: File,
    pause: Option<Pin<Box<Sleep>>>,
}

impl Tail {
    fn open(record: &Path, shim: ProcessKey, file: &Path, offset: u64) -> io::Result<Self> {
        let mut file = File::open(file)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            record: record.to_owned(),
            shim,
            file,
            pause: None,
        })
    }

    /// Whether the session has ended: its exit is in the record, or its shim is no longer
    /// running. Read before the file, so that the file read after a true answer is the last.
    fn ended(&self) -> io::Result<bool> {
        if Record::read_from(&self.record)?.exit.is_some() {
            return Ok(true);
        }
        Ok(!is_running(self.shim))
    }
}

impl AsyncRead for Tail {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some(pause) = self.pause.as_mut() {
                ready!(pause.as_mut().poll(cx));
                self.pause = None;
            }
            let ended = match self.ended() {
                Ok(ended) => ended,
                Err(e) => return Poll::Ready(Err(e)),
            };
            let this = &mut *self;
            match this.file.read(buf.initialize_unfilled()) {
                Ok(n) if n > 0 => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(_) if ended => return Poll::Ready(Ok(())),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
            self.pause = Some(Box::pin(tokio::time::sleep(POLL)));
        }
    }
}

/// The status a shim-recorded exit code stands for. On Unix the code sits in the wait status'
/// exit byte, as a child that called `exit` would have it.
fn status_of(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw((code & 0xff) << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code.cast_unsigned())
    }
}

/// A session directory name no other session of this daemon, or an earlier one, has.
fn session_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn utf8(text: &OsStr) -> io::Result<String> {
    text.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("a spec value that is not UTF-8"))
}
