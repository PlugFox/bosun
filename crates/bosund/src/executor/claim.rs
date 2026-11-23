//! The claim procedure (#5, #7, #62): the claim write and re-read, the snapshot, the static
//! lint, the base, the verdict on the base commit (D7), the hashes.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use bosun_config::{Config, Repo};
use bosun_core::ItemId;
use bosun_git::{Base, Git};
use bosun_queue::lint::verdict_shell;
use bosun_queue::{Card, Claim, Lint, LintContext, ShellResolver, Snapshot};
use bosun_runner::verdict::{Limits, Outcome, VerdictSpec};
use bosun_store::Write;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{ClaimOutcome, Claimed, Shared, blocking, git, identity, lookup, now_ms};

/// What lint says when a card's verdict already passes on the base commit.
pub(crate) const PASSES_ON_BASE: &str =
    "error: the verdict already passes on the base commit; the card is mis-specified";

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
    let queue = shared.deps.queue.clone();
    let daemon = shared.options.daemon_id.clone();
    let card = match blocking(move || bosun_queue::claim(queue.as_ref(), &card, &daemon)).await {
        Ok(Claim::Claimed(card)) => card,
        Ok(Claim::Released(_)) => return ClaimOutcome::Lost,
        Err(e) => return ClaimOutcome::Failed(e.to_string()),
    };

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

    // D7: a verdict that already passes on the base commit is mis-specified.
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

    let Some((repo_hash, policy_hash)) = hashes(config, &shared.options.repo) else {
        return ClaimOutcome::Failed("the repository is no longer configured".to_owned());
    };
    let write = Write::Snapshot(bosun_store::Snapshot {
        item: item.clone(),
        key: key.to_owned(),
        taken_ms: now_ms(),
        hash: snapshot.hash.clone(),
        body: snapshot.body.clone(),
        contract_json: contract_json(&snapshot.contract),
        comments_json: serde_json::Value::from(
            snapshot
                .comments
                .iter()
                .map(|c| serde_json::json!({"author": c.author, "key": c.key, "body": c.body}))
                .collect::<Vec<_>>(),
        )
        .to_string(),
        config_hash: Some(repo_hash.clone()),
        policy_hash: Some(policy_hash.clone()),
        base_sha: Some(base.head.clone()),
        branch: base.branch.clone(),
    });
    if let Err(e) = shared.deps.writer.write(vec![write]).await {
        return ClaimOutcome::Failed(format!("the snapshot cannot be stored: {e}"));
    }
    ClaimOutcome::Confirmed(Box::new(Claimed {
        snapshot,
        base,
        repo_hash,
        policy_hash,
    }))
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

/// The parsed contract as the snapshot row stores it.
fn contract_json(contract: &bosun_queue::Contract) -> String {
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
        "allow_protected": contract.allow_protected,
    })
    .to_string()
}
