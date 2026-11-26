//! The loop's commands (#4, #70): `status`, `queue`, `item`, `lint` and `session tail`.
//!
//! - `status` and `queue` ask each identity (`--identity`, else every identity of the
//!   configuration) and show each in a section of its own (`status.overview`, `queue.get`).
//! - `item <id>`, `lint <card>` and `session tail <id>` name one item of one repository; the
//!   identity is the one that owns the repository (`--identity`, else the configuration's).
//! - `lint` goes through the daemon (`queue.lint`): the board, the checkout and the verdict
//!   shell are the daemon's, and a client links no queue backend (#12). A card that fails
//!   exits 1, with its findings on stdout.
//! - `session tail` subscribes to the identity's events with the live tail of the item
//!   (`events.subscribe`), then asks which session to follow (`session.get`), so no line
//!   printed after the answer is missed; it ends when that session ends.

use std::fmt::Write as _;
use std::io::Write as _;

use bosun_api::rpc::code;
use bosun_api::{
    EventBody, EventFilter, IdentityParams, ItemDetail, ItemParams, LintParams, LintResult,
    Overview, QueueParams, QueueView, SessionInfo, SessionParams, Tail, WaitingFor, method,
};
use bosun_client::{Client, Delivery, Error, Subscription};
use clap::{Args, Subcommand};

use crate::control::endpoint;
use crate::time;
use crate::{Failure, Format, Global, Section, connect, exit, identities, output};

#[derive(Debug, Args)]
pub(crate) struct QueueArgs {
    /// Only this repository (its name in the configuration); every repository otherwise.
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ItemArgs {
    /// The item: `<repo>#<card>`, as `bosun queue` lists it.
    #[arg(value_name = "ITEM")]
    item: String,
}

#[derive(Debug, Args)]
pub(crate) struct LintArgs {
    /// The card: `<repo>#<card>`, or its number with --repo.
    #[arg(value_name = "CARD")]
    card: String,
    /// The repository of a card given by number.
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// Follows a running session's output until it ends.
    #[command(after_help = crate::EXIT_CODES)]
    Tail(TailArgs),
}

#[derive(Debug, Args)]
pub(crate) struct TailArgs {
    /// An item (`<repo>#<card>`: its latest session) or a provider session id.
    #[arg(value_name = "ID")]
    id: String,
}

/// The repository and card of an item id `<repo>#<card>`.
pub(crate) fn split_item(item: &str) -> Option<(&str, u64)> {
    let (repo, card) = item.rsplit_once('#')?;
    let card = card.parse().ok()?;
    (!repo.is_empty()).then_some((repo, card))
}

fn usage_item(item: &str) -> Failure {
    Failure::new(
        exit::USAGE,
        format!("{item:?} is not an item: an item is `<repo>#<card>`, as `bosun queue` lists it"),
    )
}

/// The one identity that owns the repository of `item`.
fn owner(global: &Global, repo: &str) -> Result<String, Failure> {
    identities(global, Some(repo))?
        .into_iter()
        .next()
        .ok_or_else(|| Failure::new(exit::CONFIG, "the configuration names no identity"))
}

fn json<T: serde::Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

fn ago(now_ms: i64, then_ms: i64) -> String {
    let s = (now_ms - then_ms).max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, s % 3600 / 60),
    }
}

/// `status`.
pub(crate) async fn status(global: &Global) -> Result<String, Failure> {
    let names = identities(global, None)?;
    let mut client = connect(global).await?;
    let mut sections = Vec::new();
    for name in names {
        let overview = client
            .call::<method::StatusOverview>(&IdentityParams { identity: name })
            .await?;
        sections.push(Section::of(
            &overview.identity.clone(),
            &overview,
            render_status,
        )?);
    }
    Ok(output(global, sections))
}

fn render_status(o: &Overview, out: &mut String) {
    out.push_str(&crate::render::header(&o.identity));
    let _ = writeln!(
        out,
        "daemon {} since {}",
        o.daemon_version,
        time::format(o.started_ms)
    );
    if o.killed {
        let _ = writeln!(out, "the kill switch is thrown");
    }
    if o.paused {
        let _ = writeln!(out, "every repository is paused by hand");
    }
    if let Some(until) = o.daily_cap_until_ms {
        let _ = writeln!(
            out,
            "the daily cap holds the queue until {}",
            time::format(until)
        );
    }
    for repo in &o.repos {
        let hold = repo
            .hold
            .as_deref()
            .map_or_else(String::new, |h| format!(", hold {h}"));
        // Why the fast-death breaker paused it (#7).
        let tripped = repo
            .tripped
            .as_deref()
            .map_or_else(String::new, |why| format!(": {why}"));
        let _ = writeln!(out, "repo {}: {}{hold}{tripped}", repo.repo, repo.state);
        if let Some(item) = &repo.current {
            let detail = remark(item.detail.as_deref(), &item.waiting);
            let _ = writeln!(
                out,
                "  {} {}{detail}, attempt {}, {} in state",
                item.item,
                item.state,
                item.attempt,
                ago(o.now_ms, item.since_ms)
            );
        }
        if let Some(v) = &repo.last_verdict {
            let _ = writeln!(
                out,
                "  last verdict: {} attempt {} run {}: {}, {}",
                v.item,
                v.attempt,
                v.run,
                v.result,
                time::format(v.ended_ms)
            );
        }
    }
}

/// `queue [--repo]`.
pub(crate) async fn queue(global: &Global, args: &QueueArgs) -> Result<String, Failure> {
    let names = identities(global, args.repo.as_deref())?;
    let mut client = connect(global).await?;
    let mut sections = Vec::new();
    for name in names {
        let view = client
            .call::<method::QueueGet>(&QueueParams {
                identity: name,
                repo: args.repo.clone(),
            })
            .await?;
        sections.push(Section::of(&view.identity.clone(), &view, render_queue)?);
    }
    Ok(output(global, sections))
}

fn render_queue(view: &QueueView, out: &mut String) {
    out.push_str(&crate::render::header(&view.identity));
    for repo in &view.repos {
        let hash = repo
            .hash
            .as_deref()
            .map_or_else(String::new, |h| format!(" (board {h})"));
        let _ = writeln!(out, "repo {}: {}{hash}", repo.repo, repo.queue);
        if let Some(error) = &repo.error {
            let _ = writeln!(out, "  cannot be read: {error}");
        }
        for column in repo.columns.iter().filter(|c| !c.cards.is_empty()) {
            let _ = writeln!(out, "  {}", column.column);
            for card in &column.cards {
                let mut marks = Vec::new();
                if let Some(state) = &card.state {
                    marks.push(state.clone());
                }
                if card.edit_pending {
                    marks.push("edit pending".to_owned());
                }
                if card.draft {
                    marks.push("draft".to_owned());
                }
                let marks = if marks.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", marks.join(", "))
                };
                let _ = writeln!(out, "    {} {}{marks}", card.item, card.title);
                for finding in &card.lint {
                    let _ = writeln!(out, "      {}: {}", finding.severity, finding.message);
                }
            }
        }
    }
}

/// `item <id>`: one document, of the identity that owns the item.
pub(crate) async fn item(global: &Global, args: &ItemArgs) -> Result<String, Failure> {
    let (repo, _) = split_item(&args.item).ok_or_else(|| usage_item(&args.item))?;
    let identity = owner(global, repo)?;
    let mut client = connect(global).await?;
    let detail = client
        .call::<method::ItemGet>(&ItemParams {
            identity,
            item: args.item.clone(),
        })
        .await?;
    if global.format == Format::Json {
        return Ok(json(&detail));
    }
    let mut out = String::new();
    render_item(&detail, &mut out);
    Ok(out)
}

/// What an item waits for (#8): `waiting for rig: held by item demo#3`, each full resource
/// with its holders (`busy` for another identity's), `; ` between them.
fn waiting_for(waiting: &[WaitingFor]) -> Option<String> {
    if waiting.is_empty() {
        return None;
    }
    let each: Vec<String> = waiting
        .iter()
        .map(|w| {
            if w.holders.is_empty() {
                format!("{}: queued", w.resource)
            } else {
                format!("{}: held by {}", w.resource, w.holders.join(", "))
            }
        })
        .collect();
    Some(format!("waiting for {}", each.join("; ")))
}

/// The parenthesised remark after an item's state: its detail and what it waits for, if any.
fn remark(detail: Option<&str>, waiting: &[WaitingFor]) -> String {
    let parts: Vec<String> = detail
        .map(ToOwned::to_owned)
        .into_iter()
        .chain(waiting_for(waiting))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn render_item(d: &ItemDetail, out: &mut String) {
    let detail = remark(d.detail.as_deref(), &d.waiting);
    let _ = writeln!(
        out,
        "{} in {}: {}{detail}, attempt {}, {} failed",
        d.item,
        d.repo.name.as_deref().unwrap_or(&d.repo.path),
        d.state,
        d.attempt,
        d.failed
    );
    if let Some(s) = &d.snapshot {
        let model = s
            .model
            .as_deref()
            .map_or_else(String::new, |m| format!(", model {m}"));
        let _ = writeln!(
            out,
            "snapshot {} taken {}{model}",
            s.hash,
            time::format(s.taken_ms)
        );
    }
    for a in &d.attempts {
        let _ = writeln!(out, "attempt {}: {}", a.number, a.state);
        for s in &a.sessions {
            let exit = s.exit.as_deref().unwrap_or("running");
            let _ = writeln!(
                out,
                "  session {} ({}, spawn {}): {exit}",
                s.session, s.mode, s.spawn
            );
        }
        for v in &a.verdicts {
            let result = v.result.as_deref().unwrap_or("running");
            let _ = writeln!(out, "  verdict run {}: {result}", v.run);
        }
        for reason in &a.refusal {
            // The daemon's words; a quoted path or commit message stays on its line.
            let reason: String = reason
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let _ = writeln!(out, "  refused before its verdict: {reason}");
        }
    }
    for c in &d.calls {
        let refused = if c.refused { " (refused)" } else { "" };
        let _ = writeln!(
            out,
            "agent {} in attempt {}{refused}: {}",
            c.kind, c.attempt, c.body
        );
    }
    for t in &d.transitions {
        let detail = t
            .detail
            .as_deref()
            .map_or_else(String::new, |x| format!(" ({x})"));
        let _ = writeln!(
            out,
            "  {} {} -> {}{detail} on {}",
            time::format(t.ts_ms),
            t.from,
            t.to,
            t.cause
        );
    }
}

/// `lint <card>`: exits 1 with the findings when the card is not claimable.
pub(crate) async fn lint(global: &Global, args: &LintArgs) -> Result<String, Failure> {
    let (repo, card) = match (&args.repo, split_item(&args.card)) {
        (None, Some((repo, card))) => (repo.to_owned(), card),
        (Some(repo), None) => {
            let card = args
                .card
                .trim_start_matches('#')
                .parse()
                .map_err(|_| usage_item(&args.card))?;
            (repo.clone(), card)
        }
        (Some(_), Some(_)) => {
            return Err(Failure::new(
                exit::USAGE,
                "--repo is for a card given by number; `<repo>#<card>` names its repository",
            ));
        }
        (None, None) => return Err(usage_item(&args.card)),
    };
    let identity = owner(global, &repo)?;
    let mut client = connect(global).await?;
    let result = client
        .call::<method::QueueLint>(&LintParams {
            identity,
            repo,
            card,
        })
        .await?;
    let text = match global.format {
        Format::Json => json(&result),
        _ => render_lint(&result),
    };
    if result.claimable {
        Ok(text)
    } else {
        Err(Failure::new(
            exit::FAILED,
            format!("{} fails lint: it is not claimable", result.item),
        )
        .with_stdout(text))
    }
}

fn render_lint(result: &LintResult) -> String {
    let mut out = String::new();
    let verdict = if result.claimable {
        "claimable"
    } else {
        "not claimable"
    };
    let _ = writeln!(out, "{}: {verdict}", result.item);
    for f in &result.findings {
        let _ = writeln!(out, "  {}: {}", f.severity, f.message);
    }
    out
}

/// Writes one line to stdout now: a tail is read while it runs.
fn print(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// `session tail <id>`: prints each output line of the session as it comes, and returns when
/// the session ends. Text prints the lines; JSON prints each event (`events.event`'s `Event`)
/// as one line.
pub(crate) async fn tail(global: &Global, args: &TailArgs) -> Result<String, Failure> {
    let by_item = split_item(&args.id);
    let names = match by_item {
        Some((repo, _)) => vec![owner(global, repo)?],
        None => identities(global, None)?,
    };
    let endpoint = endpoint(global)?;
    let mut client = Client::connect(&endpoint).await?;
    let ask = |identity: &str| SessionParams {
        identity: identity.to_owned(),
        session: by_item.is_none().then(|| args.id.clone()),
        item: by_item.is_some().then(|| args.id.clone()),
    };
    // The identity that has the session.
    let mut found = None;
    for (n, name) in names.iter().enumerate() {
        match client.call::<method::SessionGet>(&ask(name)).await {
            Ok(info) => {
                found = Some((name.clone(), info));
                break;
            }
            Err(Error::Rpc(e)) if e.code == code::NOT_FOUND && n + 1 < names.len() => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some((identity, first)) = found else {
        return Err(Failure::new(
            exit::FAILED,
            format!("no identity has a session {:?}", args.id),
        ));
    };
    let filter = EventFilter {
        kinds: Some(vec![
            "session_output".to_owned(),
            "session_ended".to_owned(),
        ]),
        tail: Some(Tail {
            item: Some(first.item.clone()),
        }),
    };
    let mut events = Subscription::open(endpoint, &identity, filter).await?;
    // Asked again once subscribed: a line printed after this answer reaches the stream.
    let mut session = client.call::<method::SessionGet>(&ask(&identity)).await?;
    if !session.running {
        ended(global, &session);
        return Ok(String::new());
    }
    loop {
        match events.next().await? {
            Delivery::Event(event) => match &event.body {
                EventBody::SessionOutput {
                    attempt,
                    spawn,
                    line,
                    ..
                } if (*attempt, *spawn) == (session.attempt, session.spawn) => {
                    match global.format {
                        Format::Json => print(&serde_json::to_string(&event).unwrap_or_default()),
                        _ => print(line),
                    }
                }
                EventBody::SessionEnded { attempt, spawn, .. }
                    if (*attempt, *spawn) == (session.attempt, session.spawn) =>
                {
                    if global.format == Format::Json {
                        print(&serde_json::to_string(&event).unwrap_or_default());
                    }
                    session = client.call::<method::SessionGet>(&ask(&identity)).await?;
                    ended(global, &session);
                    return Ok(String::new());
                }
                _ => {}
            },
            Delivery::Resync(_) => {
                eprintln!("bosun: the tail fell behind: some lines were not shown");
                let now = client.call::<method::SessionGet>(&ask(&identity)).await?;
                if !now.running && (now.attempt, now.spawn) == (session.attempt, session.spawn) {
                    ended(global, &now);
                    return Ok(String::new());
                }
            }
        }
    }
}

/// Says on stderr that the session ended, and how.
fn ended(global: &Global, session: &SessionInfo) {
    if global.format != Format::Json {
        eprintln!(
            "bosun: session {} of {} ended: {}",
            session.session,
            session.item,
            session.exit.as_deref().unwrap_or("killed")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_are_split() {
        assert_eq!(split_item("demo#12"), Some(("demo", 12)));
        assert_eq!(split_item("a#b#3"), Some(("a#b", 3)));
        assert_eq!(split_item("#3"), None);
        assert_eq!(split_item("demo"), None);
        assert_eq!(split_item("demo#x"), None);
    }

    /// #8: an item whose lease request waits says, in `bosun status` and `bosun item`, which
    /// resource it waits for and who holds it.
    #[test]
    fn a_waiting_item_says_what_it_waits_for() {
        let waiting = serde_json::json!([{"resource": "rig", "holders": ["item demo#3"]}]);
        let overview: Overview = serde_json::from_value(serde_json::json!({
            "identity": "personal",
            "daemon_version": "0.0.0",
            "started_ms": 0,
            "now_ms": 120_000,
            "killed": false,
            "paused": false,
            "events": {"epoch": "1-1", "head": 0, "subscribers": 0},
            "repos": [{
                "repo": "demo",
                "path": "/repos/demo",
                "queue": "local_file",
                "state": "working",
                "current": {
                    "item": "demo#4",
                    "state": "claimed",
                    "attempt": 0,
                    "since_ms": 0,
                    "waiting": waiting,
                },
            }],
        }))
        .unwrap();
        let mut out = String::new();
        render_status(&overview, &mut out);
        assert!(
            out.contains("  demo#4 claimed (waiting for rig: held by item demo#3), attempt 0, "),
            "{out}"
        );

        let detail: ItemDetail = serde_json::from_value(serde_json::json!({
            "identity": "personal",
            "item": "demo#4",
            "repo": { "name": "demo", "path": "/repos/demo" },
            "state": "claimed",
            "attempt": 0,
            "failed": 0,
            "created_ms": 0,
            "updated_ms": 0,
            "waiting": [
                {"resource": "rig", "holders": ["busy"]},
                {"resource": "cache/demo", "holders": ["bosun with 42"]},
            ],
            "transitions": [],
            "attempts": [],
            "calls": [],
        }))
        .unwrap();
        let mut out = String::new();
        render_item(&detail, &mut out);
        assert!(
            out.starts_with(
                "demo#4 in demo: claimed (waiting for rig: held by busy; cache/demo: held by \
                 bosun with 42), attempt 0, 0 failed\n"
            ),
            "{out}"
        );
    }

    /// #7: a repository the fast-death breaker paused says why on its line.
    #[test]
    fn a_tripped_repository_says_why_it_is_paused() {
        let why = "3 sessions in a row ended within 60 s of their spawn (the last: demo#1); \
                   bosun resume lifts it";
        let overview: Overview = serde_json::from_value(serde_json::json!({
            "identity": "personal",
            "daemon_version": "0.0.0",
            "started_ms": 0,
            "now_ms": 0,
            "killed": false,
            "paused": false,
            "events": {"epoch": "1-1", "head": 0, "subscribers": 0},
            "repos": [{
                "repo": "demo",
                "path": "/repos/demo",
                "queue": "local_file",
                "state": "idle",
                "hold": "pause",
                "tripped": why,
            }],
        }))
        .unwrap();
        let mut out = String::new();
        render_status(&overview, &mut out);
        assert!(
            out.contains(&format!("repo demo: idle, hold pause: {why}\n")),
            "{out}"
        );

        let status: bosun_api::ControlStatus = serde_json::from_value(serde_json::json!({
            "identity": "personal",
            "killed": false,
            "paused": false,
            "repos": [{"repo": "demo", "paused": false, "hold": "pause", "tripped": why}],
        }))
        .unwrap();
        let mut out = String::new();
        crate::control::render(&status, &mut out);
        assert!(
            out.contains(&format!("repo demo: hold pause: {why}\n")),
            "{out}"
        );
    }

    #[test]
    fn an_attempt_refused_before_its_verdict_shows_the_reasons() {
        let detail: ItemDetail = serde_json::from_value(serde_json::json!({
            "identity": "personal",
            "item": "demo#4",
            "repo": { "name": "demo", "path": "/repos/demo" },
            "state": "needs_you",
            "detail": "attempts_exhausted",
            "attempt": 1,
            "failed": 1,
            "created_ms": 0,
            "updated_ms": 0,
            "transitions": [],
            "attempts": [{
                "number": 1,
                "state": "fail",
                "started_ms": 0,
                "sessions": [],
                "verdicts": [],
                "refusal": ["the working tree is not clean: ?? loose.txt\nsecond line"],
            }],
            "calls": [],
        }))
        .unwrap();
        let mut out = String::new();
        render_item(&detail, &mut out);
        assert!(
            out.contains(
                "attempt 1: fail\n  refused before its verdict: the working tree is not clean: \
                 ?? loose.txt second line\n"
            ),
            "{out}"
        );
    }
}
