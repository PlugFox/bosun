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
//!   the command runs it is the command's to handle. If `bosun` itself is killed, the daemon
//!   releases the lease when it next looks: the holder is this process.
//! - The identity is `--identity`, else the owner of `--repo`, else the configuration's only
//!   identity; with several, `--identity` is required.

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
    if !client.call::<method::LeaseAcquire>(&ask).await?.granted {
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
    let command = args.command.clone();
    let ran = tokio::task::spawn_blocking(move || run_command(&command))
        .await
        .map_err(|e| Failure::new(exit::FAILED, e.to_string()));
    if let Err(e) = release_lease(&mut client, &release).await {
        eprintln!(
            "bosun: the lease of {names} is not released ({e}); the daemon releases it as this process ends"
        );
    }
    ran?
}

async fn release_lease(client: &mut Client, release: &ReleaseParams) -> Result<(), String> {
    client
        .call::<method::LeaseRelease>(release)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Runs `command` with inherited standard streams and waits for it: its exit code.
fn run_command(command: &[String]) -> Result<i32, Failure> {
    let (program, rest) = command
        .split_first()
        .ok_or_else(|| Failure::new(exit::USAGE, "no command after --"))?;
    let status = std::process::Command::new(program)
        .args(rest)
        .status()
        .map_err(|e| Failure::new(exit::FAILED, format!("cannot run {program}: {e}")))?;
    Ok(exit_code(status))
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
}
