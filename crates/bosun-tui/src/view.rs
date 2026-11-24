//! Drawing the frame, the Overview and the Queue (#4) from an [`App`].
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

/// `bosun  [1 Overview]  2 Queue` on the left, the link on the right.
fn tab_bar(app: &App, width: u16) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " bosun ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    let mut used = 7;
    for (n, tab) in Tab::ALL.into_iter().enumerate() {
        let label = if tab == app.tab {
            format!(" [{} {}] ", n + 1, tab.title())
        } else {
            format!("  {} {}  ", n + 1, tab.title())
        };
        used += label.len();
        let style = if tab == app.tab {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    let link = match &app.link {
        Link::Connecting => "connecting".to_owned(),
        Link::Up => match &app.daemon {
            Some(version) => format!("bosund {version}"),
            None => "connected".to_owned(),
        },
        Link::Down(_) => "daemon unreachable: reconnecting".to_owned(),
    };
    let room = usize::from(width).saturating_sub(used + 1);
    spans.push(Span::raw(format!("{:>room$} ", clip(&link, room))));
    Line::from(spans)
}

/// The key line, from the shared table and the view's own keys.
pub(crate) fn key_line(tab: Tab) -> String {
    let mut out = String::from(match tab {
        Tab::Overview => " 1/2 tab  j/k move",
        Tab::Queue => " 1/2 tab  hjkl move",
    });
    for spec in ACTIONS {
        let _ = write!(out, "  {} {}", spec.key, spec.label);
    }
    out.push_str("  q quit");
    out
}

/// `text` cut to `width` columns, with `~` where it was cut.
fn clip(text: &str, width: impl Into<usize>) -> String {
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

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

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

    #[test]
    fn durations_are_short() {
        assert_eq!(duration(-5), "0s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(754_000), "12m34s");
        assert_eq!(duration(3_600_000 + 5 * 60_000), "1h05m");
        assert_eq!(duration(2 * 86_400_000 + 4 * 3_600_000), "2d04h");
    }
}
