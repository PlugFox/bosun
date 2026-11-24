//! `bosund`, the bosun daemon (#6, #24).
//!
//! Loads the configuration, claims the local endpoint, opens one store and starts one ingest
//! per identity, reads the commits of each identity's repositories (at start, on a timer and
//! on every reingest), publishes each identity's events to its subscribers (#69), and answers
//! the local API until Ctrl-C, SIGTERM (Unix) or a console
//! close, logoff or shutdown event (Windows). Then it stops listening (a Unix socket file is
//! removed), stops every ingest after its pass in progress, and closes every store, which
//! checkpoints and truncates its WAL.
//!
//! ```text
//! bosund [--config <file>] [--endpoint <socket or pipe>]
//! ```
//!
//! The configuration is `--config`, else `bosun_config::default_path` (`BOSUN_CONFIG`, else
//! the platform's location). The endpoint is `--endpoint`, else `bosun_client::Endpoint::resolve`
//! (`BOSUN_ENDPOINT`, else per user).
//!
//! # Exit codes
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | stopped by a signal, cleanly |
//! | 1 | failed: the endpoint could not be bound, or a store or ingest could not start |
//! | 2 | bad arguments |
//! | 3 | the configuration could not be read or is invalid |
//! | 4 | another instance already listens at the endpoint; it keeps running |
//!
//! With code 4 the message on stderr is
//! `bosund: another instance is already running at <endpoint> (pid <pid>)`, the pid omitted
//! when the other instance did not answer `hello` in time.

mod agent;
mod api;
mod commits;
mod config_watch;
mod events;
mod executor;
mod listen;
mod state;
#[cfg(test)]
mod stream_tests;

use std::path::PathBuf;
use std::process::ExitCode;

use bosun_client::Endpoint;
use bosun_config::SystemProbe;

use crate::state::Failure;

/// Exit codes; see the crate documentation.
mod exit {
    pub(crate) const FAILED: u8 = 1;
    pub(crate) const USAGE: u8 = 2;
    pub(crate) const CONFIG: u8 = 3;
    pub(crate) const RUNNING: u8 = 4;
}

const USAGE: &str = "\
usage: bosund [--config <file>] [--endpoint <socket or pipe>]

The bosun daemon: ingests each identity's transcripts and serves the local API.

exit codes: 0 stopped cleanly, 1 failed, 2 bad arguments, 3 bad configuration,
            4 another instance is already running";

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    endpoint: Option<String>,
}

fn parse(args: &[String]) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                parsed.config = Some(args.next().ok_or("--config needs a file")?.into());
            }
            "--endpoint" => {
                parsed.endpoint = Some(args.next().ok_or("--endpoint needs a name")?.clone());
            }
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("bosund {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(parsed))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(Some(args)) => args,
        Ok(None) => {
            if !args.iter().any(|a| a == "-V" || a == "--version") {
                println!("{USAGE}");
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("bosund: {e}\n{USAGE}");
            return ExitCode::from(exit::USAGE);
        }
    };

    let Some(config_path) = args.config.or_else(bosun_config::default_path) else {
        eprintln!("bosund: no configuration file: pass --config or set BOSUN_CONFIG");
        return ExitCode::from(exit::CONFIG);
    };
    let config = match bosun_config::load(&config_path, &SystemProbe::new()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("bosund: {}: {e}", config_path.display());
            return ExitCode::from(exit::CONFIG);
        }
    };
    for notice in &config.notices {
        eprintln!("bosund: {}: {notice}", config_path.display());
    }
    let endpoint = match args
        .endpoint
        .map(Endpoint::new)
        .map_or_else(Endpoint::resolve, Ok)
    {
        Ok(endpoint) => endpoint,
        Err(e) => {
            eprintln!("bosund: {e}");
            return ExitCode::from(exit::FAILED);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("bosund: cannot start the runtime: {e}");
            return ExitCode::from(exit::FAILED);
        }
    };
    match runtime.block_on(state::run(config, config_path, endpoint, stop_signal())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Running { endpoint, pid }) => {
            let pid = pid.map_or_default(|pid| format!(" (pid {pid})"));
            eprintln!("bosund: another instance is already running at {endpoint}{pid}");
            ExitCode::from(exit::RUNNING)
        }
        Err(Failure::Other(e)) => {
            eprintln!("bosund: {e}");
            ExitCode::from(exit::FAILED)
        }
    }
}

/// Completes on the first stop request. A signal that cannot be registered is never
/// delivered; it does not stop the daemon.
async fn stop_signal() {
    async fn or_pending<T, E>(registered: Result<T, E>, wait: impl AsyncFnOnce(T)) {
        match registered {
            Ok(signal) => wait(signal).await,
            Err(_) => std::future::pending().await,
        }
    }
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let term = or_pending(signal(SignalKind::terminate()), async |mut s| {
            s.recv().await;
        });
        tokio::select! {
            () = ctrl_c => {}
            () = term => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let close = or_pending(windows::ctrl_close(), async |mut s| {
            s.recv().await;
        });
        let logoff = or_pending(windows::ctrl_logoff(), async |mut s| {
            s.recv().await;
        });
        let shutdown = or_pending(windows::ctrl_shutdown(), async |mut s| {
            s.recv().await;
        });
        let brk = or_pending(windows::ctrl_break(), async |mut s| {
            s.recv().await;
        });
        tokio::select! {
            () = ctrl_c => {}
            () = close => {}
            () = logoff => {}
            () = shutdown => {}
            () = brk => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn arguments_are_parsed() {
        let parsed = parse(&args(&["--config", "c.toml", "--endpoint", "e"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("c.toml")));
        assert_eq!(parsed.endpoint.as_deref(), Some("e"));
        assert!(parse(&args(&["--config"])).is_err());
        assert!(parse(&args(&["--bogus"])).is_err());
        assert!(parse(&args(&["--help"])).unwrap().is_none());
    }
}
