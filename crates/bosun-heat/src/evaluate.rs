//! One evaluation of heat (#11, part D) for one repository: the window's rows of every
//! provider, the detectors run once over them ([`detect`], which groups by provider itself),
//! the landings of the stored proposals by the repository's done items ([`crate::link`]),
//! and the reconciliation of their findings with the stored proposals ([`reconcile`]). The
//! only part of heat's proposals that reads the store; it writes nothing: the daemon, the
//! store's single writer, writes the returned batch in one transaction.

use bosun_core::proposal::ProposalState;
use bosun_core::{State, canonical_path, style_of};
use bosun_store::{Reader, Write};

use crate::detect::{Insufficient, Thresholds, detect};
use crate::exact::Scope;
use crate::landing::{LinkItem, plan};
use crate::metrics::Inputs;
use crate::reconcile::Summary;

/// The window an evaluation looks at: the 14 days before it.
pub const WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// What one evaluation of one repository found and would write.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Evaluation {
    /// The batch to write, in one transaction.
    pub writes: Vec<Write>,
    pub summary: Summary,
    /// The detectors silent below the minimum sample, per provider.
    pub insufficient: Vec<Insufficient>,
}

/// Evaluates the repository at path key `repo` at `now_ms`, over `[now_ms - WINDOW_MS,
/// now_ms)`, with the default thresholds.
///
/// # Errors
///
/// A store error of one of the reads.
pub async fn evaluate(
    reader: &Reader,
    repo: &str,
    now_ms: i64,
) -> Result<Evaluation, bosun_store::Error> {
    let from_ms = now_ms - WINDOW_MS;
    let (table, usage) = reader.usage(from_ms, now_ms).await?;
    let calls = reader.tool_calls(from_ms, now_ms).await?;
    let histories = reader.item_histories(from_ms, now_ms).await?;
    let run_commits = reader.run_commits(from_ms, now_ms).await?;
    let inputs = Inputs {
        table: &table,
        usage: &usage,
        calls: &calls,
        histories: &histories,
        run_commits: &run_commits,
    };
    let scope = Scope {
        from_ms,
        to_ms: now_ms,
        repo: Some(repo),
    };
    let detection = detect(&inputs, scope, &Thresholds::default());
    let existing = reader.proposals(Some(repo)).await?;

    // The done items of the repository, read only when a proposal could land.
    let mut done = Vec::new();
    if existing.iter().any(|row| {
        row.seen_count >= 2 && matches!(row.state, ProposalState::Open | ProposalState::Published)
    }) {
        let style = style_of(repo);
        let key = canonical_path(repo, style);
        for stored in reader.items(false).await? {
            if !matches!(stored.item.state, State::Done(_))
                || canonical_path(&stored.repo, style) != key
            {
                continue;
            }
            let done_ms = reader
                .transitions(&stored.item.id)
                .await?
                .iter()
                .rev()
                .find(|t| t.to == "done")
                .map(|t| t.ts_ms);
            let body = reader.snapshot(&stored.item.id).await?.map(|s| s.body);
            let landing = reader.landing(&stored.item.id).await?;
            done.push((stored, done_ms, body, landing));
        }
    }
    let items: Vec<LinkItem<'_>> = done
        .iter()
        .map(|(item, done_ms, body, landing)| LinkItem {
            item,
            done_ms: *done_ms,
            body: body.as_deref(),
            landing: landing.as_ref(),
        })
        .collect();
    let reconciled = plan(&detection.findings, &existing, &items, now_ms);
    Ok(Evaluation {
        writes: reconciled.writes,
        summary: reconciled.summary,
        insufficient: detection.insufficient,
    })
}
