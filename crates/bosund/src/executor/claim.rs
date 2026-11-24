//! The claim procedure (#5, #7, #62): the claim write and re-read, the snapshot, the static
//! lint, the base, the verdict on the base commit (D7), the hashes.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use bosun_config::{Config, Repo};
use bosun_core::ItemId;
use bosun_git::{Base, Git, GitConfig};
use bosun_queue::claim::other_claimants;
use bosun_queue::lint::verdict_shell;
use bosun_queue::{Card, Claim, Lint, LintContext, QueueError, ShellResolver, Snapshot};
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::Write;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{ClaimOutcome, Claimed, Shared, blocking, git, identity, lookup, now_ms};

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
/// its checkout; `blocked-by` targets must be on the board.
pub(crate) fn static_lint(card: &Card, repo: &Repo, on_board: &BTreeSet<u64>) -> Lint {
    let shell =
        verdict_shell(repo.verdict.shell.as_deref().map(Path::new)).unwrap_or_else(|| "sh".into());
    let resolver = ShellResolver::new(shell, &repo.path);
    let resources = BTreeSet::new();
    let exists = |id: bosun_queue::ItemId| on_board.contains(&id.0);
    bosun_queue::lint(
        card,
        &LintContext {
            resolver: &resolver,
            resources: &resources,
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
    mut kill: watch::Receiver<bool>,
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
    let lint = blocking(move || {
        // The re-read card's blockers were checked against the whole board before the claim;
        // here only the card itself is linted again.
        let mut card = lint_card;
        card.blocked_by.clear();
        let mut lint = static_lint(&card, &lint_repo, &on_board);
        lint.findings
            .retain(|f| !matches!(f.code, bosun_queue::Code::UnknownBlocker(_)));
        lint
    })
    .await;
    if !lint.claimable() {
        return ClaimOutcome::Lint(lint.findings.iter().map(ToString::to_string).collect());
    }

    let path = repo.path.clone();
    let base = match blocking(move || Base::capture(&Git::new(path))).await {
        Ok(base) => base,
        Err(e) => return ClaimOutcome::Failed(format!("the base cannot be captured: {e}")),
    };

    // D7: a verdict that already passes on the base commit is wrongly specified.
    if !cfg!(mutant_loop_skip_base_verdict) {
        let passes = base_verdict(shared, config, repo, &snapshot, &mut kill).await;
        let (path, at) = (repo.path.clone(), base.clone());
        if let Err(e) = blocking(move || git::restore_if_dirty(&path, &at)).await {
            return ClaimOutcome::Failed(format!("the checkout after the base verdict: {e}"));
        }
        match passes {
            Ok(true) => return ClaimOutcome::Lint(vec![PASSES_ON_BASE.to_owned()]),
            Ok(false) => {}
            Err(e) => return ClaimOutcome::Failed(e),
        }
    }

    // Git's own configuration (#59), after the verdict on the base commit: what the guards
    // compare with before every verdict of the item.
    let path = repo.path.clone();
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
    snapshot: &Snapshot,
    kill: &mut watch::Receiver<bool>,
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
    let spec = VerdictSpec {
        shell,
        command: snapshot.contract.verdict.clone(),
        env: env.iter().map(|(k, v)| (k.into(), v.to_owned())).collect(),
        cwd: repo.path.clone(),
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
