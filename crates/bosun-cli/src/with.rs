//! `bosun with [--repo <name>] <resource>... -- <command> [args...]` (#8, "Leases"; E16): runs a
//! command under the daemon's leases of the named resources, the leases items take, so
//! interactive work and scripts need no lock files of their own.
//!
//! - It connects, reads the operator token (as `pause` does, `control.rs`) and asks
//!   `lease.acquire` for its own pid; it polls every [`POLL`] until the whole set is granted,
//!   saying on stderr when it starts waiting and how long it waited.
//! - Then it runs the command with inherited standard input, output and error, waits for it,
//!   releases the lease (`lease.release`) and exits with the command's exit code; a command
//!   killed by a signal is 128 + the signal, as a shell reports it.
//! - A daemon that cannot be reached, or that refuses, is an error of its usual exit code, and
//!   the command is not run. Ctrl-C while waiting withdraws the request and exits 130; while
//!   the command runs it is the command's to handle (a terminal sends it to the command too).
//! - On Unix, while the command runs, a SIGTERM or SIGHUP to `bosun` (`timeout`, a
//!   supervisor's `kill`, a hang-up) is passed on to the command, by its pid only, through the
//!   POSIX `kill` utility (the client crates have no `unsafe`); `bosun` keeps waiting for it,
//!   releases the lease and exits with its code (128 + the signal if the signal killed it). So
//!   the command never runs on without the lease.
//! - On Windows a bare program name (no path separator, no extension) is looked up as the shell
//!   does: each `PATH` directory in order, with each extension of `PATHEXT` (default
//!   `.COM;.EXE;.BAT;.CMD`), so `npm`, `yarn` or `gradlew` find their `.cmd` or `.bat`. The
//!   standard library runs a `.bat`/`.cmd` through `cmd.exe` with its own argument escaping and
//!   refuses an argument it cannot escape safely; that refusal is "cannot run".
//! - Windows limitation: `taskkill /F` of `bosun.exe` alone leaves the command running without
//!   the lease. Ending it with `bosun` needs a Job object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
//!   that is `unsafe` FFI, which a client crate does not have. Closing the console ends both.
//! - If `bosun` itself is killed outright, the daemon releases the lease when it next looks:
//!   the holder is this process.
//! - The identity is `--identity`, else the owner of `--repo`, else the configuration's only
//!   identity; with several, `--identity` is required.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use bosun_api::{AcquireParams, ReleaseParams, method};
use bosun_client::Client;
use clap::Args;

use crate::{Failure, Global, exit, identities};

/// How often a waiting request is asked about.
const POLL: Duration = Duration::from_millis(500);

/// The exit code of an interrupt while waiting, as a shell reports SIGINT.
const INTERRUPTED: i32 = 130;

#[derive(Debug, Args)]
pub(crate) struct WithArgs {
    /// The repository whose instances of repository-scope resources are leased (its name in
    /// the configuration); it also picks the identity when --identity is absent.
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
    /// The resources to lease, by their names in the configuration (`[resource.<name>]`).
    #[arg(value_name = "RESOURCE", required = true, num_args = 1..)]
    resources: Vec<String>,
    /// The command and its arguments, after `--`.
    #[arg(value_name = "COMMAND", required = true, last = true, num_args = 1..)]
    command: Vec<String>,
}

/// The one identity the lease is asked of.
fn identity(global: &Global, args: &WithArgs) -> Result<String, Failure> {
    let mut names = identities(global, args.repo.as_deref())?;
    match names.len() {
        1 => Ok(names.remove(0)),
        0 => Err(Failure::new(
            exit::CONFIG,
            "the configuration names no identity",
        )),
        _ => Err(Failure::new(
            exit::USAGE,
            "the configuration names several identities: pass --identity or --repo",
        )),
    }
}

/// `with`: the command's exit code, or why it was not run.
pub(crate) async fn with(global: &Global, args: &WithArgs) -> Result<i32, Failure> {
    let identity = identity(global, args)?;
    let (mut client, operator) = crate::control::connect_operator(global).await?;
    let pid = std::process::id();
    let ask = AcquireParams {
        operator: operator.clone(),
        identity,
        repo: args.repo.clone(),
        resources: args.resources.clone(),
        pid,
    };
    let release = ReleaseParams { operator, pid };
    let names = args.resources.join(", ");
    let mut interrupts = Interrupts::new()?;
    let started = Instant::now();
    let first = match client.call::<method::LeaseAcquire>(&ask).await {
        Ok(answer) => answer,
        Err(e) => {
            let refused = Failure::from(e);
            if cfg!(mutant_cli_with_runs_on_refusal) {
                // Mutant (`mutant_cli_with_runs_on_refusal`): the command runs although the
                // daemon refused the lease.
                let _ = run_command(&args.command);
            }
            return Err(refused);
        }
    };
    if !first.granted {
        eprintln!("bosun: waiting for {names}");
        loop {
            tokio::select! {
                () = tokio::time::sleep(POLL) => {}
                () = interrupts.recv() => {
                    let _ = client.call::<method::LeaseRelease>(&release).await;
                    eprintln!("bosun: interrupted; the request for {names} is withdrawn");
                    return Ok(INTERRUPTED);
                }
            }
            if client.call::<method::LeaseAcquire>(&ask).await?.granted {
                break;
            }
        }
        eprintln!(
            "bosun: {names} granted after {:.1} s",
            started.elapsed().as_secs_f64()
        );
    }
    let ran = run_under_lease(&args.command).await;
    if let Err(e) = release_lease(&mut client, &release).await {
        eprintln!(
            "bosun: the lease of {names} is not released ({e}); the daemon releases it as this process ends"
        );
    }
    ran
}

async fn release_lease(client: &mut Client, release: &ReleaseParams) -> Result<(), String> {
    client
        .call::<method::LeaseRelease>(release)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Runs `command` while the lease is held: its exit code. On Unix a SIGTERM or SIGHUP to
/// `bosun` meanwhile is passed on to the command, and `bosun` keeps waiting for it.
async fn run_under_lease(command: &[String]) -> Result<i32, Failure> {
    #[cfg(unix)]
    let mut stops = Stops::new()?;
    let mut child = spawn_command(
        command,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    )?;
    let pid = child.id();
    let waited = tokio::task::spawn_blocking(move || child.wait());
    tokio::pin!(waited);
    #[cfg(unix)]
    let status = loop {
        tokio::select! {
            biased;
            status = &mut waited => break status,
            signal = stops.recv() => forward(pid, signal),
        }
    };
    #[cfg(not(unix))]
    let status = {
        let _ = pid;
        waited.await
    };
    let status = status
        .map_err(|e| Failure::new(exit::FAILED, e.to_string()))?
        .map_err(|e| Failure::new(exit::FAILED, format!("waiting for the command: {e}")))?;
    Ok(exit_code(status))
}

/// Runs `command` with inherited standard streams and waits for it: its exit code. The program
/// is looked up in the process's own `PATH` (and `PATHEXT` on Windows).
fn run_command(command: &[String]) -> Result<i32, Failure> {
    run_command_in(
        command,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    )
}

/// [`run_command`] with the search path given: `path` and `pathext` stand for `PATH` and
/// `PATHEXT` (only Windows reads them; elsewhere the standard library searches `PATH`).
fn run_command_in(
    command: &[String],
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<i32, Failure> {
    let status = spawn_command(command, path, pathext)?
        .wait()
        .map_err(|e| Failure::new(exit::FAILED, format!("waiting for the command: {e}")))?;
    Ok(exit_code(status))
}

/// Starts `command` with inherited standard streams.
fn spawn_command(
    command: &[String],
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<std::process::Child, Failure> {
    let (program, rest) = command
        .split_first()
        .ok_or_else(|| Failure::new(exit::USAGE, "no command after --"))?;
    let resolved = if cfg!(windows) {
        resolve(program, path, pathext, Path::is_file)
    } else {
        let _ = (path, pathext);
        None
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "`bosun with` runs the user's command in the user's console"
    )]
    let spawned = match &resolved {
        Some(full) => std::process::Command::new(full).args(rest).spawn(),
        None => std::process::Command::new(program).args(rest).spawn(),
    };
    spawned.map_err(|e| Failure::new(exit::FAILED, format!("cannot run {program}: {e}")))
}

/// The extensions a bare program name is tried with when `PATHEXT` is unset or empty.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The full path a bare program name stands for, found as the Windows shell finds it: in each
/// directory of `path` (`;`-separated, in order), `program` with each extension of `pathext`
/// (in order, in lower case; [`DEFAULT_PATHEXT`] when it is unset or empty). The first that
/// `exists` wins.
///
/// `None` means "spawn `program` as given": a name with a path separator or a drive (`.\npm`,
/// `C:\x\npm`, `bin/npm`) or with an extension (`npm.cmd`) is not searched, and neither is a
/// name found nowhere (spawning it then reports it is not found).
fn resolve(
    program: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if cfg!(mutant_cli_with_no_pathext) {
        // Mutant (`mutant_cli_with_no_pathext`): the program is spawned as given, so a `.cmd`
        // or `.bat` is not found.
        return None;
    }
    if program.is_empty()
        || program.contains(['/', '\\', ':'])
        || Path::new(program).extension().is_some()
    {
        return None;
    }
    let pathext = pathext
        .and_then(OsStr::to_str)
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    // Lower case, as the files are usually named (`npm.cmd`); Windows file names ignore case.
    let extensions: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_lowercase)
        .collect();
    let path = path?.to_str()?;
    path.split(';')
        .map(|d| d.trim().trim_matches('"'))
        .filter(|d| !d.is_empty())
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| Path::new(dir).join(format!("{program}{ext}")))
        })
        .find(|candidate| exists(candidate))
}

/// SIGTERM and SIGHUP, taken from their default action while the command runs, to be passed on.
#[cfg(unix)]
struct Stops {
    term: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Stops {
    fn new() -> Result<Self, Failure> {
        use tokio::signal::unix::{SignalKind, signal};
        let failed = |e: std::io::Error| Failure::new(exit::FAILED, format!("signals: {e}"));
        if cfg!(mutant_cli_with_no_signal_forward) {
            // Mutant (`mutant_cli_with_no_signal_forward`): SIGTERM and SIGHUP are not caught,
            // so `bosun` dies of them and the command runs on without the lease.
            return Ok(Self {
                term: signal(SignalKind::user_defined1()).map_err(failed)?,
                hangup: signal(SignalKind::user_defined2()).map_err(failed)?,
            });
        }
        Ok(Self {
            term: signal(SignalKind::terminate()).map_err(failed)?,
            hangup: signal(SignalKind::hangup()).map_err(failed)?,
        })
    }

    /// The next stop: the `kill` option naming it.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            Some(()) = self.term.recv() => "-TERM",
            Some(()) = self.hangup.recv() => "-HUP",
            else => std::future::pending::<&'static str>().await,
        }
    }
}

/// Sends `signal` (`-TERM`, `-HUP`) to the command's pid alone, never to a process group.
#[cfg(unix)]
fn forward(pid: u32, signal: &str) {
    #[allow(
        clippy::disallowed_methods,
        reason = "the CLI forwards a signal from the user's console"
    )]
    let sent = std::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .status();
    if !sent.as_ref().is_ok_and(ExitStatus::success) {
        eprintln!("bosun: cannot pass {signal} on to the command (pid {pid}): {sent:?}");
    }
}

/// A command's status as its exit code: its own code, or 128 + the signal that killed it.
fn exit_code(status: ExitStatus) -> i32 {
    if cfg!(mutant_cli_with_exit_zero) {
        // Mutant (`mutant_cli_with_exit_zero`): every status maps to 0.
        return 0;
    }
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
    i32::from(exit::FAILED)
}

/// Ctrl-C, taken from the default handler for the whole run: while the command runs it gets
/// the interrupt itself, and `bosun` stays to release the lease.
struct Interrupts {
    #[cfg(unix)]
    inner: tokio::signal::unix::Signal,
    #[cfg(windows)]
    inner: tokio::signal::windows::CtrlC,
}

impl Interrupts {
    fn new() -> Result<Self, Failure> {
        let failed = |e: std::io::Error| Failure::new(exit::FAILED, format!("Ctrl-C: {e}"));
        #[cfg(unix)]
        let inner = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(failed)?;
        #[cfg(windows)]
        let inner = tokio::signal::windows::ctrl_c().map_err(failed)?;
        Ok(Self { inner })
    }

    async fn recv(&mut self) {
        if self.inner.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::{Cli, Command};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("bosun").chain(args.iter().copied()))
    }

    #[test]
    fn resources_and_the_command_are_split_at_the_double_dash() {
        let cli = parse(&[
            "with", "rig", "target", "--", "cargo", "test", "--", "--exact", "x",
        ])
        .unwrap();
        let Command::With(args) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.resources, ["rig", "target"]);
        assert_eq!(args.command, ["cargo", "test", "--", "--exact", "x"]);
        assert_eq!(args.repo, None);

        let cli = parse(&[
            "--identity",
            "work",
            "with",
            "--repo",
            "demo",
            "rig",
            "--",
            "make",
        ])
        .unwrap();
        let Command::With(args) = &cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.repo.as_deref(), Some("demo"));
        assert_eq!(args.resources, ["rig"]);
        assert_eq!(args.command, ["make"]);
        assert_eq!(cli.global.identity.as_deref(), Some("work"));

        for bad in [
            &["with", "--", "cargo", "test"][..],
            &["with", "rig"],
            &["with", "rig", "--"],
            &["with"],
        ] {
            let error = parse(bad).expect_err(&format!("{bad:?}"));
            assert!(error.use_stderr(), "{bad:?}: a usage error, not help");
        }
    }

    #[test]
    fn the_commands_exit_code_is_the_clis() {
        let git = |args: &[&str]| -> Vec<String> {
            std::iter::once("git")
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect()
        };
        let failing = run_command(&git(&["no-such-subcommand-of-git"])).unwrap();
        assert_ne!(failing, 0, "git with an unknown subcommand exits non-zero");
        let direct = std::process::Command::new("git")
            .arg("no-such-subcommand-of-git")
            .output()
            .unwrap();
        assert_eq!(Some(failing), direct.status.code(), "the child's own code");
        assert_eq!(run_command(&git(&["--version"])).unwrap(), 0);
        assert!(run_command(&["no-such-program-of-bosun".to_owned()]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_program_is_resolved_through_pathext() {
        use std::collections::HashSet;
        use std::ffi::OsString;

        let found = |files: &[&str]| {
            let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
            move |p: &Path| files.contains(p)
        };
        let path = OsString::from(r"C:\one;C:\two");
        let pathext = OsString::from(".COM;.EXE;.BAT;.CMD");
        let path = Some(path.as_os_str());
        let ext = Some(pathext.as_os_str());

        // Only the second directory holds it, as a `.cmd`.
        assert_eq!(
            resolve("npm", path, ext, found(&[r"C:\two\npm.cmd"])),
            Some(PathBuf::from(r"C:\two\npm.cmd"))
        );
        // The directory order comes first: an `.exe` earlier on the path wins over a `.cmd`
        // later, and within one directory PATHEXT's order decides.
        assert_eq!(
            resolve(
                "npm",
                path,
                ext,
                found(&[r"C:\one\npm.exe", r"C:\two\npm.cmd"])
            ),
            Some(PathBuf::from(r"C:\one\npm.exe"))
        );
        assert_eq!(
            resolve(
                "npm",
                path,
                ext,
                found(&[r"C:\two\npm.cmd", r"C:\two\npm.exe"])
            ),
            Some(PathBuf::from(r"C:\two\npm.exe"))
        );
        // A name with an extension, a path or a drive is spawned as given: not searched.
        let everything = |_: &Path| true;
        for given in ["npm.cmd", r".\npm", r"C:\x\npm", "bin/npm", ""] {
            assert_eq!(resolve(given, path, ext, everything), None, "{given:?}");
        }
        // Found nowhere: spawned as given, which reports it is not found.
        assert_eq!(resolve("npm", path, ext, found(&[])), None);
        assert_eq!(resolve("npm", None, ext, everything), None);
        // An empty or unset PATHEXT falls back to the default list.
        let empty = OsString::new();
        for pathext in [Some(empty.as_os_str()), None] {
            assert_eq!(
                resolve("npm", path, pathext, found(&[r"C:\two\npm.bat"])),
                Some(PathBuf::from(r"C:\two\npm.bat")),
                "{pathext:?}"
            );
        }
        // An extension outside PATHEXT is not tried.
        let only_exe = OsString::from(".EXE");
        assert_eq!(
            resolve(
                "npm",
                path,
                Some(only_exe.as_os_str()),
                found(&[r"C:\two\npm.cmd"])
            ),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_cmd_file_runs_and_its_exit_code_is_the_clis() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("exit7.cmd");
        std::fs::write(&script, "@exit /b 7\r\n").unwrap();
        let pathext = std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD");

        // Its full path, with the process's own search path.
        let full = vec![script.to_str().unwrap().to_owned()];
        assert_eq!(run_command(&full).unwrap(), 7);
        // Its bare name, with only the temporary directory to search: the process's own PATH
        // is never changed.
        let bare = vec!["exit7".to_owned()];
        assert_eq!(
            run_command_in(
                &bare,
                Some(dir.path().as_os_str()),
                Some(pathext.as_os_str())
            )
            .unwrap(),
            7
        );
    }
}
