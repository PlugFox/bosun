//! The outbox drainer (#5, #65): every board write of the loop is a row of the store's outbox,
//! written with its transition, and this task sends the rows of one repository, in order,
//! until the board acknowledges each.
//!
//! - **In order.** The due rows are sent oldest first; the first that fails ends the pass, so
//!   a later write never overtakes an earlier one. It is tried again after a backoff (the
//!   board's own `Retry-After` for a rate limit).
//! - **Rendered at send time.** A row holds what to write, never a body. The sticky report is
//!   rendered from the database when it is sent ([`report_facts`]), a Needs-you comment from
//!   its reason (a question from the agent's recorded call, lint findings, a budget's figure
//!   and what failed to make or set up a worktree, or the pull request closed unmerged, from the
//!   item's notes, #111, #8), and a column write sends the column of the item's state
//!   now. So a replay can never put an older report or column on the board than the database
//!   holds, and a row pending across a daemon restart renders as it would have before.
//! - **Figures.** The report's cost is the item's runs, exact (#72: every session bosun spawned
//!   for it, sub-agents included), and each attempt names the models its sessions ran. Its
//!   verdict output tails are published only on a repository known to be private (#5).
//! - **Keyed.** A comment carries its row's effect key as its hidden key, the sticky report
//!   the item's report key: a send whose response was lost is replayed without a duplicate.
//! - **Acknowledged by revision.** A report row that absorbed a newer write while its send
//!   was in flight stays pending (the store's [`bosun_store::Write::Acknowledge`]), so the
//!   newer state is sent too.
//! - **An outage never stops an item.** The drainer is its own task: the loop records its
//!   rows and goes on; `landing` does not wait for the board. The board catches up after.
//!
//! While the kill switch is thrown (`Control::board_writes_stopped`, #67) no row is sent;
//! they wait in the outbox and go out in order once it is released.
//!
//! A row whose item left the board (its card removed, its issue deleted or transferred) is
//! dropped. A revert of a foreign change ([`super::commands`]) is a column write keyed outside
//! the transitions' range; it also posts one explanatory comment per item state.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bosun_config::Config;
use bosun_core::publish::{
    self, AgentSummary, AttemptFacts, DeniedCall, DiffStat, Landing, LandingMode, ParkedFacts,
    ReportFacts, ReportOptions, VerdictFacts,
};
use bosun_core::{Column, ItemId, Outcome, ParkReason, Reason, State};
use bosun_queue::{Owned, QueueError};
use bosun_store::{AgentCallKind, OutboxOp, OutboxRow, Reader, Write};
use tokio::sync::{Notify, watch};

use super::landing::NOTE_LANDING;
use super::pulls::NOTE_PR_CLOSED;
use super::{Shared, blocking, lookup};

/// Effect indexes from here up are reverts of foreign changes, not effects of a transition
/// (whose indexes are small): [`revert_index`].
pub(crate) const REVERT_FLOOR: u32 = 1_000_000_000;

/// The longest wait before a failed row is tried again.
const MAX_BACKOFF: Duration = Duration::from_mins(1);
/// The wait after an error that pauses the repository (a lost scope, a renamed column).
const PAUSE_BACKOFF: Duration = Duration::from_mins(5);
/// A question or a comment body, in characters.
const COMMENT_CAP: usize = 8_000;

/// The kind of an item's note ([`Write::ItemNote`]) that holds its last lint findings.
pub(crate) const NOTE_LINT: &str = "lint";
/// The kind of an item's note that holds the figure of the budget cap that tripped.
pub(crate) const NOTE_BUDGET: &str = "budget";
/// The kind of an item's note that lists, as a JSON array of at most
/// [`super::git::UNCOMMITTED_CAP`] paths, what its last attempt left uncommitted in its
/// worktree (#8): not part of what was verified.
pub(crate) const NOTE_UNCOMMITTED: &str = "uncommitted";
/// The kind of an item's note that says why its worktree, or a verdict checkout of it, could
/// not be made or set up (#8): the `setup` command and how it ended, or the error. Its
/// Needs-you comment (`setup`) quotes it.
pub(crate) const NOTE_SETUP: &str = "setup";
/// The kind of an item's note that holds the store's refusal of a transition the daemon
/// computed from a stale view of the item (#132). Its Needs-you comment (`stale`) quotes it.
pub(crate) const NOTE_STALE: &str = "stale";

/// The longest note of kind [`NOTE_SETUP`], in characters.
const SETUP_NOTE_CAP: usize = 500;

/// Stores why `item`'s worktree could not be made or set up, as its [`NOTE_SETUP`] note:
/// `cause` (the command and how it ended, or the error), and the output's `tail` only when the
/// repository is not public, as the verdict tails of the sticky report (#111). Sanitised and
/// bounded; a note that cannot be written is reported.
pub(crate) async fn note_setup(shared: &super::Shared, item: &ItemId, cause: &str, tail: &str) {
    // Mutant (`mutant_loop_setup_comment_without_cause`): no note; the comment cannot say what
    // failed.
    if cfg!(mutant_loop_setup_comment_without_cause) {
        return;
    }
    let mut text = cause.trim().to_owned();
    if !tail.trim().is_empty() {
        let visibility = shared.deps.visibility.clone();
        let public = super::blocking(move || visibility.public()).await;
        if !public {
            let _ = write!(text, "; its output ends: {}", tail.trim());
        }
    }
    let body = publish::sanitise_inline(&publish::scrub(&text), SETUP_NOTE_CAP);
    let note = Write::ItemNote {
        item: item.clone(),
        kind: NOTE_SETUP.to_owned(),
        body,
        at_ms: super::now_ms(),
    };
    if let Err(e) = shared.deps.writer.write(vec![note]).await {
        shared.trouble(format!("the setup failure of {item} cannot be stored: {e}"));
    }
}

/// Stores the lint findings of `item` for its Needs-you comment (#111).
pub(crate) async fn note_findings(
    writer: &bosun_store::Writer,
    item: &ItemId,
    findings: &[String],
) -> Result<(), String> {
    let body = serde_json::to_string(findings).map_err(|e| e.to_string())?;
    writer
        .write(vec![Write::ItemNote {
            item: item.clone(),
            kind: NOTE_LINT.to_owned(),
            body,
            at_ms: super::now_ms(),
        }])
        .await
        .map_err(|e| e.to_string())
}

/// The budget cap that sent an item to Needs you: `item_eqv` (eqv) or `item_wall_clock`
/// (milliseconds of active time).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BudgetFigure {
    pub(crate) cap: String,
    pub(crate) used: u64,
    pub(crate) limit: u64,
}

impl BudgetFigure {
    /// The figure in words.
    pub(crate) fn text(&self) -> String {
        if self.cap == "item_wall_clock" {
            format!(
                "active wall clock {} of its {} cap",
                minutes(self.used),
                minutes(self.limit)
            )
        } else {
            format!(
                "{} eqv of its {} eqv cap",
                group(self.used),
                group(self.limit)
            )
        }
    }
}

/// Milliseconds as whole minutes.
fn minutes(ms: u64) -> String {
    format!("{} min", group(ms / 60_000))
}

/// `n` with thousands separated by commas.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What the loop tells the drainer, and the facts of the board it remembers for rendering.
#[derive(Debug, Default)]
pub(crate) struct Board {
    wake: Notify,
    /// Card titles from the last poll, by item.
    titles: Mutex<BTreeMap<ItemId, String>>,
}

impl Board {
    /// New rows were recorded: drain now.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Remembers the titles of the cards of a poll.
    pub(crate) fn note_titles(&self, titles: impl IntoIterator<Item = (ItemId, String)>) {
        let mut map = self.titles.lock().unwrap_or_else(PoisonError::into_inner);
        map.extend(titles);
    }

    fn title(&self, item: &ItemId) -> Option<String> {
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(item)
            .cloned()
    }
}

/// The index of a revert written at `now_ms`: distinct per millisecond (for weeks), above
/// every transition's.
pub(crate) fn revert_index(now_ms: i64) -> u32 {
    let span = i64::from(u32::MAX - REVERT_FLOOR);
    REVERT_FLOOR + u32::try_from(now_ms.rem_euclid(span)).unwrap_or(0)
}

/// The board's daemon-owned state of a machine column.
pub(crate) fn owned(column: Column) -> Owned {
    match column {
        Column::InProgress => Owned::InProgress,
        Column::Verify => Owned::Verify,
        Column::Done => Owned::Done,
        Column::NeedsYou => Owned::NeedsYou,
    }
}

/// The board's number of the card of `item` (`<repo>#<n>`).
pub(crate) fn card_of(item: &ItemId) -> Option<bosun_queue::ItemId> {
    let (_, n) = item.0.rsplit_once('#')?;
    n.parse().ok().map(bosun_queue::ItemId)
}

/// The (attempt, seq, index) of an effect key `<item>/a<attempt>/t<seq>/e<index>`.
pub(crate) fn key_parts(key: &str) -> Option<(u32, u32, u32)> {
    let mut parts = key.rsplitn(4, '/');
    let index = parts.next()?.strip_prefix('e')?.parse().ok()?;
    let seq = parts.next()?.strip_prefix('t')?.parse().ok()?;
    let attempt = parts.next()?.strip_prefix('a')?.parse().ok()?;
    Some((attempt, seq, index))
}

/// The pending rows of `repo` (due or not), from the store.
pub(crate) async fn pending(reader: &Reader, repo: &str) -> Result<Vec<OutboxRow>, String> {
    let prefix = format!("{repo}#");
    let rows = reader.outbox(i64::MAX).await.map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .filter(|r| r.item.0.starts_with(&prefix))
        .collect())
}

/// Runs the drainer of the loop's repository until aborted.
pub(crate) async fn run(
    shared: Arc<Shared>,
    board: Arc<Board>,
    config: watch::Receiver<Arc<Config>>,
) {
    let mut drainer = Drainer {
        shared,
        board,
        config,
        bodies: HashMap::new(),
    };
    loop {
        drainer.drain().await;
        let poll = drainer.shared.options.poll;
        tokio::select! {
            () = drainer.board.wake.notified() => {}
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// How one send ended.
enum Sent {
    /// The board has it (or the item left the board): acknowledge.
    Done,
    /// Try again later; the pass ends here.
    Retry(QueueError),
}

struct Drainer {
    shared: Arc<Shared>,
    board: Arc<Board>,
    config: watch::Receiver<Arc<Config>>,
    /// `mutant_outbox_stored_body`: the body first rendered for each row, kept for replays.
    bodies: HashMap<i64, String>,
}

impl Drainer {
    /// Sends the due rows of the repository in order; stops at the first that fails.
    async fn drain(&mut self) {
        let deps = &self.shared.deps;
        let now = deps.clock.wall_ms();
        let prefix = format!("{}#", self.shared.options.repo);
        let rows = match deps.reader.outbox(now).await {
            Ok(rows) => rows,
            Err(e) => {
                self.shared
                    .trouble(format!("the outbox cannot be read: {e}"));
                return;
            }
        };
        for row in rows.into_iter().filter(|r| r.item.0.starts_with(&prefix)) {
            // The kill switch (#67): nothing more is written to any board; the rows wait.
            if self.shared.deps.control.board_writes_stopped() {
                return;
            }
            let write = match self.send(&row).await {
                Ok(Sent::Done) => Write::Acknowledge {
                    id: row.id,
                    revision: row.revision,
                    at_ms: self.shared.deps.clock.wall_ms(),
                },
                Ok(Sent::Retry(e)) => {
                    let now = self.shared.deps.clock.wall_ms();
                    let wait = backoff(&e, row.tries, now);
                    self.shared.trouble(format!(
                        "the board write {} waits {} s: {e}",
                        row.effect_key,
                        wait.as_secs()
                    ));
                    let write = Write::SendFailed {
                        id: row.id,
                        error: e.to_string(),
                        retry_at_ms: now + i64::try_from(wait.as_millis()).unwrap_or(i64::MAX),
                    };
                    if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
                        self.shared
                            .trouble(format!("the outbox cannot be written: {e}"));
                    }
                    return;
                }
                Err(e) => {
                    self.shared.trouble(format!(
                        "the board write {} cannot be rendered: {e}",
                        row.effect_key
                    ));
                    return;
                }
            };
            if let Err(e) = self.shared.deps.writer.write(vec![write]).await {
                self.shared
                    .trouble(format!("the outbox cannot be written: {e}"));
                return;
            }
        }
    }

    /// Renders and sends one row. `Err` when the store cannot be read.
    async fn send(&mut self, row: &OutboxRow) -> Result<Sent, String> {
        let reader = &self.shared.deps.reader;
        let Some(card) = card_of(&row.item) else {
            return Ok(Sent::Done);
        };
        let stored = reader
            .item(&row.item)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("item {} is not recorded", row.item))?;
        let state = stored.item.state;
        let queue = self.shared.deps.queue.clone();
        let result = match row.op {
            OutboxOp::Project(_) => {
                // The column of the item's state now: a replay never moves the card back.
                let Some(owned) = state.column().map(owned) else {
                    return Ok(Sent::Done);
                };
                let revert = key_parts(&row.effect_key)
                    .filter(|(_, _, index)| *index >= REVERT_FLOOR)
                    .map(|(_, seq, _)| {
                        (
                            format!("revert:{}/t{seq}", row.item),
                            revert_body(state, owned.column()),
                        )
                    });
                blocking(move || {
                    queue.project(card, owned, None)?;
                    if let Some((key, body)) = revert {
                        queue.comment(card, &body, &key)?;
                    }
                    Ok(())
                })
                .await
            }
            OutboxOp::Comment(reason) => {
                let body = self.comment_body(row, reason).await?;
                let key = Self::key(&row.effect_key, row.tries);
                blocking(move || queue.comment(card, &body, &key)).await
            }
            OutboxOp::UpsertComment => {
                // The sticky report is created at claim (#5): an item never claimed (a lint
                // bounce, a claim lost or refused) gets none.
                if reader
                    .snapshot(&row.item)
                    .await
                    .map_err(|e| e.to_string())?
                    .is_none()
                {
                    return Ok(Sent::Done);
                }
                let body = if cfg!(mutant_outbox_stored_body) {
                    // Mutant: the body rendered for the first send is replayed as it was.
                    if let Some(body) = self.bodies.get(&row.id) {
                        body.clone()
                    } else {
                        let body = self.report(&row.item).await?;
                        self.bodies.insert(row.id, body.clone());
                        body
                    }
                } else {
                    self.report(&row.item).await?
                };
                let key = Self::key(&publish::report_key(&row.item), row.tries);
                blocking(move || queue.upsert_comment(card, &key, &body)).await
            }
        };
        Ok(match result {
            Ok(()) => Sent::Done,
            Err(e) if e.is_cancel() => {
                crate::log::log!(
                    "bosund: repository {}: {e}; the board write {} is dropped",
                    self.shared.options.repo,
                    row.effect_key
                );
                Sent::Done
            }
            Err(e @ QueueError::InvalidKey(_)) => {
                self.shared.trouble(format!(
                    "the board write {} is dropped: {e}",
                    row.effect_key
                ));
                Sent::Done
            }
            Err(e) => Sent::Retry(e),
        })
    }

    /// The hidden key a comment is sent under.
    fn key(key: &str, tries: u32) -> String {
        if cfg!(mutant_outbox_no_key) {
            // Mutant: no idempotence key; every send is a new comment, a send repeated after a
            // daemon restart (with the same `tries`) included.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            format!("{key}/{tries}-{nanos}")
        } else {
            key.to_owned()
        }
    }

    async fn report(&self, item: &ItemId) -> Result<String, String> {
        let title = self.board.title(item).unwrap_or_else(|| item.to_string());
        let config = self.config.borrow().clone();
        let path = lookup(&config, &self.shared.options.repo).map(|(repo, _)| repo.path.clone());
        let facts = report_facts(
            &self.shared.deps.reader,
            item,
            title,
            &self.shared.options.repo,
            path,
        )
        .await?;
        // The tails only on a repository known to be private (#5, #111).
        let public = if cfg!(mutant_report_tails_on_public) {
            // Mutant: the visibility is not asked; every repository gets its tails.
            false
        } else {
            let visibility = self.shared.deps.visibility.clone();
            blocking(move || visibility.public()).await
        };
        Ok(publish::render_report(
            &facts,
            ReportOptions::for_repository(public),
        ))
    }

    /// The note of `kind` on `item`, if one was stored.
    async fn note(&self, item: &ItemId, kind: &str) -> Result<Option<String>, String> {
        self.shared
            .deps
            .reader
            .item_note(item, kind)
            .await
            .map_err(|e| e.to_string())
    }

    /// What a Needs-you row of [`Reason::LandingRefused`] says when its transition was a pull
    /// request closed unmerged (#8): the item's [`NOTE_PR_CLOSED`] note written for that very
    /// transition. `None` for any other refusal.
    async fn closed_pull_request(&self, row: &OutboxRow) -> Result<Option<String>, String> {
        #[derive(serde::Deserialize)]
        struct Closed {
            number: u64,
            #[serde(default)]
            url: Option<String>,
            seq: u32,
        }
        let Some((_, seq, _)) = key_parts(&row.effect_key) else {
            return Ok(None);
        };
        let Some(closed) = self
            .note(&row.item, NOTE_PR_CLOSED)
            .await?
            .and_then(|text| serde_json::from_str::<Closed>(&text).ok())
            .filter(|c| c.seq == seq)
        else {
            return Ok(None);
        };
        let mut text = format!(
            "Pull request #{} was closed without being merged",
            closed.number
        );
        if let Some(url) = closed.url.filter(|u| !u.trim().is_empty()) {
            let _ = write!(text, " ({})", publish::sanitise_inline(&url, 200));
        }
        Ok(Some(text))
    }

    /// What a Needs-you row of [`Reason::LandingRefused`] says the forge answered: the error of
    /// the item's [`NOTE_LANDING`] note, written by the refusal of the row's attempt,
    /// scrubbed and capped. `None` for a refusal that left no such note.
    async fn landing_error(&self, row: &OutboxRow) -> Result<Option<String>, String> {
        #[derive(serde::Deserialize)]
        struct Refused {
            error: String,
            #[serde(default)]
            attempt: Option<u32>,
        }
        let attempt = key_parts(&row.effect_key).map(|(a, _, _)| a);
        Ok(self
            .note(&row.item, NOTE_LANDING)
            .await?
            .and_then(|text| serde_json::from_str::<Refused>(&text).ok())
            .filter(|r| r.attempt.is_some() && r.attempt == attempt)
            .map(|r| r.error)
            .filter(|e| !e.trim().is_empty())
            .map(|e| publish::sanitise_inline(&publish::scrub(&e), 500)))
    }

    /// The new comment of a Needs-you row: its reason; a question's own text.
    async fn comment_body(&self, row: &OutboxRow, reason: Reason) -> Result<String, String> {
        let item = &row.item;
        let mut body = format!("**bosun: {item} needs you.** ");
        match reason {
            Reason::Question => {
                let attempt = key_parts(&row.effect_key).map_or(0, |(a, _, _)| a);
                let calls = self
                    .shared
                    .deps
                    .reader
                    .agent_calls(item)
                    .await
                    .map_err(|e| e.to_string())?;
                let question = calls
                    .iter()
                    .rev()
                    .find(|c| {
                        c.attempt == attempt
                            && !c.refused
                            && c.kind == AgentCallKind::Question { blocking: true }
                    })
                    .map_or("", |c| c.body.as_str());
                let _ = write!(
                    body,
                    "The agent asked a blocking question in attempt {attempt} (its text, \
                     not checked by bosun):\n\n"
                );
                let text = publish::sanitise(&publish::scrub(question), COMMENT_CAP);
                for line in text.lines() {
                    body.push_str("> ");
                    body.push_str(line);
                    body.push('\n');
                }
                body.push_str(
                    "\nAnswer by editing the card, then move it to Ready for a fresh attempt.",
                );
            }
            Reason::Lint => {
                body.push_str("The card fails lint and was not claimed.");
                let findings: Vec<String> = self
                    .note(item, NOTE_LINT)
                    .await?
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default();
                if !findings.is_empty() {
                    body.push('\n');
                }
                for finding in findings {
                    body.push_str("\n- ");
                    body.push_str(&publish::sanitise_inline(&finding, 500));
                }
                body.push_str("\n\nFix the card, then move it to Ready.");
            }
            Reason::LandingRefused if let Some(closed) = self.closed_pull_request(row).await? => {
                body.push_str(&closed);
                body.push_str(
                    ". The item report above has the facts. Move the card to Ready to retry \
                     with a fresh attempt counter, or to Backlog to cancel.",
                );
            }
            other => {
                body.push_str(reason_text(other));
                if other == Reason::Budget
                    && let Some(figure) = budget_figure(&self.shared.deps.reader, item).await?
                {
                    let _ = write!(body, ": {}", figure.text());
                }
                // What failed (#8): the command and how it ended, or the error.
                if other == Reason::Setup
                    && let Some(cause) = self.note(item, NOTE_SETUP).await?
                    && !cause.trim().is_empty()
                {
                    let _ = write!(body, ": {}", publish::sanitise_inline(&cause, 500));
                }
                // What the store refused (#132).
                if other == Reason::Stale
                    && let Some(refusal) = self.note(item, NOTE_STALE).await?
                    && !refusal.trim().is_empty()
                {
                    let _ = write!(body, ": {}", publish::sanitise_inline(&refusal, 500));
                }
                if other == Reason::LandingRefused
                    && let Some(error) = self.landing_error(row).await?
                {
                    let _ = write!(body, ": {error}");
                }
                body.push_str(
                    ". The item report above has the facts. Move the card to Ready to retry \
                     with a fresh attempt counter, or to Backlog to cancel.",
                );
            }
        }
        body.push_str(&self.dependants(item));
        Ok(body)
    }

    /// The last paragraph of a Needs-you comment: the cards of the repository waiting on
    /// `item` (#5), from the loop's last plan, so one answer visibly unblocks a chain. Empty
    /// when none waits.
    fn dependants(&self, item: &ItemId) -> String {
        let waiting = self
            .shared
            .deps
            .blockers
            .dependants_of(&self.shared.options.repo, item);
        // Mutant (`mutant_outbox_comment_without_dependants`): the paragraph is left out.
        if waiting.is_empty() || cfg!(mutant_outbox_comment_without_dependants) {
            return String::new();
        }
        let names: Vec<String> = waiting.iter().map(ToString::to_string).collect();
        format!(
            "

Waiting on this item: {}. They are claimed once it is Done.",
            names.join(", ")
        )
    }
}

/// The wait before row `tries` (failed `tries` times before) is sent again after `e`.
fn backoff(e: &QueueError, tries: u32, now_ms: i64) -> Duration {
    match e {
        QueueError::RateLimited(limit) => limit.wait(u64::try_from(now_ms / 1000).unwrap_or(0)),
        e if e.pauses_repository() => PAUSE_BACKOFF,
        _ => Duration::from_secs(1u64 << tries.min(6)).min(MAX_BACKOFF),
    }
}

fn reason_text(reason: Reason) -> &'static str {
    match reason {
        Reason::Lint => "The card fails lint",
        Reason::Cycle => "The card is in a `blocked-by` cycle",
        Reason::Question => "The agent asked a blocking question",
        Reason::Auth => "The provider's authentication expired",
        Reason::Budget => "The item's budget was exceeded",
        Reason::ParkCap => "The item was parked and resumed too many times",
        Reason::ConfigChanged => {
            "The configuration or the permission policy changed during the item"
        }
        Reason::AttemptsExhausted => "The failed attempts reached the limit",
        Reason::Guard(_) => "A guard refused the attempt before its verdict",
        Reason::LandingRefused => "Landing was refused",
        Reason::Setup => "The item's worktree could not be made or set up",
        Reason::Handshake => "The session's MCP server did not start, again after one retry",
        Reason::Identity => "The identity checks before a session refused it",
        Reason::Stale => "The daemon's view of the item was older than its stored history",
    }
}

/// The comment that explains a reverted foreign change.
fn revert_body(state: State, column: bosun_queue::Column) -> String {
    format!(
        "**bosun:** this card is the daemon's while its item is `{}`; a change bosun did not \
         make was reverted to {column}. To cancel the item, move the card to Backlog; to start \
         it over, to Ready; to accept it as done, to Done; to restart its session, add the \
         `restart` label.",
        state.as_str()
    )
}

fn park_reason(name: &str) -> Option<ParkReason> {
    Some(match name {
        "limit" => ParkReason::Limit,
        "paused" => ParkReason::Paused,
        "killed" => ParkReason::Killed,
        "daily_cap" => ParkReason::DailyCap,
        _ => return None,
    })
}

/// The figure stored when the item's budget sent it to Needs you, if any.
async fn budget_figure(reader: &Reader, item: &ItemId) -> Result<Option<BudgetFigure>, String> {
    Ok(reader
        .item_note(item, NOTE_BUDGET)
        .await
        .map_err(|e| e.to_string())?
        .and_then(|text| serde_json::from_str(&text).ok()))
}

/// The facts of the item report of `item`, from the database (#5). `name` is the repository's
/// name in the configuration; `repo` is the checkout, for the landed commits and the diff stat.
#[allow(clippy::too_many_lines)]
pub(crate) async fn report_facts(
    reader: &Reader,
    item: &ItemId,
    title: String,
    name: &str,
    repo: Option<std::path::PathBuf>,
) -> Result<ReportFacts, String> {
    let e = |e: bosun_store::Error| e.to_string();
    let stored = reader
        .item(item)
        .await
        .map_err(e)?
        .ok_or_else(|| format!("item {item} is not recorded"))?;
    let snapshot = reader.snapshot(item).await.map_err(e)?;
    let attempts = reader.attempts(item).await.map_err(e)?;
    let runs = reader.runs(item).await.map_err(e)?;
    let verdicts = reader.verdict_runs(item).await.map_err(e)?;
    let calls = reader.agent_calls(item).await.map_err(e)?;
    let transitions = reader.transitions(item).await.map_err(e)?;
    let flaky = reader.flaky_tests(item).await.map_err(e)?;
    let denials = reader.denials(item).await.map_err(e)?;
    // The item's runs, exact, as `bosun report` bills them (#72).
    let row = bosun_heat::item_row(
        reader,
        item,
        bosun_api::RepoRef {
            name: Some(name.to_owned()),
            path: stored.repo.clone(),
        },
    )
    .await
    .map_err(e)?;
    let models_of = |number: u32| -> Option<String> {
        let attempt = row.as_ref()?.attempts.iter().find(|a| a.number == number)?;
        let mut models: Vec<&bosun_api::ModelRow> = attempt
            .models
            .iter()
            .filter(|m| m.sums.messages > 0)
            .collect();
        // The model that cost the most first.
        models.sort_by(|a, b| b.sums.eqv.total_cmp(&a.sums.eqv));
        let names: Vec<&str> = models.iter().map(|m| m.model.as_str()).collect();
        (!names.is_empty()).then(|| names.join(", "))
    };

    let verdict = snapshot
        .as_ref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s.contract_json).ok())
        .and_then(|c| c["verdict"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let model = snapshot
        .as_ref()
        .and_then(|s| crate::item::session_model(&s.contract_json));
    let attempts_facts = attempts
        .iter()
        .map(|row| {
            let number = row.attempt.number;
            let mut sessions: Vec<String> = Vec::new();
            for run in runs.iter().filter(|r| r.attempt == number) {
                if !sessions.contains(&run.session_key) {
                    sessions.push(run.session_key.clone());
                }
            }
            let done: Vec<_> = verdicts
                .iter()
                .filter(|v| v.attempt == number && v.result.is_some())
                .collect();
            let verdict = done.first().and_then(|first| {
                let last = done.last()?;
                Some(VerdictFacts {
                    first: first.result?,
                    rerun: done.get(1).and_then(|v| v.result),
                    duration_ms: done.iter().filter_map(|v| v.duration_ms).sum(),
                    line: last.verdict_line.clone().unwrap_or_default(),
                    tail: last.tail.clone().unwrap_or_default(),
                })
            });
            AttemptFacts {
                number,
                state: row.attempt.state,
                model: models_of(number),
                effort: row.effort.clone(),
                sessions,
                crash_retries: row.attempt.crash_retries,
                verdict,
                refusal: row.refusal.clone(),
                denials: denials
                    .iter()
                    .filter(|d| d.attempt == number)
                    .map(|d| DeniedCall {
                        tool: d.tool.clone(),
                        target: d.target.clone(),
                    })
                    .collect(),
            }
        })
        .collect();

    let mut parked = Vec::new();
    for (at, t) in transitions.iter().enumerate() {
        if t.to != "parked" {
            continue;
        }
        let (Some(reason), Some(next)) = (
            t.to_detail.as_deref().and_then(park_reason),
            transitions.get(at + 1),
        ) else {
            continue;
        };
        parked.push(ParkedFacts {
            reason,
            ms: u64::try_from(next.ts_ms - t.ts_ms).unwrap_or(0),
        });
    }

    let last_attempt = attempts.last().map(|a| a.attempt.number);
    let summary = match last_attempt {
        Some(attempt) => reader.summary(item, attempt).await.map_err(e)?.map(|call| {
            let (decisions, followups) = match &call.kind {
                AgentCallKind::Summary {
                    decisions,
                    followups,
                } => (decisions.clone(), followups.clone()),
                _ => (Vec::new(), Vec::new()),
            };
            AgentSummary {
                attempt,
                head: call.head_sha.clone().unwrap_or_default(),
                done: call.body.clone(),
                decisions,
                deviations: Vec::new(),
                questions: calls
                    .iter()
                    .filter(|c| {
                        c.attempt == attempt
                            && !c.refused
                            && c.kind == AgentCallKind::Question { blocking: false }
                    })
                    .map(|c| c.body.clone())
                    .collect(),
                followups,
            }
        }),
        None => None,
    };

    let base = snapshot.as_ref().and_then(|s| s.base_sha.clone());
    let head = runs.iter().rev().find_map(|r| r.head_sha.clone());
    let (commits, diff) = match (repo, base, head) {
        (Some(path), Some(base), Some(head)) if base != head => {
            blocking(move || {
                let git = bosun_git::Git::new(path);
                let commits = super::git::range(&git, &base, &head).unwrap_or_default();
                let diff = git
                    .run(&["diff", "--shortstat", &base, &head], None)
                    .ok()
                    .map(|out| shortstat(&out));
                (commits, diff)
            })
            .await
        }
        _ => (Vec::new(), None),
    };
    let landing = matches!(
        stored.item.state,
        State::Done(Outcome::Pass | Outcome::Flaky)
    )
    .then(|| Landing {
        mode: LandingMode::Local,
        branch: snapshot
            .as_ref()
            .and_then(|s| s.branch.clone())
            .map_or_else(String::new, |b| {
                b.strip_prefix("refs/heads/")
                    .map_or(b.clone(), str::to_owned)
            }),
        commits: commits.clone(),
        pull_request: None,
    });

    let detail = if stored.item.state == State::NeedsYou(Reason::Budget) {
        budget_figure(reader, item).await?.map(|f| f.text())
    } else {
        None
    };
    let runs = row.as_ref().map(|r| &r.runs);
    let bill = publish::Bill {
        eqv: runs.map_or(0.0, |r| r.eqv),
        input_tokens: runs.map_or(0, |r| r.input),
        output_tokens: runs.map_or(0, |r| r.output),
        cache_read_tokens: runs.map_or(0, |r| r.cache_read),
        cache_write_tokens: runs.map_or(0, |r| r.cache_write_5m + r.cache_write_1h),
        wall_clock_ms: row.as_ref().map_or(0, |r| r.wall_clock_ms),
        commits: u32::try_from(commits.len()).unwrap_or(u32::MAX),
    };
    Ok(ReportFacts {
        item: item.clone(),
        title,
        state: stored.item.state,
        detail,
        verdict,
        model,
        attempts: attempts_facts,
        flaky_tests: flaky.into_iter().map(|(_, name)| name).collect(),
        parked,
        landing,
        diff,
        summary,
        bill,
    })
}

/// `git diff --shortstat` as a diff stat.
fn shortstat(out: &str) -> DiffStat {
    let mut stat = DiffStat {
        files: 0,
        insertions: 0,
        deletions: 0,
        protected: Vec::new(),
    };
    for part in out.trim().split(',') {
        let mut words = part.split_whitespace();
        let Some(n) = words.next().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        match words.next() {
            Some(w) if w.starts_with("file") => stat.files = u32::try_from(n).unwrap_or(u32::MAX),
            Some(w) if w.starts_with("insertion") => stat.insertions = n,
            Some(w) if w.starts_with("deletion") => stat.deletions = n,
            _ => {}
        }
    }
    stat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_keys_and_card_numbers_parse() {
        assert_eq!(key_parts("demo#3/a2/t17/e1"), Some((2, 17, 1)));
        assert_eq!(key_parts("my/repo#3/a0/t1/e0"), Some((0, 1, 0)));
        assert_eq!(key_parts("report:demo#3"), None);
        assert_eq!(
            card_of(&ItemId::new("demo#12")),
            Some(bosun_queue::ItemId(12))
        );
        assert_eq!(card_of(&ItemId::new("demo")), None);
        assert!(revert_index(1_764_000_000_000) >= REVERT_FLOOR);
    }

    #[test]
    fn shortstat_reads_every_part() {
        let stat = shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n");
        assert_eq!((stat.files, stat.insertions, stat.deletions), (3, 10, 2));
        let stat = shortstat(" 1 file changed, 1 deletion(-)\n");
        assert_eq!((stat.files, stat.insertions, stat.deletions), (1, 0, 1));
    }
}
