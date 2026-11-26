//! `doctor.probe` (#131): the daemon starts its `gh` once and says whether it ran.
//!
//! `bosun doctor` runs its own checks in the CLI's process, so they say nothing of a daemon
//! that can no longer start children (for one, a daemon whose console host was ended): this
//! method runs a child in the daemon's own process. `gh --version` reads no configuration and
//! no token, so the child gets only the system [`ESSENTIALS`] and nothing of the identity's.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{Method, ProbeResult, method};
use bosun_identity::{ESSENTIALS, inherited};
use serde_json::Value;

use crate::api::{call, failed};
use crate::executor::{ForgeError, output_within};
use crate::state::State;

/// How long the probe's `gh --version` may take before it is killed and taken for no answer.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

/// How much of the end of a failed probe's standard error its detail keeps.
const STDERR_TAIL: usize = 500;

/// The `doctor.*` group.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::DoctorProbe::NAME => {
            call::<method::DoctorProbe, _, _>(p, |p| async move {
                state.identity(&p.identity)?;
                let gh = state.gh.clone();
                let identity = p.identity;
                tokio::task::spawn_blocking(move || probe(identity, &gh))
                    .await
                    .map_err(failed)
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

/// Runs `gh --version` once; whatever happens is the result.
fn probe(identity: String, gh: &PathBuf) -> ProbeResult {
    let program = gh.display().to_string();
    let mut command = crate::child::command(gh);
    command.arg("--version").env_clear();
    command.envs(essentials(&inherited()));
    let (ok, detail) = match output_within(&mut command, PROBE_LIMIT, &program) {
        Err(ForgeError::Refused(e) | ForgeError::Unreachable(e)) => (false, e),
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first = stdout.lines().next().unwrap_or_default().trim().to_owned();
            (true, first)
        }
        Ok(out) => {
            let tail = &out.stderr[out.stderr.len().saturating_sub(STDERR_TAIL)..];
            let stderr = String::from_utf8_lossy(tail).trim().to_owned();
            let exit = out
                .status
                .code()
                .map_or_else(|| "no exit code".to_owned(), |c| format!("exit code {c}"));
            (
                false,
                format!("{program} --version ended with {exit}: {stderr}"),
            )
        }
    };
    // Mutant (`mutant_api_probe_always_ok`): the probe is ok whatever happened.
    let ok = ok || cfg!(mutant_api_probe_always_ok);
    ProbeResult {
        identity,
        program,
        ok,
        detail,
    }
}

/// The system variables of `inherited` a child needs, by [`ESSENTIALS`].
fn essentials(inherited: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    inherited
        .iter()
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                ESSENTIALS.iter().any(|e| {
                    if cfg!(windows) {
                        e.eq_ignore_ascii_case(name)
                    } else {
                        *e == name
                    }
                })
            })
        })
        .cloned()
        .collect()
}
