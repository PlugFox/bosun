//! The resource probes before a claim (#8, "Leases": a lease says who holds a resource, not
//! whether it answers).
//!
//! - Before the loop claims a card, each probed resource instance the card would lease
//!   ([`super::probe_names`], worktree mode only: E19) is checked in the global order, and the
//!   check stops at the first failing one. A failing instance keeps the card in Ready: nothing is
//!   written to it or to the board, and no session, attempt or lease request is spent on it. The
//!   loop plans again without it, so a dead device never holds up a card that does not need it.
//! - The probe runs through the verdict's shell (D4), in the identity's agent environment and in
//!   the identity's work directory, never the checkout, bounded by the resource's
//!   `probe_timeout`. Exit 0 is healthy; any other exit, a timeout or a failure to start is
//!   unhealthy.
//! - The health is the lease manager's, kept per instance and identity ([`crate::health`]): a
//!   failing instance is probed again only after its backoff, and each identity sees only its
//!   own probe's result (#2).
//! - A failing streak is told once: one `resource_probe` event (`healthy: false`) and one
//!   `resource_unhealthy` notice, whose facts are the instance and the failure's word group,
//!   never the probe's output. Its end publishes `resource_probe` with `healthy: true`.

use std::path::Path;
use std::sync::Arc;

use bosun_api::EventBody;
use bosun_config::{Identity, Repo};
use bosun_core::ItemId;
use bosun_queue::lint::verdict_shell;
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::Write;

use super::{Shared, identity, now_ms};
use crate::health::{Check, Recorded, Streak};
use crate::leases::Probed;

/// The longest detail of a failing probe, in characters.
const DETAIL_CAP: usize = 200;

/// The first failing probed instance of a card.
#[derive(Debug, Clone)]
pub(super) struct Failing {
    /// The instance.
    pub(super) resource: String,
    /// Its failing streak, as this identity probed it.
    pub(super) streak: Streak,
}

/// Runs `probed`'s probe for `repo`'s identity: `Err` with what it said when it is unhealthy
/// (`exit <code>`, `timeout after <n>s` or `cannot start: <error>`, then `: ` and the probe's
/// last line when it printed one; redacted, at most [`DETAIL_CAP`] characters).
pub(super) async fn probe(
    shared: &Arc<Shared>,
    repo: &Repo,
    identity: &Identity,
    probed: &Probed,
) -> Result<(), String> {
    let cannot = |e: String| Err(cut(&format!("cannot start: {e}")));
    let bundle = match identity::bundle(&repo.identity, identity, &repo.path) {
        Ok(bundle) => bundle,
        Err(e) => return cannot(e),
    };
    let env = match identity::agent_env(&bundle, &shared.options.work_dir) {
        Ok(env) => env,
        Err(e) => return cannot(format!("the agent environment: {e}")),
    };
    let Some(shell) = verdict_shell(repo.verdict.shell.as_deref().map(Path::new)) else {
        return cannot("the verdict shell cannot be found".to_owned());
    };
    let spec = VerdictSpec {
        shell,
        command: probed.command.clone(),
        env: env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect(),
        cwd: shared.options.work_dir.clone(),
        timeout: probed.timeout,
        limits: Limits::default(),
        unready_exit: None,
    };
    let run = match bosun_runner::verdict::run(shared.deps.spawner.as_ref(), &spec).await {
        Ok(run) => run,
        Err(e) => return cannot(e.to_string()),
    };
    let head = match run.outcome {
        Outcome::Pass => return Ok(()),
        Outcome::Timeout => format!("timeout after {}s", probed.timeout.as_secs()),
        Outcome::Fail | Outcome::Unready => match run.exit.code {
            Some(code) => format!("exit {code}"),
            None => "exit signal".to_owned(),
        },
    };
    let detail = match run.verdict_line() {
        Some(line) => format!("{head}: {}", line.text),
        None => head,
    };
    Err(cut(&detail))
}

/// `text` redacted and cut to [`DETAIL_CAP`] characters.
fn cut(text: &str) -> String {
    bosun_core::publish::scrub(text)
        .chars()
        .take(DETAIL_CAP)
        .collect()
}

/// The word group of a failure's detail, which a notice may carry: `exit <code>`, `timeout` or
/// `cannot start`; never the probe's output.
pub(super) fn failure_of(detail: &str) -> String {
    if detail.starts_with("timeout") {
        "timeout".to_owned()
    } else if detail.starts_with("cannot start") {
        "cannot start".to_owned()
    } else {
        detail.split(':').next().unwrap_or(detail).to_owned()
    }
}

/// Checks the probed instances of `names` (plain resource names) for `item`, a card of the
/// loop's repository `repo` about to be claimed, in the global order: the first failing one,
/// or `None` when every one is healthy. An instance whose streak's probe is not due counts as
/// failing without running it; a streak a probe here ended publishes `resource_probe` with
/// `healthy: true`.
pub(super) async fn check(
    shared: &Arc<Shared>,
    repo: &Repo,
    identity: &Identity,
    names: &[String],
    item: &ItemId,
) -> Option<Failing> {
    let leases = &shared.deps.leases;
    let me = &repo.identity;
    for probed in leases.probed(names, &shared.options.repo, me) {
        let now = shared.deps.clock.wall_ms();
        if let Check::Failing(streak) = leases.probe_begin(&probed.instance, me, now) {
            return Some(Failing {
                resource: probed.instance,
                streak,
            });
        }
        let result = probe(shared, repo, identity, &probed).await;
        let now = shared.deps.clock.wall_ms();
        match leases.probe_record(&probed.instance, me, now, result) {
            Recorded::Failing(streak) => {
                return Some(Failing {
                    resource: probed.instance,
                    streak,
                });
            }
            Recorded::Healthy { ended: true } => {
                crate::log::log!(
                    "bosund: repository {}: resource {} passes its probe again",
                    shared.options.repo,
                    probed.instance
                );
                shared.publish(EventBody::ResourceProbe {
                    repo: shared.options.repo.clone(),
                    resource: probed.instance,
                    item: item.to_string(),
                    healthy: true,
                    detail: None,
                });
            }
            Recorded::Healthy { ended: false } => {}
        }
    }
    None
}

/// Tells that `item` waits on `failing` when its streak was not told yet (once per streak):
/// publishes `resource_probe` with `healthy: false` and queues the `resource_unhealthy`
/// notice.
pub(super) async fn tell(shared: &Arc<Shared>, repo: &Repo, item: &ItemId, failing: &Failing) {
    let told = shared
        .deps
        .leases
        .probe_tell(&failing.resource, &repo.identity);
    // Mutant (`mutant_loop_probe_told_every_time`): every failing check is told.
    if !told && !cfg!(mutant_loop_probe_told_every_time) {
        return;
    }
    let name = shared.options.repo.clone();
    let detail = &failing.streak.detail;
    crate::log::log!(
        "bosund: repository {name}: resource {} fails its probe ({detail}); {item} waits in Ready",
        failing.resource
    );
    shared.publish(EventBody::ResourceProbe {
        repo: name.clone(),
        resource: failing.resource.clone(),
        item: item.to_string(),
        healthy: false,
        detail: Some(detail.clone()),
    });
    let notice = bosun_store::NewNotice {
        key: format!(
            "resource_unhealthy:{}/{}",
            failing.resource, failing.streak.since_ms
        ),
        event: "resource_unhealthy".to_owned(),
        repo: name,
        item: Some(item.to_string()),
        facts: std::collections::BTreeMap::from([
            ("resource".to_owned(), failing.resource.clone()),
            ("failure".to_owned(), failure_of(detail)),
        ]),
        at_ms: now_ms(),
    };
    if let Err(e) = shared
        .deps
        .writer
        .write(vec![Write::NoticeQueued(notice)])
        .await
    {
        shared.trouble(format!(
            "the notice of the failing probe of {} cannot be queued: {e}",
            failing.resource
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_carries_the_failures_word_group_only() {
        assert_eq!(failure_of("exit 1: no device"), "exit 1");
        assert_eq!(failure_of("exit 127"), "exit 127");
        assert_eq!(failure_of("timeout after 10s: waiting"), "timeout");
        assert_eq!(failure_of("cannot start: not found: x"), "cannot start");
        let long = "x".repeat(500);
        assert_eq!(cut(&long).chars().count(), DETAIL_CAP);
    }
}
