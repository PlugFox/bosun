//! Heat's proposals (#11, part D, "Heat: conclusions and Backlog proposals"; migration 25).
//!
//! A proposal is the stored record of a detector that fired: the snapshot of its metric over a
//! window, its target and remedy, and its lifecycle ([`bosun_core::proposal`]). Every write
//! addresses a proposal by its `proposal_key`; the integer id is the row's own.
//!
//! [`Write::ProposalOpened`] is where an incomplete finding stops: a proposal without a value, a
//! target or a window, with an empty metric or unit, an empty sample or oversized evidence is
//! refused by name ([`Error::Refused`]), and so is a second active proposal of one (detector,
//! repository, provider, subject) and a repeat inside the cooldown
//! ([`bosun_core::proposal::COOLDOWN_MS`]) after a dismissal or a follow-up that did not move the
//! metric. The schema refuses an incomplete row too. [`Write::ProposalSeen`] counts the
//! evaluations that found it again; [`Write::ProposalStep`] takes one arrow of the machine.
//!
//! [`Write::ProposalOpened`]: crate::Write::ProposalOpened
//! [`Write::ProposalSeen`]: crate::Write::ProposalSeen
//! [`Write::ProposalStep`]: crate::Write::ProposalStep

use bosun_core::proposal::{
    self, COOLDOWN_MS, FollowupOutcome, LandedHow, ProposalEvent, ProposalState, SubjectKind,
};
use bosun_core::{ItemId, canonical_path, style_of};
use sqlx::sqlite::SqliteConnection;

use crate::error::{Error, from_db, to_db};
use crate::read::Reader;
use crate::work::write::item_id;
use crate::writer::repo_at;

/// The largest `evidence_json` stored, in bytes.
const EVIDENCE_CAP: usize = 16 * 1024;

/// A finding to store as an open proposal ([`crate::Write::ProposalOpened`]). Integers are in the
/// metric's unit; shares in ppm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProposal {
    /// The proposal's key ([`bosun_core::proposal::proposal_key`]).
    pub key: String,
    /// The repository's path key (or a path it was configured at before).
    pub repo: String,
    pub provider: String,
    pub detector: String,
    pub detector_version: u32,
    pub remedy: String,
    pub subject_kind: SubjectKind,
    /// The subject: a repository-relative path for a file or a directory.
    pub subject: String,
    pub metric: String,
    pub unit: String,
    /// The metric is an estimate (from bytes per token), not an exact count.
    pub estimate: bool,
    /// The metric's value over the window; `None` refuses the proposal.
    pub baseline_value: Option<i64>,
    pub threshold: i64,
    /// The value the remedy aims at; `None` refuses the proposal.
    pub target_value: Option<i64>,
    /// The window `(from_ms, to_ms)`, `to > from`; `None` refuses the proposal.
    pub window: Option<(i64, i64)>,
    /// How many observations the value is computed from; 0 refuses the proposal.
    pub sample: u64,
    /// A guard metric and its baseline.
    pub guard: Option<(String, i64)>,
    pub impact_eqv_estimate: Option<u64>,
    /// The rest of the snapshot, a JSON object.
    pub params_json: String,
    /// The evidence, a JSON array, already redacted; over 16 KiB refuses the proposal.
    pub evidence_json: String,
    pub at_ms: i64,
}

/// A guard metric measured by the follow-up: its baseline and its value after the landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub metric: String,
    pub baseline: i64,
    pub after: i64,
}

/// One arrow of the proposal machine ([`crate::Write::ProposalStep`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalStep {
    /// Published as a card.
    Published {
        card_key: String,
        issue_number: Option<u64>,
    },
    /// Its remedy landed.
    Landed {
        item: Option<ItemId>,
        sha: Option<String>,
        how: LandedHow,
        landed_ms: i64,
        followup_due_ms: i64,
    },
    /// The follow-up measured it.
    Measured {
        after_value: Option<i64>,
        after_sample: u64,
        /// Written whole when `Some`: the guard's metric, baseline and after value.
        guard: Option<Guard>,
        /// Replaces the row's snapshot parameters when `Some`.
        params_json: Option<String>,
        outcome: FollowupOutcome,
    },
    /// The user dismissed it.
    Dismissed { reason: String },
    /// Not seen for two windows.
    Expired,
}

impl ProposalStep {
    fn event(&self) -> ProposalEvent {
        match self {
            Self::Published { .. } => ProposalEvent::Published,
            Self::Landed { .. } => ProposalEvent::Landed,
            Self::Measured { outcome, .. } => ProposalEvent::Measured(*outcome),
            Self::Dismissed { .. } => ProposalEvent::Dismissed,
            Self::Expired => ProposalEvent::Expired,
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::Published { .. } => "published",
            Self::Landed { .. } => "landed",
            Self::Measured { .. } => "measured",
            Self::Dismissed { .. } => "dismissed",
            Self::Expired => "expired",
        }
    }
}

/// A stored proposal, every column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRow {
    pub id: i64,
    pub key: String,
    pub dedup_key: String,
    /// The repository's current path key.
    pub repo: String,
    pub provider: String,
    pub detector: String,
    pub detector_version: u32,
    pub remedy: String,
    pub subject_kind: SubjectKind,
    pub subject: String,
    pub metric: String,
    pub unit: String,
    pub estimate: bool,
    pub baseline_value: i64,
    pub threshold: i64,
    pub target_value: i64,
    pub window_from_ms: i64,
    pub window_to_ms: i64,
    pub sample: u64,
    pub guard_metric: Option<String>,
    pub guard_baseline: Option<i64>,
    pub guard_after: Option<i64>,
    pub impact_eqv_estimate: Option<u64>,
    pub params_json: String,
    pub evidence_json: String,
    pub state: ProposalState,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    pub seen_count: u64,
    pub card_key: Option<String>,
    pub issue_number: Option<u64>,
    pub published_ms: Option<i64>,
    pub item: Option<ItemId>,
    pub landed_sha: Option<String>,
    pub landed_ms: Option<i64>,
    pub landed_how: Option<LandedHow>,
    pub followup_due_ms: Option<i64>,
    pub after_value: Option<i64>,
    pub after_sample: Option<u64>,
    pub measured_ms: Option<i64>,
    pub outcome: Option<FollowupOutcome>,
    pub dismissed_ms: Option<i64>,
    pub dismiss_reason: Option<String>,
}

/// The completeness of `new`: what is missing or wrong, by name.
fn incomplete(new: &NewProposal) -> Option<String> {
    let Some(_) = new.baseline_value else {
        return Some("no current value".to_owned());
    };
    let Some(_) = new.target_value else {
        return Some("no target".to_owned());
    };
    let Some((from, to)) = new.window else {
        return Some("no window".to_owned());
    };
    if to <= from {
        return Some(format!("an empty window ({from}..{to})"));
    }
    if new.metric.is_empty() {
        return Some("an empty metric".to_owned());
    }
    if new.unit.is_empty() {
        return Some("an empty unit".to_owned());
    }
    if new.sample == 0 {
        return Some("an empty sample".to_owned());
    }
    if new.evidence_json.len() > EVIDENCE_CAP {
        return Some(format!(
            "evidence of {} bytes, over {EVIDENCE_CAP}",
            new.evidence_json.len()
        ));
    }
    None
}

/// Stores `new` as an open proposal, or refuses it.
// Long: every refusal, then one line per column.
#[allow(clippy::too_many_lines)]
pub(crate) async fn open(conn: &mut SqliteConnection, new: &NewProposal) -> Result<(), Error> {
    // Mutant (`mutant_proposal_allow_incomplete`): the completeness check is skipped, so an
    // incomplete finding reaches the INSERT and the schema's refusal names nothing.
    if !cfg!(mutant_proposal_allow_incomplete)
        && let Some(why) = incomplete(new)
    {
        return Err(Error::Refused(format!(
            "proposal {} is not stored: {why}",
            new.key
        )));
    }
    let style = style_of(&new.repo);
    let repo_key = canonical_path(&new.repo, style);
    let Some(repo_id) = repo_at(conn, &repo_key).await? else {
        return Err(Error::Refused(format!(
            "proposal {} is not stored: unknown repository {}",
            new.key, new.repo
        )));
    };
    // Mutant (`mutant_proposal_no_dedup`): the proposal's own key stands for its dedup key, so
    // neither the check below nor the partial index sees a repeat.
    let dedup = if cfg!(mutant_proposal_no_dedup) {
        new.key.clone()
    } else {
        proposal::dedup_key(
            &new.detector,
            &new.provider,
            new.subject_kind,
            &new.subject,
            style,
        )
    };
    let active = sqlx::query_scalar!(
        "SELECT proposal_key FROM proposal
         WHERE repo_id = ? AND dedup_key = ? AND state IN ('open', 'published', 'landed')",
        repo_id,
        dedup,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(active) = active {
        return Err(Error::Refused(format!(
            "proposal {} is not stored: proposal {active} of the same subject is active",
            new.key
        )));
    }
    // Mutant (`mutant_proposal_no_cooldown`): a repeat right after a dismissal is stored.
    if !cfg!(mutant_proposal_no_cooldown)
        && let Some(ended) = cooldown_start(conn, repo_id, &dedup).await?
        && new.at_ms - ended < COOLDOWN_MS
    {
        return Err(Error::Refused(format!(
            "proposal {} is not stored: its subject is in cooldown until {} (ms)",
            new.key,
            ended + COOLDOWN_MS
        )));
    }

    let detector_version = i64::from(new.detector_version);
    let subject_kind = new.subject_kind.as_str();
    let exactness = if new.estimate { "estimate" } else { "exact" };
    let (window_from, window_to) = match new.window {
        Some((from, to)) => (Some(from), Some(to)),
        None => (None, None),
    };
    let sample = to_db(new.sample, "the sample")?;
    let (guard_metric, guard_baseline) = match &new.guard {
        Some((metric, baseline)) => (Some(metric.as_str()), Some(*baseline)),
        None => (None, None),
    };
    let impact = new
        .impact_eqv_estimate
        .map(|v| to_db(v, "the impact estimate"))
        .transpose()?;
    sqlx::query!(
        "INSERT INTO proposal (
             proposal_key, dedup_key, repo_id, provider, detector, detector_version, remedy,
             subject_kind, subject, metric, unit, exactness, baseline_value, threshold,
             target_value, window_from_ms, window_to_ms, sample, guard_metric, guard_baseline,
             impact_eqv_estimate, params_json, evidence_json, state, created_ms, last_seen_ms,
             seen_count)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open',
                 ?, ?, 1)",
        new.key,
        dedup,
        repo_id,
        new.provider,
        new.detector,
        detector_version,
        new.remedy,
        subject_kind,
        new.subject,
        new.metric,
        new.unit,
        exactness,
        new.baseline_value,
        new.threshold,
        new.target_value,
        window_from,
        window_to,
        sample,
        guard_metric,
        guard_baseline,
        impact,
        new.params_json,
        new.evidence_json,
        new.at_ms,
        new.at_ms,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// When the latest proposal of `dedup` in the repository that starts the cooldown ended.
async fn cooldown_start(
    conn: &mut SqliteConnection,
    repo_id: i64,
    dedup: &str,
) -> Result<Option<i64>, Error> {
    let ended = sqlx::query!(
        r#"SELECT proposal_key, state, outcome, dismissed_ms, measured_ms FROM proposal
           WHERE repo_id = ? AND dedup_key = ? AND state IN ('dismissed', 'measured')"#,
        repo_id,
        dedup,
    )
    .fetch_all(conn)
    .await?;
    let mut latest = None;
    for row in ended {
        let state = parse_state(&row.proposal_key, &row.state)?;
        let outcome = row
            .outcome
            .as_deref()
            .map(|o| parse_outcome(&row.proposal_key, o))
            .transpose()?;
        if !proposal::starts_cooldown(state, outcome) {
            continue;
        }
        let at = match state {
            ProposalState::Dismissed => row.dismissed_ms,
            _ => row.measured_ms,
        };
        latest = latest.max(at);
    }
    Ok(latest)
}

/// The state of proposal `key`.
async fn state_of(conn: &mut SqliteConnection, key: &str) -> Result<ProposalState, Error> {
    let state = sqlx::query_scalar!("SELECT state FROM proposal WHERE proposal_key = ?", key)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| Error::UnknownRow(format!("proposal {key}")))?;
    parse_state(key, &state)
}

/// Counts one more evaluation that found proposal `key`.
pub(crate) async fn seen(conn: &mut SqliteConnection, key: &str, at_ms: i64) -> Result<(), Error> {
    let state = state_of(conn, key).await?;
    let Some(next) = proposal::step(state, ProposalEvent::Seen) else {
        return Err(Error::Refused(format!(
            "proposal {key} is {}: it is not seen again",
            state.as_str()
        )));
    };
    let next = next.as_str();
    sqlx::query!(
        "UPDATE proposal SET state = ?, seen_count = seen_count + 1, last_seen_ms = ?
         WHERE proposal_key = ?",
        next,
        at_ms,
        key,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Takes `step` on proposal `key`.
// Long: one arm per step.
#[allow(clippy::too_many_lines)]
pub(crate) async fn step(
    conn: &mut SqliteConnection,
    key: &str,
    step: &ProposalStep,
    at_ms: i64,
) -> Result<(), Error> {
    let state = state_of(conn, key).await?;
    let Some(next) = proposal::step(state, step.event()) else {
        let why = if matches!(
            state,
            ProposalState::Measured | ProposalState::Dismissed | ProposalState::Expired
        ) {
            format!("a {} proposal takes no further step", state.as_str())
        } else {
            format!("a {} proposal cannot be {}", state.as_str(), step.verb())
        };
        return Err(Error::Refused(format!("proposal {key}: {why}")));
    };
    let next = next.as_str();
    match step {
        ProposalStep::Published {
            card_key,
            issue_number,
        } => {
            let issue = issue_number
                .map(|n| to_db(n, "the issue number"))
                .transpose()?;
            sqlx::query!(
                "UPDATE proposal SET state = ?, card_key = ?, issue_number = ?, published_ms = ?
                 WHERE proposal_key = ?",
                next,
                card_key,
                issue,
                at_ms,
                key,
            )
            .execute(conn)
            .await?;
        }
        ProposalStep::Landed {
            item,
            sha,
            how,
            landed_ms,
            followup_due_ms,
        } => {
            let item_row = match item {
                Some(item) => Some(item_id(conn, item).await?),
                None => None,
            };
            let how = how.as_str();
            sqlx::query!(
                "UPDATE proposal SET state = ?, item_id = ?, landed_sha = ?, landed_ms = ?,
                                     landed_how = ?, followup_due_ms = ?
                 WHERE proposal_key = ?",
                next,
                item_row,
                sha,
                landed_ms,
                how,
                followup_due_ms,
                key,
            )
            .execute(conn)
            .await?;
        }
        ProposalStep::Measured {
            after_value,
            after_sample,
            guard,
            params_json,
            outcome,
        } => {
            let after_sample = to_db(*after_sample, "the sample after")?;
            let outcome = outcome.as_str();
            sqlx::query!(
                "UPDATE proposal SET state = ?, after_value = ?, after_sample = ?,
                                     outcome = ?, measured_ms = ?,
                                     params_json = coalesce(?, params_json)
                 WHERE proposal_key = ?",
                next,
                after_value,
                after_sample,
                outcome,
                at_ms,
                params_json,
                key,
            )
            .execute(&mut *conn)
            .await?;
            if let Some(guard) = guard {
                sqlx::query!(
                    "UPDATE proposal SET guard_metric = ?, guard_baseline = ?, guard_after = ?
                     WHERE proposal_key = ?",
                    guard.metric,
                    guard.baseline,
                    guard.after,
                    key,
                )
                .execute(conn)
                .await?;
            }
        }
        ProposalStep::Dismissed { reason } => {
            sqlx::query!(
                "UPDATE proposal SET state = ?, dismissed_ms = ?, dismiss_reason = ?
                 WHERE proposal_key = ?",
                next,
                at_ms,
                reason,
                key,
            )
            .execute(conn)
            .await?;
        }
        ProposalStep::Expired => {
            sqlx::query!(
                "UPDATE proposal SET state = ? WHERE proposal_key = ?",
                next,
                key,
            )
            .execute(conn)
            .await?;
        }
    }
    Ok(())
}

fn parse_state(key: &str, name: &str) -> Result<ProposalState, Error> {
    ProposalState::parse(name)
        .ok_or_else(|| Error::Corrupt(format!("the state of proposal {key}: {name:?}")))
}

fn parse_outcome(key: &str, name: &str) -> Result<FollowupOutcome, Error> {
    FollowupOutcome::parse(name)
        .ok_or_else(|| Error::Corrupt(format!("the outcome of proposal {key}: {name:?}")))
}

impl Reader {
    /// The proposals of the repository at path key `repo` (or of every repository), by id; none
    /// for a path no repository has.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn proposals(&self, repo: Option<&str>) -> Result<Vec<ProposalRow>, Error> {
        let repo_id = match repo {
            Some(repo) => {
                let key = canonical_path(repo, style_of(repo));
                let mut conn = self.pool.acquire().await?;
                match repo_at(&mut conn, &key).await? {
                    Some(id) => Some(id),
                    None => return Ok(Vec::new()),
                }
            }
            None => None,
        };
        self.proposal_rows(repo_id, None).await
    }

    /// The proposal with key `key`.
    ///
    /// # Errors
    ///
    /// A database error, or a stored value that does not read back.
    pub async fn proposal(&self, key: &str) -> Result<Option<ProposalRow>, Error> {
        Ok(self
            .proposal_rows(None, Some(key))
            .await?
            .into_iter()
            .next())
    }

    // Long: one line per column.
    #[allow(clippy::too_many_lines)]
    async fn proposal_rows(
        &self,
        repo_id: Option<i64>,
        key: Option<&str>,
    ) -> Result<Vec<ProposalRow>, Error> {
        let rows = sqlx::query!(
            r#"SELECT p.id AS "id!: i64", p.proposal_key, p.dedup_key, r.path AS "repo!: String",
                      p.provider, p.detector, p.detector_version, p.remedy, p.subject_kind,
                      p.subject, p.metric, p.unit, p.exactness, p.baseline_value, p.threshold,
                      p.target_value, p.window_from_ms, p.window_to_ms, p.sample, p.guard_metric,
                      p.guard_baseline, p.guard_after, p.impact_eqv_estimate, p.params_json,
                      p.evidence_json, p.state, p.created_ms, p.last_seen_ms, p.seen_count,
                      p.card_key, p.issue_number, p.published_ms,
                      i.item_key AS "item_key?: String", p.landed_sha, p.landed_ms,
                      p.landed_how, p.followup_due_ms, p.after_value, p.after_sample,
                      p.measured_ms, p.outcome, p.dismissed_ms, p.dismiss_reason
               FROM proposal p JOIN repo r ON r.id = p.repo_id
               LEFT JOIN item i ON i.id = p.item_id
               WHERE (?1 IS NULL OR p.repo_id = ?1) AND (?2 IS NULL OR p.proposal_key = ?2)
               ORDER BY p.id"#,
            repo_id,
            key,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let key = row.proposal_key;
                let corrupt = |what: &str, value: &str| {
                    Error::Corrupt(format!("the {what} of proposal {key}: {value:?}"))
                };
                let subject_kind = SubjectKind::parse(&row.subject_kind)
                    .ok_or_else(|| corrupt("subject kind", &row.subject_kind))?;
                let estimate = match row.exactness.as_str() {
                    "exact" => false,
                    "estimate" => true,
                    other => return Err(corrupt("exactness", other)),
                };
                let landed_how = row
                    .landed_how
                    .as_deref()
                    .map(|h| LandedHow::parse(h).ok_or_else(|| corrupt("landing", h)))
                    .transpose()?;
                let detector_version = u32::try_from(row.detector_version)
                    .map_err(|_| corrupt("detector version", &row.detector_version.to_string()))?;
                Ok(ProposalRow {
                    state: parse_state(&key, &row.state)?,
                    outcome: row
                        .outcome
                        .as_deref()
                        .map(|o| parse_outcome(&key, o))
                        .transpose()?,
                    subject_kind,
                    estimate,
                    landed_how,
                    detector_version,
                    sample: from_db(row.sample, "the sample")?,
                    seen_count: from_db(row.seen_count, "the seen count")?,
                    impact_eqv_estimate: row
                        .impact_eqv_estimate
                        .map(|v| from_db(v, "the impact estimate"))
                        .transpose()?,
                    issue_number: row
                        .issue_number
                        .map(|v| from_db(v, "the issue number"))
                        .transpose()?,
                    after_sample: row
                        .after_sample
                        .map(|v| from_db(v, "the sample after"))
                        .transpose()?,
                    item: row.item_key.map(ItemId::new),
                    id: row.id,
                    dedup_key: row.dedup_key,
                    repo: row.repo,
                    provider: row.provider,
                    detector: row.detector,
                    remedy: row.remedy,
                    subject: row.subject,
                    metric: row.metric,
                    unit: row.unit,
                    baseline_value: row.baseline_value,
                    threshold: row.threshold,
                    target_value: row.target_value,
                    window_from_ms: row.window_from_ms,
                    window_to_ms: row.window_to_ms,
                    guard_metric: row.guard_metric,
                    guard_baseline: row.guard_baseline,
                    guard_after: row.guard_after,
                    params_json: row.params_json,
                    evidence_json: row.evidence_json,
                    created_ms: row.created_ms,
                    last_seen_ms: row.last_seen_ms,
                    card_key: row.card_key,
                    published_ms: row.published_ms,
                    landed_sha: row.landed_sha,
                    landed_ms: row.landed_ms,
                    followup_due_ms: row.followup_due_ms,
                    after_value: row.after_value,
                    measured_ms: row.measured_ms,
                    dismissed_ms: row.dismissed_ms,
                    dismiss_reason: row.dismiss_reason,
                    key,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
