//! The claim procedure (#5, #7, #62): the claim write and re-read, the snapshot, the static
//! lint, the base, the verdict on the base commit (D7), the hashes.
//!
//! In worktree mode (#8, E19), after the static lint: the item's leases are taken
//! ([`super::lease_names_of`]; a claim that does not confirm releases them after its worktree
//! is closed, a confirmed one keeps them for its first session), then the daemon's clone is
//! made or checked, the target (`landing.target`, else the branch the user's checkout is on) is
//! fetched into it (E14), the user's checkout is captured for the guard of E4/E21, and the
//! item's worktree is opened and set up. The rest of the claim runs in the worktree, except the verdict on the base
//! commit: that runs in a throwaway checkout of the base commit of its own (kind `verdict`, set
//! up as every verdict checkout, closed right after the run), so what `setup` made in the item's
//! worktree is there for its first session and the worktree is never restored. A setup that
//! fails (of the item's worktree or of that checkout) is [`ClaimOutcome::Setup`] (Needs you),
//! with its cause stored as the item's `setup` note; one killed by a cancel or the kill switch
//! ends the claim as a killed base verdict does. A claim that ends any other way than confirmed
//! after the worktree was opened closes it again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bosun_config::{Config, Identity, Repo};
use bosun_core::ItemId;
use bosun_git::{Base, Git, GitConfig};
use bosun_queue::claim::other_claimants;
use bosun_queue::lint::verdict_shell;
use bosun_queue::{Card, Claim, Lint, LintContext, QueueError, ShellResolver, Snapshot};
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::{WorktreeKind, Write};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::checkout::UserCheckout;
use super::workspace::{Unopened, Workspace};
use super::{
    ClaimOutcome, Claimed, ItemWorktree, Shared, Signal, blocking, git, identity, lookup, now_ms,
};

/// What lint says when a card's verdict already passes on the base commit.
pub(crate) const PASSES_ON_BASE: &str =
    "error: the verdict already passes on the base commit; the card is wrongly specified";

/// Lowercase hex SHA-256 of `text`, shortened.
pub(crate) fn text_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..8].iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The repository's configuration hash and policy hash now.
pub(crate) fn hashes(config: &Config, repo: &str) -> Option<(String, String)> {
    let hash = config.repo_hash(repo)?;
    let policy = config.repos.get(repo)?.policy.hash();
    Some((hash, policy))
}

/// The static lint of `card` (#5): programs resolve through the repository's verdict shell in
/// its checkout; `blocked-by` targets must be on the board; `needs` names `resources`
/// ([`super::lint_resources`]).
pub(crate) fn static_lint(
    card: &Card,
    repo: &Repo,
    on_board: &BTreeSet<u64>,
    resources: &BTreeSet<String>,
) -> Lint {
    let shell =
        verdict_shell(repo.verdict.shell.as_deref().map(Path::new)).unwrap_or_else(|| "sh".into());
    let resolver = ShellResolver::new(shell, &repo.path);
    let exists = |id: bosun_queue::ItemId| on_board.contains(&id.0);
    bosun_queue::lint(
        card,
        &LintContext {
            resolver: &resolver,
            resources,
            item_exists: &exists,
        },
    )
}

/// Runs the claim of `card` for `item`; `key` is the claim effect's key, which keys the
/// snapshot.
#[allow(clippy::too_many_lines)]
pub(crate) async fn claim(
    shared: &Arc<Shared>,
    config: &Config,
    card: Card,
    item: &ItemId,
    key: &str,
    mut kill: watch::Receiver<Signal>,
) -> ClaimOutcome {
    let Some((repo, identity)) = lookup(config, &shared.options.repo) else {
        return ClaimOutcome::Failed("the repository is no longer configured".to_owned());
    };
    if shared.deps.control.board_writes_stopped() {
        // The kill switch: nothing further is written to any board (#67).
        return ClaimOutcome::Failed("the kill switch is thrown".to_owned());
    }
    let queue = shared.deps.queue.clone();
    let daemon = shared.options.daemon_id.clone();
    // Another daemon's claim marker already on the card (#5): the repository is refused before
    // anything is written to the board.
    let others = other_claimants(&card, &daemon);
    if !others.is_empty() {
        return ClaimOutcome::Board(QueueError::Contested {
            item: card.id,
            others,
        });
    }
    let card = match blocking(move || bosun_queue::claim(queue.as_ref(), &card, &daemon)).await {
        Ok(Claim::Claimed(card)) => card,
        Ok(Claim::Released(_)) => return ClaimOutcome::Lost,
        Err(e) => return ClaimOutcome::Board(e),
    };
    // A restart consumes its label (#5). A removal that fails leaves the label; the loop acts
    // on a label once, so it cannot restart again for it.
    if card
        .labels
        .iter()
        .any(|l| l == super::commands::RESTART_LABEL)
    {
        let queue = shared.deps.queue.clone();
        let id = card.id;
        let removed =
            blocking(move || queue.remove_label(id, super::commands::RESTART_LABEL)).await;
        if let Err(e) = removed {
            shared.trouble(format!(
                "the `restart` label of {item} cannot be removed: {e}"
            ));
        }
    }

    // Only text by logins on the author allowlist is inlined, never the daemon's (#2).
    let allowed: BTreeSet<String> = identity
        .author_allowlist
        .iter()
        .map(|a| a.to_ascii_lowercase())
        .collect();
    let snapshot = match Snapshot::take(&card, |c| {
        c.key.is_none() && allowed.contains(&c.author.to_ascii_lowercase())
    }) {
        Ok(snapshot) => snapshot,
        Err(errors) => {
            return ClaimOutcome::Lint(errors.iter().map(|e| format!("error: {e}")).collect());
        }
    };
    let on_board = BTreeSet::from([card.id.0]);
    let (lint_card, lint_repo) = (card.clone(), repo.clone());
    let resources = super::lint_resources(repo, &shared.deps.leases.names());
    let lint = blocking(move || {
        // The re-read card's blockers were checked against the whole board before the claim;
        // here only the card itself is linted again.
        let mut card = lint_card;
        card.blocked_by.clear();
        let mut lint = static_lint(&card, &lint_repo, &on_board, &resources);
        lint.findings
            .retain(|f| !matches!(f.code, bosun_queue::Code::UnknownBlocker(_)));
        lint
    })
    .await;
    if !lint.claimable() {
        return ClaimOutcome::Lint(lint.findings.iter().map(ToString::to_string).collect());
    }

    // Worktree mode (#8, E19): the item's leases, then its worktree off the daemon's clone;
    // else the checkout.
    let Some(workspace) = Workspace::of(repo) else {
        return confirm(shared, config, repo, snapshot, item, key, &mut kill, None).await;
    };
    // E1: the setup, the base verdict and every teardown of the claim run under the shared
    // build, so they run under the item's leases; a confirmed claim keeps them for its first
    // session (whose own request is then granted at once).
    // Mutant (`mutant_loop_lease_after_setup`): the claim takes nothing; the first session
    // takes the leases, after the setup and the base verdict ran.
    let names = if cfg!(mutant_loop_lease_after_setup) {
        Vec::new()
    } else {
        super::lease_names_of(&snapshot, repo)
    };
    match super::hold_names(shared, repo, names, item, &mut kill).await {
        Ok(()) => {}
        Err(None) => return ClaimOutcome::Failed("killed".to_owned()),
        Err(Some(e)) => {
            release(shared, item).await;
            return ClaimOutcome::Failed(format!("the item's leases: {e}"));
        }
    }
    let opened = match open_worktree(shared, identity, repo, &workspace, item, &mut kill).await {
        Ok(opened) => opened,
        Err(outcome) => {
            release(shared, item).await;
            return outcome;
        }
    };
    let close = Some((opened.workspace.clone(), opened.worktree.clone()));
    let opened = Some(opened);
    let mut outcome = confirm(shared, config, repo, snapshot, item, key, &mut kill, opened).await;
    // Confirmed after the kill was raised (a cancel past the base verdict): the loop let go of
    // the item and drops this outcome, so nobody else would close its worktree.
    let dropped = *kill.borrow() != Signal::Run
        && close.is_some()
        && matches!(outcome, ClaimOutcome::Confirmed(_));
    if dropped {
        outcome = ClaimOutcome::Lost;
    }
    if !matches!(outcome, ClaimOutcome::Confirmed(_))
        && let Some((workspace, worktree)) = close
    {
        let aside = dropped.then_some(item);
        workspace
            .close(shared, identity, &worktree, aside, Some(&mut kill))
            .await;
        // After the teardown, which ran under them.
        release(shared, item).await;
    }
    outcome
}

/// Releases the leases a claim of `item` took (#8); nothing happens when it took none.
async fn release(shared: &Shared, item: &ItemId) {
    shared
        .deps
        .leases
        .release(&crate::leases::Holder::Item(item.clone()))
        .await;
}

/// Worktree mode: the clone, the target, the user's checkout, the item's worktree and its
/// setup.
async fn open_worktree(
    shared: &Arc<Shared>,
    identity: &Identity,
    repo: &Repo,
    workspace: &Workspace,
    item: &ItemId,
    kill: &mut watch::Receiver<Signal>,
) -> Result<ItemWorktree, ClaimOutcome> {
    let (ws, user, target) = (
        workspace.clone(),
        repo.path.clone(),
        repo.landing.target.clone(),
    );
    // E28: in `pr` mode the item starts from the forge's target, fetched as the identity.
    // Mutant (`mutant_loop_pr_based_on_local_target`): it starts from the user's local target.
    let forge = if repo.landing.mode == bosun_config::LandingMode::Pr
        && !cfg!(mutant_loop_pr_based_on_local_target)
    {
        let bundle = super::identity::bundle(&repo.identity, identity, &repo.path)
            .map_err(ClaimOutcome::Failed)?;
        Some(bosun_identity::Env::tool(
            &bundle,
            &bosun_identity::inherited(),
        ))
    } else {
        None
    };
    let prepared = blocking(move || -> Result<(String, String, UserCheckout), String> {
        ws.ensure_clone()
            .map_err(|e| format!("the daemon's clone: {e}"))?;
        let target = if let Some(target) = target {
            target
        } else {
            let branch = Git::new(&user)
                .optional(&["symbolic-ref", "--quiet", "HEAD"])
                .map_err(|e| format!("the branch of the user's checkout: {e}"))?;
            let Some(branch) = branch else {
                return Err(
                    "the user's checkout is on a detached HEAD and `landing.target` is \
                            not configured: there is no branch to start the item from"
                        .to_owned(),
                );
            };
            branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&branch)
                .to_owned()
        };
        let at = if let Some(env) = &forge {
            super::landing::fetch_forge_target(&ws.clone_path(), &user, env, &target).map_err(
                |e| format!("the target {target} cannot be fetched from the forge: {e}"),
            )?;
            super::landing::forge_target_ref(&target)
        } else {
            ws.sync_target(&target).map_err(|e| {
                format!("the target {target} cannot be fetched into the daemon's clone: {e}")
            })?;
            format!("refs/heads/{target}")
        };
        // E4, E21: the user's checkout as it is now, compared before every verdict.
        let checkout = UserCheckout::capture(&user)
            .map_err(|e| format!("the user's checkout cannot be captured: {e}"))?;
        Ok((target, at, checkout))
    })
    .await;
    let (target, at, user) = prepared.map_err(ClaimOutcome::Failed)?;
    // What no item owns goes before the item gets its worktree: nothing else of the loop runs
    // now, and the item's own earlier worktree (a claim redone) is not its to keep.
    workspace.sweep(shared, identity, &[], false).await;
    let opened = workspace
        .open_with_setup(
            shared,
            identity,
            item,
            &item.0,
            &at,
            WorktreeKind::Item,
            Some(user.to_json()),
            kill,
        )
        .await;
    match opened {
        Ok(worktree) => Ok(ItemWorktree {
            workspace: workspace.clone(),
            worktree,
            user,
            target,
            base_ref: at,
        }),
        // Mutant (`mutant_loop_setup_failure_is_claim_failure`): a failing setup is a failed
        // claim, retried later, instead of Needs you.
        Err(Unopened::Setup(failed)) if cfg!(mutant_loop_setup_failure_is_claim_failure) => {
            Err(ClaimOutcome::Failed(failed.to_string()))
        }
        Err(Unopened::Setup(failed)) => Err(setup_failed(shared, item, &failed).await),
        Err(Unopened::Error(e)) => Err(ClaimOutcome::Failed(format!("the item's worktree: {e}"))),
        Err(Unopened::Killed) => Err(ClaimOutcome::Failed("killed".to_owned())),
    }
}

/// A `setup` of the claim failed: its cause is stored as the item's `setup` note, and the claim
/// is [`ClaimOutcome::Setup`] (Needs you).
async fn setup_failed(
    shared: &Shared,
    item: &ItemId,
    failed: &super::workspace::SetupFailed,
) -> ClaimOutcome {
    super::outbox::note_setup(shared, item, &failed.cause(), &failed.tail).await;
    ClaimOutcome::Setup(failed.to_string())
}

/// The rest of the claim, in `opened`'s worktree or else in the checkout: the base, the
/// verdict on the base commit, git's configuration, the hashes and the snapshot row.
#[allow(clippy::too_many_arguments)]
async fn confirm(
    shared: &Arc<Shared>,
    config: &Config,
    repo: &Repo,
    snapshot: Snapshot,
    item: &ItemId,
    key: &str,
    kill: &mut watch::Receiver<Signal>,
    opened: Option<ItemWorktree>,
) -> ClaimOutcome {
    let workdir: PathBuf = opened
        .as_ref()
        .map_or_else(|| repo.path.clone(), |o| o.worktree.path.clone());
    let path = workdir.clone();
    let base = match blocking(move || Base::capture(&Git::new(path))).await {
        Ok(base) => base,
        Err(e) => return ClaimOutcome::Failed(format!("the base cannot be captured: {e}")),
    };

    // D7: a verdict that already passes on the base commit is wrongly specified.
    if !cfg!(mutant_loop_skip_base_verdict) {
        // Worktree mode: in a checkout of the base commit of its own; the item's worktree, with
        // what its setup made, is not touched.
        // Mutant (`mutant_loop_base_verdict_in_item_worktree`): it runs in the item's worktree,
        // which is restored after it, what `setup` made with it.
        let own = opened
            .as_ref()
            .filter(|_| !cfg!(mutant_loop_base_verdict_in_item_worktree));
        let passes = if let Some(wt) = own {
            let Some((_, identity)) = lookup(config, &shared.options.repo) else {
                return ClaimOutcome::Failed("the repository is no longer configured".to_owned());
            };
            let name = format!("{item}-base");
            let checkout = match wt
                .workspace
                .checkout(shared, identity, item, &name, &base.head, kill)
                .await
            {
                Ok(checkout) => checkout,
                Err(Unopened::Setup(failed)) => return setup_failed(shared, item, &failed).await,
                Err(Unopened::Killed) => return ClaimOutcome::Failed("killed".to_owned()),
                Err(Unopened::Error(e)) => {
                    return ClaimOutcome::Failed(format!(
                        "the checkout of the base commit for its verdict: {e}"
                    ));
                }
            };
            let passes = base_verdict(shared, config, repo, checkout.path(), &snapshot, kill).await;
            checkout.close(shared, identity, Some(kill)).await;
            passes
        } else {
            let passes = base_verdict(shared, config, repo, &workdir, &snapshot, kill).await;
            let (path, at) = (workdir.clone(), base.clone());
            if let Err(e) = blocking(move || git::restore_if_dirty(&path, &at)).await {
                return ClaimOutcome::Failed(format!("the checkout after the base verdict: {e}"));
            }
            passes
        };
        match passes {
            Ok(true) => return ClaimOutcome::Lint(vec![PASSES_ON_BASE.to_owned()]),
            Ok(false) => {}
            Err(e) => return ClaimOutcome::Failed(e),
        }
    }

    // Git's own configuration (#59), after the verdict on the base commit: what the guards
    // compare with before every verdict of the item.
    let path = workdir.clone();
    let git_config = match blocking(move || GitConfig::capture(&Git::new(path))).await {
        Ok(git_config) => git_config,
        Err(e) => {
            return ClaimOutcome::Failed(format!("git's configuration cannot be recorded: {e}"));
        }
    };

    let Some((repo_hash, policy_hash)) = hashes(config, &shared.options.repo) else {
        return ClaimOutcome::Failed("the repository is no longer configured".to_owned());
    };
    let claimed = Claimed {
        snapshot,
        workdir,
        worktree: opened,
        base,
        git_config,
        repo_hash,
        policy_hash,
    };
    if let Err(e) = shared
        .deps
        .writer
        .write(vec![snapshot_row(
            item,
            key,
            &claimed,
            super::session::session_model(&claimed.snapshot.contract, repo).as_deref(),
        )])
        .await
    {
        return ClaimOutcome::Failed(format!("the snapshot cannot be stored: {e}"));
    }
    ClaimOutcome::Confirmed(Box::new(claimed))
}

/// The snapshot row of `claimed`, keyed by the claim effect's `key`; `model` is the model its
/// sessions ask for, recorded for the item report and `bosun item` (#133).
fn snapshot_row(item: &ItemId, key: &str, claimed: &Claimed, model: Option<&str>) -> Write {
    let snapshot = &claimed.snapshot;
    Write::Snapshot(bosun_store::Snapshot {
        item: item.clone(),
        key: key.to_owned(),
        taken_ms: now_ms(),
        hash: snapshot.hash.clone(),
        body: snapshot.body.clone(),
        contract_json: contract_json(&snapshot.contract, model),
        comments_json: serde_json::Value::from(
            snapshot
                .comments
                .iter()
                .map(|c| serde_json::json!({"author": c.author, "key": c.key, "body": c.body}))
                .collect::<Vec<_>>(),
        )
        .to_string(),
        config_hash: Some(claimed.repo_hash.clone()),
        policy_hash: Some(claimed.policy_hash.clone()),
        base_sha: Some(claimed.base.head.clone()),
        branch: claimed.base.branch.clone(),
        // The rest of the base, for crash recovery (#68).
        base_refs_json: Some(serde_json::json!(claimed.base.refs).to_string()),
        base_stash_json: Some(serde_json::json!(claimed.base.stash).to_string()),
    })
}

/// Runs the snapshot's verdict once on the base commit. `Ok(true)` when it passes.
async fn base_verdict(
    shared: &Arc<Shared>,
    config: &Config,
    repo: &Repo,
    workdir: &Path,
    snapshot: &Snapshot,
    kill: &mut watch::Receiver<Signal>,
) -> Result<bool, String> {
    let Some((_, identity)) = lookup(config, &shared.options.repo) else {
        return Err("the repository is no longer configured".to_owned());
    };
    let bundle = identity::bundle(&repo.identity, identity, &repo.path)?;
    let env = identity::agent_env(&bundle, &shared.options.work_dir)
        .map_err(|e| format!("the agent environment: {e}"))?;
    let Some(shell) = verdict_shell(repo.verdict.shell.as_deref().map(Path::new)) else {
        return Err("the verdict shell cannot be found".to_owned());
    };
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect();
    // The shared build directory (E1), and the worktree's files made newer than its last build.
    super::workspace::shared_build(repo, workdir, &mut vars, true).await?;
    let spec = VerdictSpec {
        shell,
        command: snapshot.contract.verdict.clone(),
        env: vars,
        cwd: workdir.to_owned(),
        timeout: snapshot
            .contract
            .verdict_timeout
            .unwrap_or(repo.verdict.timeout),
        limits: Limits::default(),
    };
    tokio::select! {
        run = bosun_runner::verdict::run(shared.deps.spawner.as_ref(), &spec) => {
            let run = run.map_err(|e| format!("the verdict on the base commit cannot run: {e}"))?;
            Ok(run.outcome == Outcome::Pass)
        }
        () = super::verdict::killed(kill) => Err("killed".to_owned()),
    }
}

/// The parsed contract as the snapshot row stores it, with `session_model`: the model its
/// sessions ask for, the card's `model` or else the repository's default (#133). A change of
/// the repository's default after the claim changes its configuration hash, so no session of
/// the claim runs on another model.
fn contract_json(contract: &bosun_queue::Contract, session_model: Option<&str>) -> String {
    serde_json::json!({
        "verdict": contract.verdict,
        "verdict_timeout_s": contract.verdict_timeout.map(|d| d.as_secs()),
        "needs": contract.needs,
        "blocked_by": contract.blocked_by.iter().map(|b| b.0).collect::<Vec<_>>(),
        "budget": {
            "eqv": contract.budget.eqv,
            "wall_s": contract.budget.wall.map(|d| d.as_secs()),
        },
        "runner": contract.runner,
        "model": contract.model,
        "session_model": session_model,
        "allow_protected": contract.allow_protected,
    })
    .to_string()
}
