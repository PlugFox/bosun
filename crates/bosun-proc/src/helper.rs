//! Helper processes for the tests: the test executable re-invoked with one test selected,
//! [`run_role`], and the role in an environment variable. A helper reports pids and values on
//! stdout as `bosun-proc-helper <key> <value>` lines; libtest's own lines are ignored.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::spawn::{ChildSpec, NativeSpawner, Output, Spawner};

const ROLE: &str = "BOSUN_PROC_HELPER";
const PREFIX: &str = "bosun-proc-helper";
/// How long a sleeping helper lives: far beyond any test, short enough that a leak ends.
const SLEEP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Sleeps.
    Sleep,
    /// Reports its environment and directory on stdout, and a line on stderr; exits.
    Speak,
    /// Starts a sleeping grandchild, reports its pid, exits.
    Parent,
    /// As `Parent`, then sleeps.
    ParentStay,
    /// Spawns a `ParentStay` through `NativeSpawner`, reports the child's and the grandchild's
    /// pids, sleeps holding the job.
    Holder,
}

impl Role {
    const ALL: [Self; 5] = [
        Self::Sleep,
        Self::Speak,
        Self::Parent,
        Self::ParentStay,
        Self::Holder,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Speak => "speak",
            Self::Parent => "parent",
            Self::ParentStay => "parent-stay",
            Self::Holder => "holder",
        }
    }
}

fn args() -> Vec<OsString> {
    ["helper::run_role", "--exact", "--nocapture", "--quiet"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// The environment of a helper: its role, and on Windows `SystemRoot`, which parts of the
/// system expect. Nothing else of this process.
fn env(role: Role) -> Vec<(OsString, OsString)> {
    let mut env = vec![(ROLE.into(), role.name().into())];
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        env.push(("SystemRoot".into(), root));
    }
    env
}

/// The spec of a helper for [`NativeSpawner`].
pub(crate) fn spec(role: Role) -> ChildSpec {
    ChildSpec {
        program: std::env::current_exe().expect("current exe"),
        args: args(),
        env: env(role),
        cwd: std::env::temp_dir(),
    }
}

/// A helper as a plain `std` child, outside any container of this crate.
pub(crate) fn std_command(role: Role) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current exe"));
    command
        .args(args())
        .env_clear()
        .envs(env(role))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The `bosun-proc-helper` lines of `text`, by key.
pub(crate) fn lines(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(PREFIX)?.strip_prefix(' ')?;
            let (key, value) = rest.split_once(' ')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Reads `stdout` up to the report of `key` and returns its value.
pub(crate) async fn read_key(stdout: &mut Output, key: &str) -> String {
    use tokio::io::AsyncBufReadExt;
    let mut stdout = tokio::io::BufReader::new(stdout);
    loop {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).await.expect("read");
        assert!(read > 0, "the helper exited before it reported {key}");
        if let Some(value) = lines(&line).remove(key) {
            return value;
        }
    }
}

fn report(out: &mut impl Write, key: &str, value: impl std::fmt::Display) {
    writeln!(out, "{PREFIX} {key} {value}").expect("report");
    out.flush().expect("flush");
}

/// Starts a [`Role::Holder`] and reads the pids it reports: (holder, child, grandchild).
#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn start_holder() -> (std::process::Child, u32, u32) {
    use std::io::{BufRead, BufReader};

    let mut holder = std_command(Role::Holder)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the holder");
    let mut stdout = BufReader::new(holder.stdout.take().expect("piped"));
    let mut seen = HashMap::new();
    while !(seen.contains_key("child") && seen.contains_key("grandchild")) {
        let mut line = String::new();
        if stdout.read_line(&mut line).expect("read") == 0 {
            holder.kill().ok();
            panic!("the holder exited before it reported: {seen:?}");
        }
        seen.extend(lines(&line));
    }
    let pid = |key: &str| seen[key].parse().expect("pid");
    let (child, grandchild) = (pid("child"), pid("grandchild"));
    (holder, child, grandchild)
}

/// The helper's entry point: a no-op unless the role variable selects a role.
#[test]
fn run_role() {
    let Some(name) = std::env::var_os(ROLE) else {
        return;
    };
    let role = Role::ALL
        .into_iter()
        .find(|r| name == r.name())
        .expect("a known role");
    let mut out = std::io::stdout();
    match role {
        Role::Sleep => std::thread::sleep(SLEEP),
        Role::Speak => {
            let value = std::env::var("BOSUN_PROC_TEST_VALUE").unwrap_or_else(|_| "none".into());
            report(&mut out, "value", value);
            let inherited = if std::env::var_os("PATH").is_some() {
                "present"
            } else {
                "absent"
            };
            report(&mut out, "inherited", inherited);
            let cwd = std::env::current_dir().expect("cwd");
            report(&mut out, "cwd", cwd.display());
            report(&mut std::io::stderr(), "stderr", "line");
        }
        Role::Parent | Role::ParentStay => {
            #[expect(
                clippy::zombie_processes,
                reason = "the grandchild must outlive its parent; its new parent reaps it"
            )]
            let grandchild = std_command(Role::Sleep).spawn().expect("spawn");
            report(&mut out, "grandchild", grandchild.id());
            if role == Role::ParentStay {
                std::thread::sleep(SLEEP);
            }
        }
        Role::Holder => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let mut spawned = NativeSpawner.spawn(&spec(Role::ParentStay)).expect("spawn");
                let grandchild = read_key(&mut spawned.stdout, "grandchild").await;
                report(&mut out, "child", spawned.tree.key().pid);
                report(&mut out, "grandchild", grandchild);
                std::thread::sleep(SLEEP);
                drop(spawned);
            });
        }
    }
}
