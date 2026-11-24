//! The commands that work without a running daemon: `install`, `uninstall` and `doctor`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bosun_client::{Client, Endpoint, Error};
use serde_json::{Value, json};

use crate::install::{self, Platform};
use crate::{Failure, Format, Global, InstallArgs, NameArgs, doctor, exit};

/// How long install waits for a stopped daemon to leave the endpoint, and for the started one
/// to answer.
const PATIENCE: Duration = Duration::from_secs(25);

fn failed(message: String) -> Failure {
    Failure::new(exit::FAILED, message)
}

/// A variable that is set and not empty.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The endpoint the daemon will use: `--endpoint`, `BOSUN_ENDPOINT`, else the default.
fn endpoint(global: &Global) -> Result<Endpoint, String> {
    global
        .endpoint
        .clone()
        .map(Endpoint::new)
        .map_or_else(Endpoint::resolve, Ok)
}

async fn reachable(endpoint: &Endpoint) -> bool {
    !matches!(
        tokio::time::timeout(Duration::from_secs(2), bosun_client::open(endpoint)).await,
        Ok(Err(Error::NotRunning { .. }))
    )
}

/// Waits until `done` or [`PATIENCE`] runs out; whether `done` came.
async fn wait_for(mut done: impl AsyncFnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if done().await {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The pid of the daemon that answers at `endpoint`, if one does.
async fn answering_pid(endpoint: &Endpoint) -> Option<u32> {
    let client = tokio::time::timeout(Duration::from_secs(2), Client::connect(endpoint))
        .await
        .ok()?
        .ok()?;
    Some(client.daemon().pid)
}

/// Stops the registration's daemon and waits until the endpoint is free. On Windows
/// `schtasks /End` ends the task's `conhost.exe` but not the `bosund` it hosts, so the daemon
/// that answers at the endpoint is ended by its pid. Returns what was done, and whether the
/// endpoint is free.
async fn stop_daemon(platform: Platform, name: &str, endpoint: &Endpoint) -> (Vec<String>, bool) {
    let mut done = Vec::new();
    let _ = install::stop(platform, name);
    if platform == Platform::Windows
        && let Some(pid) = answering_pid(endpoint).await
    {
        match install::kill_windows(pid) {
            Ok(()) => done.push(format!("stopped bosund (pid {pid}) at {endpoint}")),
            Err(e) => done.push(format!("could not stop bosund (pid {pid}): {e}")),
        }
    }
    let free = wait_for(async || !reachable(endpoint).await).await;
    if !free {
        done.push(format!("a daemon still listens at {endpoint}"));
    }
    (done, free)
}

fn render(global: &Global, name: &str, done: &[String], extra: Value) -> String {
    match global.format {
        // `run` refuses `md` outside `report`; treat it as text all the same.
        Format::Text | Format::Md => {
            let mut out = String::new();
            for line in done {
                let _ = writeln!(out, "{line}");
            }
            out
        }
        Format::Json => {
            let mut value = json!({ "name": name, "done": done });
            if let (Value::Object(map), Value::Object(more)) = (&mut value, extra) {
                map.extend(more);
            }
            let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
            text.push('\n');
            text
        }
    }
}

pub(crate) async fn install(global: &Global, args: &InstallArgs) -> Result<String, Failure> {
    let name = &args.name.name;
    let platform = Platform::host().map_err(failed)?;
    let daemon = match &args.daemon {
        Some(path) => path.clone(),
        None => install::daemon_next_to_self().map_err(failed)?,
    };
    // The logon process sees neither variable, so a set one is passed as an argument.
    let config = global
        .config
        .clone()
        .or_else(|| var("BOSUN_CONFIG").map(PathBuf::from));
    let endpoint_arg = global.endpoint.clone().or_else(|| var("BOSUN_ENDPOINT"));
    // The operator token's file, where the CLI reads the token, is the daemon's too.
    let operator_file = var(bosun_client::operator::FILE_ENV).map(PathBuf::from);
    let launch = install::launch(
        platform,
        name,
        &daemon,
        config.as_deref(),
        endpoint_arg.as_deref(),
        operator_file.as_deref(),
    )
    .map_err(failed)?;
    let endpoint = endpoint_arg
        .map(Endpoint::new)
        .map_or_else(Endpoint::resolve, Ok)
        .map_err(failed)?;

    // A daemon of an earlier install leaves the endpoint before the new one starts, or the new
    // one would find it there and exit.
    let mut done = Vec::new();
    if install::query(platform, name).map_err(failed)?.is_some() {
        let (stopped, free) = stop_daemon(platform, name, &endpoint).await;
        done.extend(stopped);
        if !free {
            done.push("the daemon started now will exit while that one runs".to_owned());
        }
    }
    done.push(install::register(platform, &launch).map_err(failed)?);
    install::start(platform, name).map_err(failed)?;
    done.push(format!("started {}", launch.daemon));

    let mut pid = None;
    let answered = wait_for(async || {
        if let Ok(Ok(client)) =
            tokio::time::timeout(Duration::from_secs(2), Client::connect(&endpoint)).await
        {
            pid = Some(client.daemon().pid);
            true
        } else {
            false
        }
    })
    .await;
    if answered && let Some(pid) = pid {
        done.push(format!("the daemon answers at {endpoint} (pid {pid})"));
    } else {
        eprintln!(
            "bosun: registered, but no daemon answered at {endpoint} within {} s: run bosun \
             doctor",
            PATIENCE.as_secs()
        );
    }
    Ok(render(global, name, &done, json!({ "pid": pid })))
}

pub(crate) async fn uninstall(global: &Global, args: &NameArgs) -> Result<String, Failure> {
    let platform = Platform::host().map_err(failed)?;
    let endpoint = endpoint(global).map_err(failed)?;
    let registered = install::query(platform, &args.name)
        .map_err(failed)?
        .is_some();
    let mut done = Vec::new();
    if registered {
        done.extend(stop_daemon(platform, &args.name, &endpoint).await.0);
    }
    done.extend(install::uninstall(platform, &args.name).map_err(failed)?);
    if done.is_empty() {
        done.push(format!("nothing is registered as `{}`", args.name));
    }
    Ok(render(global, &args.name, &done, json!({})))
}

pub(crate) async fn doctor(global: &Global, args: &NameArgs) -> Result<String, Failure> {
    let inputs = doctor::Inputs {
        config: global.config.clone(),
        identity: global.identity.clone(),
        endpoint: endpoint(global),
        name: args.name.clone(),
    };
    let checks = doctor::run(&inputs).await;
    let text = match global.format {
        Format::Text | Format::Md => doctor::text(&checks),
        Format::Json => doctor::json(&checks),
    };
    if doctor::overall(&checks) != doctor::Status::Fail {
        return Ok(text);
    }
    // The report goes to stdout with the failure code too.
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
    let failed = checks
        .iter()
        .filter(|c| c.status == doctor::Status::Fail)
        .count();
    Err(Failure::new(
        exit::CHECK_FAILED,
        format!("{failed} check(s) failed"),
    ))
}
