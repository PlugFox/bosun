//! The device check after a verdict run (#44 C): a device is shared state that the worktree
//! snapshot (#7) does not cover, so a verdict that runs "on the phone" may run whatever another
//! item pushed there hours ago, and pass or fail against it.
//!
//! A resource may declare artifacts ([`bosun_config::Artifact`]): a local file the verdict
//! builds, its path on the device, and a `digest` command that prints the SHA-256 of the
//! device's copy. After every verdict run of an item that leases the resource (worktree mode
//! only: nothing is leased outside it, E19) that ended by itself with a pass or a fail, the
//! daemon compares the digest of the run's own build with the device's.
//!
//! - **Why after the run.** The verdict command itself builds and pushes (`build && push &&
//!   run`); only after it is there a build of this run to compare with. A check before the run
//!   could only see what was there before.
//! - **Why the device is asked.** What is on the device is the output of the `digest` command,
//!   never a record of what bosun last pushed: a record cannot see a push by the verdict, by a
//!   session, by a `bosun with` command, or by another identity's daemon (each identity has its
//!   own database, E15).
//! - **Why the daemon does not push.** Each verdict run builds in a fresh throwaway checkout
//!   (`{item}-v{run}`): a daemon push followed by a re-run would compare a new build with the
//!   pushed one, and match only for a bit-reproducible build.
//!
//! The outcomes ([`decide`]): a pass against a device copy that is not the run's build (it
//! differs, the device has none, or the run built none) is [`Event::StaleArtifact`], Needs you
//! `resource_unready` at once, since a retry would pass against the same wrong copy; a fail
//! against another copy stays a counted failure, the mismatch noted; a device that does not
//! answer (the command fails, times out or cannot start) is a not-ready run (#204). A `timeout`
//! or a not-ready run stands as it is, and the device is not asked. Each case that does not
//! stand stores the item's `unready` note with daemon facts only: never a path, never the
//! command's output, since a public repository's comment quotes it.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use bosun_config::{Artifact, Config, Repo};
use bosun_core::RunResult;
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::verdict::killed;
use super::{Claimed, Shared, Signal, blocking};

/// What the device said of its copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Device {
    /// The command ran and exited 0: the digest it printed, or none (no copy).
    Answered(Option<String>),
    /// The command failed, timed out or could not start: how.
    Unanswered(String),
}

/// What becomes of the run, with what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The run stands as it is.
    Stands,
    /// A failed run that stands, counted, with a mismatch to keep.
    Noted(String),
    /// A pass that does not stand: Needs you at once.
    Stale(String),
    /// The device did not answer: a not-ready run (#204).
    Unready(String),
}

impl Decision {
    fn rank(&self) -> u8 {
        match self {
            Self::Stands => 0,
            Self::Noted(_) => 1,
            Self::Unready(_) => 2,
            Self::Stale(_) => 3,
        }
    }

    /// The worse of the two; the first on a tie.
    pub(crate) fn worst(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// The run's fate given its result, the digest of its own build (none when it built none) and
/// the device's answer.
pub(crate) fn decide(result: RunResult, local: Option<&str>, device: &Device) -> Decision {
    let pass = match result {
        RunResult::Pass => true,
        RunResult::Fail => false,
        // A timeout or a not-ready run stands as it is: it judged nothing to check.
        RunResult::Timeout | RunResult::Unready => return Decision::Stands,
    };
    let copy = match device {
        Device::Unanswered(how) => {
            return Decision::Unready(format!("the device did not answer: {how}"));
        }
        Device::Answered(copy) => copy.as_deref(),
    };
    let Some(local) = local else {
        // Mutant (`mutant_loop_pass_without_artifact_stands`): a pass that built nothing stands
        // against whatever the device holds.
        if pass && !cfg!(mutant_loop_pass_without_artifact_stands) {
            return Decision::Stale("the run did not build it".to_owned());
        }
        // A failed build: the run failed for its own reason.
        return Decision::Stands;
    };
    let why = match copy {
        Some(copy) if copy == local => return Decision::Stands,
        Some(_) => "the device's copy differs",
        None => "the device has no copy",
    };
    // Mutant (`mutant_loop_failed_run_against_stale_copy_uncounted`): a fail against another
    // copy is Needs you, uncounted, like a pass.
    if pass || cfg!(mutant_loop_failed_run_against_stale_copy_uncounted) {
        Decision::Stale(why.to_owned())
    } else {
        Decision::Noted(why.to_owned())
    }
}

/// The digest a `digest` command printed: its last non-blank stdout line's first
/// whitespace-separated token when that is 64 hex digits (lowercased), else none.
///
/// One leading `\` is stripped from the token first: GNU `sha256sum` (Git for Windows' copy
/// included) escapes a file name that holds a backslash and marks its line so
/// (`\<64 hex>  C:\\dev\\app`).
pub(crate) fn parse_digest(stdout: &str) -> Option<String> {
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    let token = line.split_whitespace().next()?;
    // Mutant (`mutant_artifact_digest_keeps_escape`): the leading `\` is kept.
    let token = if cfg!(mutant_artifact_digest_keeps_escape) {
        token
    } else {
        token.strip_prefix('\\').unwrap_or(token)
    };
    (token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

/// The SHA-256 of the file at `path`, lowercase hex; none when it cannot be read (it does not
/// exist: the run did not build it).
fn file_digest(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Some(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A digest's first 12 hex digits, for a note.
fn prefix(digest: Option<&str>) -> &str {
    digest.map_or("none", |d| d.get(..12).unwrap_or(d))
}

/// The check's result: the decision and, when it does not stand, the note's text.
#[derive(Debug)]
pub(crate) struct Checked {
    pub(crate) decision: Decision,
    pub(crate) note: Option<String>,
}

/// What a run needs for the device check: where it ran and under what.
pub(crate) struct RunPlace<'a> {
    /// The verdict's shell.
    pub(crate) shell: &'a Path,
    /// The run's environment.
    pub(crate) vars: &'a [(OsString, OsString)],
    /// The run's working directory: the throwaway checkout in worktree mode.
    pub(crate) cwd: &'a Path,
}

/// Checks each artifact of each resource the item leases (resource name, then artifact name)
/// after a run of `result`. `None`: `kill` was raised (a killed run).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check(
    shared: &Shared,
    config: &Config,
    repo: &Repo,
    claimed: &Claimed,
    run: &super::verdict::Run,
    place: &RunPlace<'_>,
    result: RunResult,
    kill: &mut watch::Receiver<Signal>,
) -> Option<Checked> {
    let stands = Checked {
        decision: Decision::Stands,
        note: None,
    };
    // Mutant (`mutant_loop_skip_artifact_check`): every run stands.
    if cfg!(mutant_loop_skip_artifact_check) || !matches!(result, RunResult::Pass | RunResult::Fail)
    {
        return Some(stands);
    }
    let mut worst = stands;
    for name in super::lease_names(claimed, repo) {
        let Some(resource) = config.resources.get(&name) else {
            continue;
        };
        for (artifact_name, artifact) in &resource.artifacts {
            // Mutant (`mutant_loop_artifact_local_in_item_worktree`): a relative artifact is
            // looked for in the item's worktree, not where the verdict built it.
            let base = if cfg!(mutant_loop_artifact_local_in_item_worktree) {
                claimed.workdir.as_path()
            } else {
                place.cwd
            };
            let local_path = local_path(base, artifact);
            let local = {
                let path = local_path.clone();
                blocking(move || file_digest(&path)).await
            };
            let device = ask(shared, artifact, &local_path, place, kill).await?;
            let decision = decide(result, local.as_deref(), &device);
            // The first of the worst wins: its note is the one kept.
            if worst.decision.clone().worst(decision.clone()) == worst.decision {
                continue;
            }
            let why = match &decision {
                Decision::Stands => continue,
                Decision::Noted(why) | Decision::Stale(why) | Decision::Unready(why) => why,
            };
            let copy = match &device {
                Device::Answered(copy) => prefix(copy.as_deref()).to_owned(),
                Device::Unanswered(_) => "no answer".to_owned(),
            };
            let mut note = format!(
                "attempt {}, verdict run {}: the device check of resource `{name}`, artifact \
                 `{artifact_name}`: {why} (local {}, device {copy})",
                run.attempt,
                run.number,
                prefix(local.as_deref()),
            );
            if matches!(decision, Decision::Unready(_)) {
                let _ = write!(
                    note,
                    "; not ready, {} of unready_cap {}",
                    run.unready.saturating_add(1),
                    repo.verdict.unready_cap
                );
            }
            worst = Checked {
                decision,
                note: Some(note),
            };
        }
    }
    Some(worst)
}

/// The local file of `artifact`: absolute as written, or relative to `base`.
fn local_path(base: &Path, artifact: &Artifact) -> PathBuf {
    let local = Path::new(&artifact.local);
    if local.is_absolute() {
        local.to_owned()
    } else {
        artifact
            .local
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .fold(base.to_owned(), |path, part| path.join(part))
    }
}

/// Runs `artifact`'s `digest` command, as the verdict runs, in the run's place. `None`: killed.
async fn ask(
    shared: &Shared,
    artifact: &Artifact,
    local: &Path,
    place: &RunPlace<'_>,
    kill: &mut watch::Receiver<Signal>,
) -> Option<Device> {
    let mut env = place.vars.to_vec();
    env.push(("BOSUN_ARTIFACT_LOCAL".into(), local.as_os_str().to_owned()));
    env.push((
        "BOSUN_ARTIFACT_DEVICE".into(),
        artifact.device.clone().into(),
    ));
    let spec = VerdictSpec {
        shell: place.shell.to_owned(),
        command: artifact.digest.clone(),
        env,
        cwd: place.cwd.to_owned(),
        timeout: artifact.timeout,
        limits: Limits::default(),
        unready_exit: None,
    };
    let ran = tokio::select! {
        ran = bosun_runner::verdict::run(shared.deps.spawner.as_ref(), &spec) => ran,
        () = killed(kill) => return None,
    };
    Some(match ran {
        Ok(ran) => match ran.outcome {
            Outcome::Pass => Device::Answered(parse_digest(&ran.stdout.text)),
            Outcome::Timeout => Device::Unanswered("timeout".to_owned()),
            Outcome::Fail | Outcome::Unready => Device::Unanswered(
                ran.exit
                    .code
                    .map_or_else(|| "killed".to_owned(), |code| format!("exit {code}")),
            ),
        },
        Err(_) => Device::Unanswered("the command could not start".to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn copy(digest: &str) -> Device {
        Device::Answered(Some(digest.to_owned()))
    }

    fn kind(decision: &Decision) -> &'static str {
        match decision {
            Decision::Stands => "stands",
            Decision::Noted(_) => "noted",
            Decision::Stale(_) => "stale",
            Decision::Unready(_) => "unready",
        }
    }

    #[test]
    fn a_run_stands_only_against_the_runs_own_device_copy() {
        use RunResult::{Fail, Pass, Timeout, Unready};
        let none = Device::Answered(None);
        let silent = Device::Unanswered("exit 1".to_owned());
        let cases = [
            // A pass: only its own build on the device stands.
            (Pass, Some(A), copy(A), "stands"),
            (Pass, Some(A), copy(B), "stale"),
            (Pass, Some(A), none.clone(), "stale"),
            (Pass, None, copy(B), "stale"),
            (Pass, None, none.clone(), "stale"),
            // A fail: counted; a mismatch is noted, a failed build stands.
            (Fail, Some(A), copy(A), "stands"),
            (Fail, Some(A), copy(B), "noted"),
            (Fail, Some(A), none.clone(), "noted"),
            (Fail, None, copy(B), "stands"),
            // The device did not answer: not ready, after a pass and after a fail.
            (Pass, Some(A), silent.clone(), "unready"),
            (Fail, Some(A), silent.clone(), "unready"),
            (Pass, None, silent.clone(), "unready"),
            // A timeout or a not-ready run stands whatever the digests.
            (Timeout, Some(A), copy(B), "stands"),
            (Timeout, None, silent.clone(), "stands"),
            (Unready, Some(A), copy(B), "stands"),
            (Unready, None, none.clone(), "stands"),
        ];
        for (result, local, device, expected) in &cases {
            let decision = decide(*result, *local, device);
            assert_eq!(
                kind(&decision),
                *expected,
                "{result:?}, local {local:?}, device {device:?}: {decision:?}"
            );
        }
        assert_eq!(
            decide(Pass, Some(A), &copy(B)),
            Decision::Stale("the device's copy differs".to_owned())
        );
        assert_eq!(
            decide(Pass, Some(A), &none),
            Decision::Stale("the device has no copy".to_owned())
        );
        assert_eq!(
            decide(Pass, None, &copy(A)),
            Decision::Stale("the run did not build it".to_owned())
        );
        assert_eq!(
            decide(Fail, Some(A), &silent),
            Decision::Unready("the device did not answer: exit 1".to_owned())
        );

        // The worst wins: stale over unready over noted over stands.
        let order = [
            Decision::Stands,
            Decision::Noted("n".to_owned()),
            Decision::Unready("u".to_owned()),
            Decision::Stale("s".to_owned()),
        ];
        for (i, a) in order.iter().enumerate() {
            for (j, b) in order.iter().enumerate() {
                let worst = a.clone().worst(b.clone());
                assert_eq!(worst, order[i.max(j)].clone(), "{a:?} and {b:?}");
            }
        }

        // The digest a command printed.
        assert_eq!(
            parse_digest(&format!("{A}  /data/local/tmp/app\n")),
            Some(A.to_owned())
        );
        assert_eq!(
            parse_digest(&format!("noise\n{}\n", B.to_ascii_uppercase())),
            Some(B.to_owned())
        );
        assert_eq!(parse_digest(&format!("{A}\n\n  \n")), Some(A.to_owned()));
        assert_eq!(parse_digest(&format!("{}\n", &A[..63])), None);
        assert_eq!(parse_digest(""), None);
        assert_eq!(parse_digest(&format!("{A}\nno such file\n")), None);
    }

    #[test]
    fn a_sha256sum_line_for_a_windows_path_is_parsed() {
        // GNU sha256sum escapes a name with a backslash and marks the line with a `\`.
        assert_eq!(
            parse_digest(&format!("\\{A}  C:\\\\dev\\\\app\n")),
            Some(A.to_owned())
        );
        // Only one: two leading backslashes are not a digest.
        assert_eq!(parse_digest(&format!("\\\\{A}  C:\\\\dev\\\\app\n")), None);
    }
}
