//! Drawing the frame, the Overview, the Queue (#4) and the Resources tab (#8) from an [`App`];
//! the Session and Tokens tabs are drawn by `session.rs` and `tokens.rs`.
//!
//! The frame is the tab bar on the first line, the tab's body, a message line and the key
//! line; a pending question is drawn over the body. The key line is built from the shared
//! action table, so it names the same keys [`App::on_key`] reads. Text in ASCII and the
//! borders in box-drawing characters, without colour, so a snapshot of the test backend reads
//! the same on every platform.

use std::fmt::Write as _;

use bosun_api::{CardView, Overview, RepoOverview};
use bosun_client::actions::ACTIONS;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, Link, Tab};

/// Draws the whole frame. `now_ms` is the local clock, against which the time in state is
/// counted on from the last refresh.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, now_ms: i64) {
    let [bar, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(Paragraph::new(tab_bar(app, bar.width)), bar);
    match app.tab {
        Tab::Overview => overview(frame, app, body, now_ms),
        Tab::Queue => queue(frame, app, body),
        Tab::Session => {
            let session = app.session.as_ref();
            let daemon_now = session
                .and_then(|s| app.identities.iter().find(|(n, _)| *n == s.identity))
                .and_then(|(_, v)| {
                    v.overview
                        .as_ref()
                        .map(|o| o.now_ms + (now_ms - v.fetched_ms).max(0))
                })
                .unwrap_or(now_ms);
            crate::session::draw(frame, session, body, daemon_now);
        }
        Tab::Tokens => crate::tokens::draw(frame, app, body),
        Tab::Resources => resources(frame, app, body),
    }
    let said = app.message.as_deref().unwrap_or("");
    frame.render_widget(Paragraph::new(clip(said, message.width)), message);
    frame.render_widget(
        Paragraph::new(clip(&key_line(app.tab), keys.width))
            .style(Style::default().add_modifier(Modifier::DIM)),
        keys,
    );
    if let Some(pending) = &app.pending {
        question(frame, body, &pending.question);
    }
}

/// `bosun  [1 Overview]  2 Queue` on the left, the link on the right. When the link does not
/// fit beside every tab's title, the tabs not shown are drawn by their number only.
fn tab_bar(app: &App, width: u16) -> Line<'static> {
    let link = match &app.link {
        Link::Connecting => "connecting".to_owned(),
        Link::Up => match &app.daemon {
            Some(version) => format!("bosund {version}"),
            None => "connected".to_owned(),
        },
        Link::Down(_) => "daemon unreachable: reconnecting".to_owned(),
    };
    let labels = |short: bool| -> Vec<String> {
        Tab::ALL
            .into_iter()
            .enumerate()
            .map(|(n, tab)| match (tab == app.tab, short) {
                (true, _) => format!(" [{} {}]", n + 1, tab.title()),
                (false, false) => format!(" {} {}", n + 1, tab.title()),
                (false, true) => format!(" {}", n + 1),
            })
            .collect()
    };
    let mut tabs = labels(false);
    let length = |tabs: &[String]| 6 + tabs.iter().map(String::len).sum::<usize>();
    if length(&tabs) + link.len() + 2 > usize::from(width) {
        tabs = labels(true);
    }
    let used = length(&tabs);
    let mut spans = vec![Span::styled(
        " bosun",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (label, tab) in tabs.into_iter().zip(Tab::ALL) {
        let style = if tab == app.tab {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    let room = usize::from(width).saturating_sub(used + 1);
    spans.push(Span::raw(format!("{:>room$} ", clip(&link, room))));
    Line::from(spans)
}

/// The key line, from the shared table and the view's own keys.
pub(crate) fn key_line(tab: Tab) -> String {
    let mut out = String::from(match tab {
        Tab::Overview | Tab::Queue => " 1-5 tab  arrows",
        Tab::Session | Tab::Tokens | Tab::Resources => " 1-5 tab",
    });
    for spec in ACTIONS {
        let _ = write!(out, "  {} {}", spec.key, spec.label);
    }
    out.push_str("  q quit");
    out
}

/// `text` cut to `width` columns, with `~` where it was cut.
pub(crate) fn clip(text: &str, width: impl Into<usize>) -> String {
    let width = width.into();
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('~');
    out
}

/// A field padded or cut to `width`.
fn cell(text: &str, width: usize) -> String {
    format!("{:<width$}", clip(text, width))
}

/// `ms` as a short duration: `45s`, `12m30s`, `3h05m`, `2d04h`.
pub(crate) fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        3600..86_400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// A size on disk in human units: bytes below 1 KiB, else KiB, MiB or GiB with one decimal.
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    #[expect(clippy::cast_precision_loss, reason = "a size shown to one decimal")]
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// Lines with the `selected` one kept in view: the paragraph's scroll offset.
fn scroll(selected: Option<usize>, height: u16) -> u16 {
    let height = usize::from(height).max(1);
    let offset = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    u16::try_from(offset).unwrap_or(u16::MAX)
}

/// The control state of an identity, on its heading line.
fn control(o: &Overview) -> String {
    let mut out = Vec::new();
    if o.killed {
        out.push("KILL SWITCH THROWN".to_owned());
    }
    if o.paused {
        out.push("paused".to_owned());
    }
    if let Some(until) = o.daily_cap_until_ms {
        out.push(format!(
            "daily cap for {}",
            duration(until.saturating_sub(o.now_ms))
        ));
    }
    if out.is_empty() {
        "running".to_owned()
    } else {
        out.join(", ")
    }
}

/// One repository row of the Overview: repo, state, item, attempt, time in state, last
/// verdict, hold.
fn repo_row(repo: &RepoOverview, now_ms: i64) -> String {
    let (state, item, attempt, time) = match &repo.current {
        Some(c) => (
            c.state.as_str(),
            c.item.as_str(),
            c.attempt.to_string(),
            duration(now_ms - c.since_ms),
        ),
        None => (repo.state.as_str(), "-", "-".to_owned(), "-".to_owned()),
    };
    let verdict = repo.last_verdict.as_ref().map_or_else(
        || "-".to_owned(),
        |v| format!("{} {} a{}", v.result, v.item, v.attempt),
    );
    format!(
        "  {} {} {} {:>3} {:>7}  {} {}",
        cell(&repo.repo, 12),
        cell(state, 9),
        cell(item, 12),
        attempt,
        time,
        cell(&verdict, 16),
        cell(repo.hold.as_deref().unwrap_or("-"), 9),
    )
}

fn overview(frame: &mut Frame<'_>, app: &App, area: Rect, now_ms: i64) {
    let [table, detail] = Layout::vertical([Constraint::Min(1), Constraint::Length(5)]).areas(area);
    let mut lines = vec![Line::styled(
        format!(
            "  {} {} {} {:>3} {:>7}  {} {}",
            cell("REPO", 12),
            cell("STATE", 9),
            cell("ITEM", 12),
            "ATT",
            "IN STATE",
            cell("LAST VERDICT", 16),
            cell("HOLD", 9),
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    let mut selected_line = None;
    let mut row = 0;
    for (name, view) in &app.identities {
        let heading = match (&view.overview, &view.error) {
            (_, Some(error)) => format!("{name}: {error}"),
            (Some(o), None) => format!("{name}: {}", control(o)),
            (None, None) => format!("{name}: waiting for the daemon"),
        };
        lines.push(Line::styled(
            clip(&heading, table.width),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let Some(o) = &view.overview else { continue };
        // The daemon's clock, carried on by the local clock since the answer.
        let daemon_now = o.now_ms + (now_ms - view.fetched_ms).max(0);
        if o.repos.is_empty() {
            lines.push(Line::raw("  no repository"));
        }
        for repo in &o.repos {
            let text = repo_row(repo, daemon_now);
            if row == app.overview_row {
                selected_line = Some(lines.len());
                lines.push(Line::styled(text, SELECTED));
            } else {
                lines.push(Line::raw(text));
            }
            row += 1;
        }
    }
    let offset = scroll(selected_line, table.height);
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), table);
    let rows = app.overview_rows();
    let text = match rows.get(app.overview_row) {
        Some((identity, o, repo)) => {
            let mut text = vec![
                format!("{} ({identity})  {}", repo.repo, repo.path),
                format!("queue {}  state {}", repo.queue, repo.state),
            ];
            if let Some(c) = &repo.current {
                let detail = c
                    .detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(": {d}"));
                text.push(format!(
                    "{} {}{detail}, attempt {}",
                    c.item, c.state, c.attempt
                ));
            }
            text.push(format!(
                "events: epoch {} at {}, {} subscriber(s)",
                o.events.epoch, o.events.head, o.events.subscribers
            ));
            text
        }
        None => vec!["nothing to show yet".to_owned()],
    };
    detail_block(frame, detail, " repository ", &text);
}

fn detail_block(frame: &mut Frame<'_>, area: Rect, title: &str, text: &[String]) {
    let lines: Vec<Line<'_>> = text.iter().map(|t| Line::raw(t.clone())).collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .title(title.to_owned()),
        ),
        area,
    );
}

/// One card line: number, title, state, snapshot, markers.
fn card_row(card: &CardView, width: u16) -> String {
    let mut marks = String::new();
    if card.edit_pending {
        marks.push_str(" EDIT");
    }
    let errors = card.lint.iter().filter(|f| f.severity == "error").count();
    let warnings = card.lint.len() - errors;
    if !card.lint.is_empty() {
        let _ = write!(marks, " lint {errors}e{warnings}w");
    }
    let snapshot = card
        .snapshot_hash
        .as_deref()
        .map_or_else(|| "-".to_owned(), |h| h.chars().take(8).collect());
    let line = format!(
        "  #{:<4} {} {} {}{marks}",
        card.card,
        cell(&card.title, 34),
        cell(card.state.as_deref().unwrap_or("-"), 10),
        cell(&snapshot, 8),
    );
    clip(&line, width)
}

fn queue(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let boards = app.boards();
    let Some((identity, board)) = boards.get(app.board) else {
        frame.render_widget(Paragraph::new("  no board yet"), area);
        return;
    };
    let [head, list, detail] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(5),
    ])
    .areas(area);
    let hash = board
        .hash
        .as_deref()
        .map_or_else(|| "-".to_owned(), |h| h.chars().take(8).collect());
    frame.render_widget(
        Paragraph::new(Line::styled(
            clip(
                &format!(
                    "{} ({identity})  {}  board {hash}  [{}/{}]",
                    board.repo,
                    board.queue,
                    app.board + 1,
                    boards.len()
                ),
                head.width,
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        head,
    );
    if let Some(error) = &board.error {
        frame.render_widget(
            Paragraph::new(format!("  the board was not read: {error}")).wrap(Wrap { trim: false }),
            list,
        );
        return;
    }
    let mut lines = Vec::new();
    let mut selected_line = None;
    let mut n = 0;
    for column in &board.columns {
        lines.push(Line::styled(
            format!("{} ({})", column.column, column.cards.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        for card in &column.cards {
            let text = card_row(card, list.width);
            if n == app.card {
                selected_line = Some(lines.len());
                lines.push(Line::styled(text, SELECTED));
            } else {
                lines.push(Line::raw(text));
            }
            n += 1;
        }
    }
    let offset = scroll(selected_line, list.height);
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), list);
    let cards = App::cards(board);
    let text = match cards.get(app.card) {
        Some((column, card)) => {
            let mut text = vec![format!("{} in {column}: {}", card.item, card.title)];
            if card.edit_pending {
                text.push(
                    "edited since its snapshot: the edit applies at the next claim".to_owned(),
                );
            }
            if card.lint.is_empty() {
                text.push("no lint finding".to_owned());
            }
            for finding in &card.lint {
                text.push(format!("{}: {}", finding.severity, finding.message));
            }
            text
        }
        None => vec!["no card".to_owned()],
    };
    detail_block(frame, detail, " card ", &text);
}

/// The Resources tab (#8): per identity, its resource instances (name, kind, held out of the
/// capacity, waiting requests, holders) and its worktrees on disk or quarantined (item, state,
/// size, path). Read-only: no key acts on a row.
fn resources(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines = Vec::new();
    for (name, view) in &app.identities {
        let heading = match (&view.resources, &view.resources_error) {
            (_, Some(error)) => format!("{name}: {error}"),
            (Some(_), None) => format!("{name}: resources"),
            (None, None) => format!("{name}: waiting for the daemon"),
        };
        lines.push(Line::styled(clip(&heading, area.width), BOLD));
        let Some(list) = &view.resources else {
            continue;
        };
        // Mutant (`mutant_tui_resources_tab_blank`): the tab draws no rows.
        let blank = cfg!(mutant_tui_resources_tab_blank);
        lines.push(Line::styled(
            format!(
                "  {} {} {:>5} {:>7}  {}",
                cell("RESOURCE", 18),
                cell("KIND", 10),
                "HELD",
                "WAITING",
                "HOLDERS"
            ),
            BOLD,
        ));
        if list.resources.is_empty() {
            lines.push(Line::raw("  no resource is configured"));
        }
        for r in list.resources.iter().filter(|_| !blank) {
            let holders = if r.holders.is_empty() {
                "-".to_owned()
            } else {
                r.holders.join(", ")
            };
            let held = format!("{}/{}", r.holders.len(), r.capacity);
            let text = format!(
                "  {} {} {held:>5} {:>7}  {holders}",
                cell(&r.name, 18),
                cell(&r.kind, 10),
                r.waiting,
            );
            lines.push(Line::raw(clip(&text, area.width)));
        }
        lines.push(Line::styled(
            format!(
                "  {} {} {:>10}  {}",
                cell("WORKTREE OF", 14),
                cell("STATE", 11),
                "SIZE",
                "PATH"
            ),
            BOLD,
        ));
        if list.worktrees.is_empty() {
            lines.push(Line::raw("  no worktree on disk"));
        }
        for w in list.worktrees.iter().filter(|_| !blank) {
            let blocked = w
                .blocking
                .as_deref()
                .map_or_else(String::new, |b| format!("  (blocked by {b})"));
            let text = format!(
                "  {} {} {:>10}  {}{blocked}",
                cell(&w.item, 14),
                cell(&w.state, 11),
                w.bytes.map_or_else(|| "-".to_owned(), size),
                w.path,
            );
            lines.push(Line::raw(clip(&text, area.width)));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The question of a pending action, over the body.
fn question(frame: &mut Frame<'_>, body: Rect, text: &str) {
    let width = body.width.saturating_sub(8).max(20).min(body.width);
    let area = Rect {
        x: body.x + (body.width - width) / 2,
        y: body.y + body.height.saturating_sub(7) / 2,
        width,
        height: 7.min(body.height),
    };
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::raw(text.to_owned()),
        Line::raw(""),
        Line::styled(
            "y: yes   any other key: no, nothing is sent",
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" confirm ")),
        area,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::Key;
    use crate::app::tests::{NOW, app};

    /// The frame as text, one line per row.
    pub(crate) fn screen(app: &App, now_ms: i64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, app, now_ms)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// Compares `got` with the committed snapshot `name`; `BOSUN_UPDATE_SNAPSHOTS=1` writes it.
    fn snapshot(name: &str, got: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("snapshots")
            .join(format!("{name}.txt"));
        if std::env::var_os("BOSUN_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, got).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| {
                panic!(
                    "{}: {e}; BOSUN_UPDATE_SNAPSHOTS=1 writes it",
                    path.display()
                )
            })
            .replace("\r\n", "\n");
        assert!(
            want == got,
            "{name} differs from {}:\n--- want\n{want}--- got\n{got}",
            path.display()
        );
    }

    #[test]
    fn the_overview_renders_at_80x24() {
        // Ten seconds after the answer: the time in state counts on.
        snapshot("overview", &screen(&app(), NOW + 10_000));
    }

    #[test]
    fn the_queue_renders_at_80x24() {
        let mut app = app();
        let _ = app.on_key(Key::Char('2'));
        let _ = app.on_key(Key::Down);
        snapshot("queue", &screen(&app, NOW));
        let _ = app.on_key(Key::Right);
        snapshot("queue-unread-board", &screen(&app, NOW));
    }

    #[test]
    fn a_question_is_drawn_over_the_body() {
        let mut app = app();
        let _ = app.on_key(Key::Char('p'));
        snapshot("confirm", &screen(&app, NOW));
    }

    #[test]
    fn an_unreachable_daemon_keeps_the_last_view() {
        let mut app = app();
        app.link = Link::Down("closed".to_owned());
        let text = screen(&app, NOW);
        assert!(text.contains("daemon unreachable: reconnecting"), "{text}");
        assert!(text.contains("alpha#3"), "{text}");
    }

    #[test]
    fn the_key_line_names_every_action_of_the_table() {
        for tab in Tab::ALL {
            let line = key_line(tab);
            assert!(line.len() <= 80, "{line}");
            for spec in ACTIONS {
                assert!(
                    line.contains(&format!("{} {}", spec.key, spec.label)),
                    "{line}"
                );
            }
        }
    }

    /// The app with the Session tab following alpha#3, fed the lines of a `fake-agent` stream.
    fn session_app() -> App {
        let mut app = app();
        let _ = app.on_key(Key::Char('3'));
        let (identity, item) = app.followed().expect("alpha#3 is in flight");
        let mut view = crate::session::SessionView::new(identity, item);
        view.replace(bosun_api::SessionTail {
            identity: "personal".to_owned(),
            item: "alpha#3".to_owned(),
            epoch: "1-1".to_owned(),
            head: 13,
            lines: crate::session::tests::fake_agent_stream(),
            gap: false,
            truncated: false,
            session: Some(bosun_api::SessionInfo {
                item: "alpha#3".to_owned(),
                attempt: 2,
                spawn: 1,
                session: "00000000-0000-4000-8000-000000000003".to_owned(),
                provider: "claude".to_owned(),
                mode: "fresh".to_owned(),
                started_ms: NOW - 754_000,
                ended_ms: None,
                exit: None,
                running: true,
            }),
            context: Some(bosun_api::ContextSize {
                prompt: 92_400,
                peak: 120_000,
                model: "claude-opus-5-5".to_owned(),
                ts_ms: NOW - 2_000,
                window: Some(200_000),
            }),
        });
        app.session = Some(view);
        app
    }

    /// #74: the tail of a `fake-agent` stream renders at 80x24 with its tool calls collapsed:
    /// a run of calls is one row, and no `tool:` line is drawn on its own.
    #[test]
    fn the_session_tab_renders_at_80x24() {
        let text = screen(&session_app(), NOW + 10_000);
        snapshot("session", &text);
        assert!(text.contains("> 4 tool calls: Read x3, Grep"), "{text}");
        assert!(text.contains("> 2 tool calls: Edit, Bash"), "{text}");
        assert!(!text.contains("tool: "), "{text}");
        assert!(text.contains("context  ["), "{text}");
    }

    /// #74: the speedometer's reference is the configured budget, and the tab says so; with
    /// `mutant_tui_pace_unlabelled` the reference is drawn unlabelled.
    #[test]
    fn the_speedometer_labels_its_reference_configured() {
        let mut app = app();
        app.identities[0].1.pace = Some(crate::tokens::tests::pace("personal"));
        let _ = app.on_key(Key::Char('4'));
        let text = screen(&app, NOW);
        let reference = text
            .lines()
            .find(|l| l.starts_with("ref "))
            .unwrap_or_else(|| panic!("no reference line:\n{text}"));
        assert!(
            reference.contains("ref    configured "),
            "the reference is not labelled configured: {reference}"
        );
        assert!(text.contains("speed "), "{text}");
        assert!(text.contains("today "), "{text}");
        assert!(text.contains("run "), "{text}");
        snapshot("tokens", &text);
    }

    /// A `lease.list` answer: `rig` held by alpha#3 with one request waiting, a free `cache`,
    /// and a quarantined worktree.
    fn resources(identity: &str) -> bosun_api::Resources {
        bosun_api::Resources {
            identity: identity.to_owned(),
            resources: vec![
                bosun_api::ResourceView {
                    name: "cache".to_owned(),
                    kind: "build-dir".to_owned(),
                    capacity: 2,
                    scope: "machine".to_owned(),
                    holders: Vec::new(),
                    waiting: 0,
                },
                bosun_api::ResourceView {
                    name: "rig".to_owned(),
                    kind: "device".to_owned(),
                    capacity: 1,
                    scope: "machine".to_owned(),
                    holders: vec!["item alpha#3".to_owned()],
                    waiting: 1,
                },
            ],
            worktrees: vec![bosun_api::WorktreeView {
                item: "alpha#2".to_owned(),
                path: "/work/worktrees/alpha-2".to_owned(),
                kind: "item".to_owned(),
                state: "quarantined".to_owned(),
                bytes: Some(5 * 1024 * 1024 + 300 * 1024),
                blocking: Some("target/lock".to_owned()),
            }],
        }
    }

    /// #8: key `5` shows the Resources tab; it draws the held resource with its holder and
    /// waiting count, and the quarantined worktree with its item and state.
    /// `mutant_tui_resources_tab_blank` draws no rows.
    #[test]
    fn the_resources_tab_lists_holders_waits_and_worktrees() {
        let mut app = app();
        app.identities[0].1.resources = Some(resources("personal"));
        assert_eq!(app.on_key(Key::Char('5')), None);
        assert_eq!(app.tab, Tab::Resources);
        let text = screen(&app, NOW);
        let rig = text
            .lines()
            .find(|l| l.trim_start().starts_with("rig "))
            .unwrap_or_else(|| panic!("no row of rig:\n{text}"));
        assert!(rig.contains("item alpha#3"), "{rig}");
        let cells: Vec<&str> = rig.split_whitespace().collect();
        assert_eq!(
            cells.get(2..4),
            Some(&["1/1", "1"][..]),
            "held and waiting: {rig}"
        );
        let worktree = text
            .lines()
            .find(|l| l.contains("alpha#2"))
            .unwrap_or_else(|| panic!("no row of alpha#2:\n{text}"));
        assert!(worktree.contains("quarantined"), "{worktree}");
        assert!(worktree.contains("5.3 MiB"), "{worktree}");
        snapshot("resources", &text);
    }

    #[test]
    fn sizes_are_in_human_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(5 * 1024 * 1024 + 300 * 1024), "5.3 MiB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(size(2048 * 1024 * 1024 * 1024), "2048.0 GiB");
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(duration(-5), "0s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(754_000), "12m34s");
        assert_eq!(duration(3_600_000 + 5 * 60_000), "1h05m");
        assert_eq!(duration(2 * 86_400_000 + 4 * 3_600_000), "2d04h");
    }
}
