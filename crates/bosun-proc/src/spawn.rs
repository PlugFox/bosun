//! Spawning a child into a container: the [`Spawner`] seam and its in-process implementation.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};

use tokio::io::{AsyncRead, AsyncWriteExt as _};

use crate::key::{ProcessKey, StartTime};
use crate::sys;

/// What to start: the program, its arguments, its whole environment and its working
/// directory. Nothing is inherited from the daemon: the environment is exactly `env` (#2), and
/// stdin holds exactly the bytes of `stdin`, or nothing.
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
    /// The bytes the child reads on standard input, then end of file: an agent CLI reads its
    /// prompt there. `None` is a closed stdin.
    pub stdin: Option<Vec<u8>>,
}

/// One output stream of a child, read by the caller.
///
/// A boxed reader rather than tokio's `ChildStdout`, so that an implementation whose child is
/// not the daemon's own (the session shim of #8 relays the streams over its own channel) fits
/// the same seam. The native one boxes tokio's pipe; reading it costs one virtual call per read.
pub type Output = Box<dyn AsyncRead + Send + Unpin>;

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

    /// The pids of the processes left in the container: the child while it runs and every
    /// descendant still inside, whether or not the child has exited. Windows lists the Job
    /// Object exactly; Unix lists the process group, best effort (see the crate documentation),
    /// and on macOS more weakly still, from a snapshot of the process table.
    ///
    /// # Errors
    ///
    /// When the operating system refuses the query, or (Windows) when the job keeps gaining
    /// processes faster than a bounded number of reads can list them.
    fn members(&self) -> io::Result<Vec<u32>>;

    /// Waits for the child itself to exit. Its descendants may outlive it, still inside the
    /// container until [`Tree::kill_tree`] or the drop.
    fn wait(&mut self) -> WaitFuture<'_>;

    /// The container, as another thread lists it while this tree lives
    /// ([`ContainerId::members`]); `None` when it cannot be named (a fake tree).
    fn container(&self) -> Option<ContainerId> {
        None
    }
}

/// A tree's container by name, listed from any thread: what [`Tree::members`] reads, without
/// the tree. Windows: the job, through the process that holds it and the handle's value there.
/// Unix: the process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContainerId(pub(crate) sys::ContainerId);

impl ContainerId {
    /// The pids of the processes in the container, as [`Tree::members`] lists them; none on
    /// Windows once the job's holder is gone.
    ///
    /// # Errors
    ///
    /// When the operating system refuses the query.
    pub fn members(self) -> io::Result<Vec<u32>> {
        sys::container_members(self.0)
    }
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
/// The bytes of [`ChildSpec::stdin`] go through a pipe, written by a task of its own that shuts
/// the pipe down after the last byte (or when the child stops reading).
///
/// Dropping its [`Tree`] kills whatever is left of the tree: on Windows the job's last handle
/// closes, on Unix the group is killed. Must be called inside a tokio runtime with the I/O
/// driver enabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeSpawner;

impl Spawner for NativeSpawner {
    fn spawn(&self, spec: &ChildSpec) -> io::Result<Spawned> {
        #[allow(
            clippy::disallowed_methods,
            reason = "a contained spawn: the platform layer sets CREATE_NO_WINDOW"
        )]
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spec.cwd)
            .stdin(if spec.stdin.is_some() {
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
        let pipe = child.stdin.take();
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
        if let (Some(mut pipe), Some(bytes)) = (pipe, spec.stdin.clone()) {
            // A child that exits without reading breaks the pipe: nothing to report then.
            tokio::spawn(async move {
                let _ = pipe.write_all(&bytes).await;
                let _ = pipe.shutdown().await;
            });
        }
        Ok(Spawned {
            tree: Box::new(tree),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
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

    fn members(&self) -> io::Result<Vec<u32>> {
        if cfg!(mutant_proc_members_child_only) {
            // Mutant (`mutant_proc_members_child_only`): the direct child alone, and nothing
            // once it has exited; its descendants go unlisted.
            return Ok(self.child.id().into_iter().collect());
        }
        self.container.members()
    }

    fn wait(&mut self) -> WaitFuture<'_> {
        Box::pin(self.child.wait())
    }

    fn container(&self) -> Option<ContainerId> {
        self.container.id().ok().map(ContainerId)
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
    use crate::key::{is_running, start_time};

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

    /// The bytes of `stdin` reach the child exactly, then end of file: it reads to the end and
    /// exits.
    #[test]
    fn the_child_reads_the_bytes_of_its_stdin_then_end_of_file() {
        let bytes = b"first line\nsecond line\r\n\x00\xff".to_vec();
        let mut spec = helper::spec(Role::Copy);
        spec.stdin = Some(bytes.clone());
        let (seen, status) = runtime().block_on(async {
            let mut spawned = NativeSpawner.spawn(&spec).expect("spawn");
            let seen = helper::read_key(&mut spawned.stdout, "stdin").await;
            let status = tokio::time::timeout(Duration::from_secs(30), spawned.tree.wait())
                .await
                .expect("the child saw end of file and exited")
                .expect("wait");
            (seen, status)
        });
        assert!(status.success(), "{status}");
        assert_eq!(seen, helper::hex(&bytes));
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

    /// Windows: a grandchild started with `CREATE_BREAKAWAY_FROM_JOB` is refused breakaway (or,
    /// when the job above refuses first, starts detached) and stays in the job: `kill_tree`
    /// leaves neither it nor the child within 5 s.
    #[cfg(windows)]
    #[test]
    fn a_grandchild_that_tries_to_escape_dies_with_the_tree() {
        let rt = runtime();
        let (left, how, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Escape))
                .expect("spawn");
            let child = spawned.tree.key();
            let grandchild = helper::read_key(&mut spawned.stdout, "grandchild").await;
            let grandchild = key_of(grandchild.parse().expect("pid"));
            let how = helper::read_key(&mut spawned.stdout, "escape").await;
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            spawned.tree.kill_tree().expect("kill tree");
            let left = survivors(&[child, grandchild], Duration::from_secs(5));
            (left, how, spawned)
        });
        drop(spawned);
        // A grandchild outside the job survives the drop too: reap it by pid and start time.
        for &key in &left {
            crate::reap(key).ok();
        }
        assert!(left.is_empty(), "still running ({how}): {left:?}");
    }

    /// Unix: a grandchild that calls `setsid` leaves the process group and survives
    /// `kill_tree`, the gap the crate documentation names and cgroups (#8) close. The test pins
    /// the gap so that closing it shows; the grandchild is reaped by pid and start time.
    #[cfg(unix)]
    #[test]
    fn a_grandchild_that_calls_setsid_leaves_the_process_group() {
        let rt = runtime();
        let (grandchild, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Escape))
                .expect("spawn");
            let grandchild = helper::read_key(&mut spawned.stdout, "grandchild").await;
            let grandchild = key_of(grandchild.parse().expect("pid"));
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            spawned.tree.kill_tree().expect("kill tree");
            (grandchild, spawned)
        });
        drop(spawned);
        let left = survivors(&[grandchild], Duration::from_millis(500));
        crate::reap(grandchild).ok();
        assert_eq!(left, [grandchild], "setsid no longer escapes the group");
    }

    /// Polls until `tree` lists no process, for at most `limit`. Returns the last listing.
    fn members_after(tree: &dyn Tree, limit: Duration) -> Vec<u32> {
        let deadline = Instant::now() + limit;
        loop {
            let members = tree.members().expect("members");
            if members.is_empty() || Instant::now() >= deadline {
                return members;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A child that starts a grandchild and exits: `members` still lists the grandchild, and
    /// not the child; after `kill_tree` it lists nothing.
    #[test]
    fn members_lists_the_processes_left_in_the_container() {
        let rt = runtime();
        let (child, grandchild, after_exit, after_kill, spawned) = rt.block_on(async {
            let mut spawned = NativeSpawner
                .spawn(&helper::spec(Role::Parent))
                .expect("spawn");
            let child = spawned.tree.key().pid;
            let grandchild: u32 = helper::read_key(&mut spawned.stdout, "grandchild")
                .await
                .parse()
                .expect("pid");
            let status = spawned.tree.wait().await.expect("wait");
            assert!(status.success(), "{status}");
            let after_exit = spawned.tree.members().expect("members");
            spawned.tree.kill_tree().expect("kill tree");
            let after_kill = members_after(spawned.tree.as_ref(), Duration::from_secs(5));
            (child, grandchild, after_exit, after_kill, spawned)
        });
        drop(spawned);
        assert!(
            after_exit.contains(&grandchild),
            "grandchild {grandchild} missing from {after_exit:?}"
        );
        assert!(!after_exit.contains(&child), "the exited child is listed");
        assert!(after_kill.is_empty(), "left after the kill: {after_kill:?}");
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
