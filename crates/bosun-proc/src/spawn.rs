//! Spawning a child into a container: the [`Spawner`] seam and its in-process implementation.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::key::{ProcessKey, StartTime, is_running, start_time};
use crate::sys;

/// What to start: the program, its arguments, its whole environment and its working
/// directory. Nothing is inherited from the daemon: the environment is exactly `env` (#2), and
/// stdin is closed unless `stdin` asks for a pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// The program, a path or a name looked up on the daemon's `PATH`.
    pub program: PathBuf,
    /// The arguments after the program.
    pub args: Vec<OsString>,
    /// The child's entire environment.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory.
    pub cwd: PathBuf,
    /// Give the child a standard input pipe ([`Spawned::stdin`]) instead of a closed one: an
    /// agent CLI reads its prompt there.
    pub stdin: bool,
}

/// One output stream of a child, read by the caller.
///
/// A boxed reader rather than tokio's `ChildStdout`, so that an implementation whose child is
/// not the daemon's own (the session shim of #8 relays the streams over its own channel) fits
/// the same seam. The native one boxes tokio's pipe; reading it costs one virtual call per read.
pub type Output = Box<dyn AsyncRead + Send + Unpin>;

/// The standard input of a child, written by the caller; dropping it closes the pipe.
pub type Input = Box<dyn AsyncWrite + Send + Unpin>;

/// The future of [`Tree::wait`].
pub type WaitFuture<'a> = Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + 'a>>;

/// A started child and its container.
pub trait Tree: Send + fmt::Debug {
    /// The child's pid and start time, as the daemon records it for [`crate::reap`].
    fn key(&self) -> ProcessKey;

    /// Kills the child and every process in its container. Returns once the kill is issued;
    /// the processes are gone shortly after.
    ///
    /// # Errors
    ///
    /// When the operating system refuses the kill.
    fn kill_tree(&mut self) -> io::Result<()>;

    /// Waits for the child itself to exit. Its descendants may outlive it, still inside the
    /// container until [`Tree::kill_tree`] or the drop.
    fn wait(&mut self) -> WaitFuture<'_>;

    /// The processes left in the container: the child while it runs, and its descendants,
    /// whether or not the child is alive. A process that has exited is never listed. On
    /// Windows this is the job's list; on Linux the group, read from `/proc`; on macOS the
    /// group through `libproc`, which is best effort and may miss members.
    ///
    /// # Errors
    ///
    /// When the operating system refuses the inspection.
    fn members(&self) -> io::Result<Vec<ProcessKey>>;
}

/// A child as [`Spawner::spawn`] returns it: the tree, and its stdout and stderr as separate
/// pipes. The caller reads both, concurrently, or the child blocks when a pipe fills.
pub struct Spawned {
    /// The child and its container.
    pub tree: Box<dyn Tree>,
    /// The child's stdout.
    pub stdout: Output,
    /// The child's stderr.
    pub stderr: Output,
    /// The child's stdin, when [`ChildSpec::stdin`] asked for it.
    pub stdin: Option<Input>,
}

impl fmt::Debug for Spawned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spawned")
            .field("tree", &self.tree)
            .finish_non_exhaustive()
    }
}

/// Starts children inside a container.
pub trait Spawner: Send + Sync + fmt::Debug {
    /// Starts `spec` in a new container.
    ///
    /// # Errors
    ///
    /// When the program cannot be started or the container cannot be made; nothing is left
    /// running then.
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned>;
}

/// The in-process [`Spawner`]: the child is the daemon's own, in a Job Object on Windows and a
/// process group on Unix (see the crate documentation).
///
/// Dropping its [`Tree`] kills whatever is left of the tree: on Windows the job's last handle
/// closes, on Unix the group is killed. Must be called inside a tokio runtime with the I/O
/// driver enabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeSpawner;

impl Spawner for NativeSpawner {
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spec.cwd)
            .stdin(if spec.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, container) = sys::spawn_contained(&mut command)?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the child has no pid"))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stdin = child.stdin.take().map(|s| Box::new(s) as Input);
        let start = match sys::child_start_time(&child) {
            Ok(start) => start,
            Err(e) => {
                container.kill();
                return Err(e);
            }
        };
        let tree = NativeTree {
            child,
            key: ProcessKey {
                pid,
                start: StartTime::from_raw(start),
            },
            container,
        };
        Ok(Spawned {
            tree: Box::new(tree),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            stdin,
        })
    }
}

#[derive(Debug)]
struct NativeTree {
    child: tokio::process::Child,
    key: ProcessKey,
    container: sys::Container,
}

impl Tree for NativeTree {
    fn key(&self) -> ProcessKey {
        self.key
    }

    fn kill_tree(&mut self) -> io::Result<()> {
        if cfg!(mutant_proc_kill_parent_only) {
            // The child alone, by its own handle; its descendants live on.
            self.child.start_kill().ok();
            return Ok(());
        }
        self.container.kill_all()
    }

    fn wait(&mut self) -> WaitFuture<'_> {
        Box::pin(self.child.wait())
    }

    fn members(&self) -> io::Result<Vec<ProcessKey>> {
        if cfg!(mutant_proc_members_child_only) {
            // Mutant (`mutant_proc_members_child_only`): the direct child alone is listed, so the
            // descendants left in the container are invisible.
            return Ok(is_running(self.key)
                .then_some(self.key)
                .into_iter()
                .collect());
        }
        let mut members = Vec::new();
        for pid in self.container.members()? {
            if let Some(start) = start_time(pid)? {
                members.push(ProcessKey { pid, start });
            }
        }
        Ok(members)
    }
}

impl Drop for NativeTree {
    fn drop(&mut self) {
        // Windows: dropping the job's handle kills the tree. Unix: kill the group.
        self.container.kill();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::helper::{self, Role};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// Polls until no process of `keys` runs, for at most `limit`. Returns those still running.
    fn survivors(keys: &[ProcessKey], limit: Duration) -> Vec<ProcessKey> {
        let deadline = Instant::now() + limit;
        loop {
            let left: Vec<_> = keys.iter().copied().filter(|&k| is_running(k)).collect();
            if left.is_empty() || Instant::now() >= deadline {
                return left;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn key_of(pid: u32) -> ProcessKey {
        let start = start_time(pid).expect("start time").expect("live");
        ProcessKey { pid, start }
    }

    #[test]
    fn the_child_gets_its_environment_and_directory_and_separate_pipes() {
        let dir = std::env::temp_dir();
        let mut spec = helper::spec(Role::Speak);
        spec.cwd.clone_from(&dir);
        spec.env
            .push(("BOSUN_PROC_TEST_VALUE".into(), "carried".into()));
        let (stdout, stderr, status, key) = runtime().block_on(async {
            let mut spawned = NativeSpawner.spawn(&spec).expect("spawn");
            let mut stdout = String::new();
            let mut stderr = String::new();
            let (out, err) = tokio::join!(
                spawned.stdout.read_to_string(&mut stdout),
                spawned.stderr.read_to_string(&mut stderr),
            );
            out.expect("stdout");
            err.expect("stderr");
            let status = spawned.tree.wait().await.expect("wait");
            (stdout, stderr, status, spawned.tree.key())
        });
        assert!(status.success(), "{status}");
        assert!(key.pid != 0);
        let lines = helper::lines(&stdout);
        assert_eq!(lines.get("value").map(String::as_str), Some("carried"));
        assert_eq!(
            lines.get("inherited").map(String::as_str),
            Some("absent"),
            "the daemon's environment leaked"
        );
        let cwd = PathBuf::from(&lines["cwd"]);
        assert_eq!(
            cwd.canonicalize().expect("cwd"),
            dir.canonicalize().expect("dir")
        );
        assert!(helper::lines(&stderr).contains_key("stderr"), "{stderr}");
        assert!(!lines.contains_key("stderr"), "stderr reached stdout");
    }

    #[test]
    fn spawn_reports_the_pid_and_start_time() {
        runtime().block_on(async {
            let spawned = NativeSpawner
                .spawn(&helper::spec(Role::Sleep))
                .expect("spawn");
            let key = spawned.tree.key();
            assert_eq!(start_time(key.pid).expect("start time"), Some(key.start));
        });
    }

    /// A child that spawns a grandchild and exits: `kill_tree` leaves neither within 5 s.
    #[test]
    fn kill_tree_kills_a_child_and_its_grandchild() {
        let rt = runtime();
        let (left, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Parent))
                .expect("spawn");
            let child = spawned.tree.key();
            let grandchild = helper::read_key(&mut spawned.stdout, "grandchild").await;
            let grandchild = key_of(grandchild.parse().expect("pid"));
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            assert!(is_running(grandchild), "the grandchild outlives its parent");
            spawned.tree.kill_tree().expect("kill tree");
            let left = survivors(&[child, grandchild], Duration::from_secs(5));
            (left, spawned)
        });
        // The drop cleans up whatever a broken kill left.
        drop(spawned);
        assert!(left.is_empty(), "still running: {left:?}");
    }

    /// Polls `tree` until it lists no process, for at most `limit`. Returns what it still lists.
    fn members_within(tree: &dyn Tree, limit: Duration) -> Vec<ProcessKey> {
        let deadline = Instant::now() + limit;
        loop {
            let members = tree.members().expect("members");
            if members.is_empty() || Instant::now() >= deadline {
                return members;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Windows: the child asks for a breakaway from its job. The test run's own job refuses it,
    /// and so does the job of the spawner when it allows none, so the grandchild stays in the
    /// container and `kill_tree` takes it. With the breakaway allowed it leaves, and lives on.
    #[cfg(windows)]
    #[test]
    fn a_grandchild_that_tries_to_escape_dies_with_the_tree() {
        let rt = runtime();
        let (left, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Escape))
                .expect("spawn");
            let pid: u32 = helper::read_key(&mut spawned.stdout, "grandchild")
                .await
                .parse()
                .expect("pid");
            let grandchild = key_of(pid);
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            assert!(is_running(grandchild), "the grandchild outlives its parent");
            spawned.tree.kill_tree().expect("kill tree");
            let left = survivors(&[grandchild], Duration::from_secs(5));
            // Reaps what a broken kill left, so that no helper outlives the test.
            for &key in &left {
                crate::reap(key).ok();
            }
            (left, spawned)
        });
        drop(spawned);
        assert!(left.is_empty(), "the grandchild escaped: {left:?}");
    }

    /// `members` lists every process left in the container: a grandchild that outlives the child
    /// which started it is listed after the child has exited, and nothing is listed after the
    /// kill.
    #[test]
    fn members_lists_the_processes_left_in_the_container() {
        let rt = runtime();
        let (grandchild, listed, after_kill, left, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Parent))
                .expect("spawn");
            let pid: u32 = helper::read_key(&mut spawned.stdout, "grandchild")
                .await
                .parse()
                .expect("pid");
            let grandchild = key_of(pid);
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            let listed = spawned.tree.members().expect("members");
            spawned.tree.kill_tree().expect("kill tree");
            let left = survivors(&[grandchild], Duration::from_secs(5));
            for &key in &left {
                crate::reap(key).ok();
            }
            let after_kill = members_within(&*spawned.tree, Duration::from_secs(5));
            (grandchild, listed, after_kill, left, spawned)
        });
        drop(spawned);
        assert!(left.is_empty(), "still running: {left:?}");
        assert!(
            listed.contains(&grandchild),
            "the grandchild is not listed: {listed:?}"
        );
        assert!(after_kill.is_empty(), "still listed: {after_kill:?}");
    }

    /// A holder process that owns the job and is killed without warning (`TerminateProcess`)
    /// leaves no descendant: the job closes with its last handle and kills them.
    #[cfg(windows)]
    #[test]
    fn a_dead_holder_leaves_no_descendant() {
        let (mut holder, child, grandchild) = helper::start_holder();
        let child = key_of(child);
        let grandchild = key_of(grandchild);
        holder.kill().expect("kill the holder");
        holder.wait().expect("wait");
        let left = survivors(&[child, grandchild], Duration::from_secs(5));
        for &key in &left {
            crate::reap(key).ok();
        }
        assert!(left.is_empty(), "still running: {left:?}");
    }

    /// Linux: the direct child dies with its holder through the parent-death signal. Its own
    /// child does not (the signal is not inherited); see the crate documentation.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_holder_takes_its_direct_child() {
        let (mut holder, child, grandchild) = helper::start_holder();
        let child = key_of(child);
        let grandchild = key_of(grandchild);
        holder.kill().expect("kill the holder");
        holder.wait().expect("wait");
        let left = survivors(&[child], Duration::from_secs(5));
        crate::reap(child).ok();
        crate::reap(grandchild).ok();
        assert!(left.is_empty(), "still running: {left:?}");
    }
}
