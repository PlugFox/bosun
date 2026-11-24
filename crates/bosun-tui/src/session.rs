//! The Session tab (#4, #74): the live tail of the item in flight, its tool calls collapsed,
//! and the context-size gauge of its session.
//!
//! [`SessionView`] holds the output lines of one item: those `session.tail` returned, then the
//! `session_output` events of a tail subscription. Each line carries its sequence, so a line
//! that comes both ways is kept once. A resync means lines were missed: the view is replaced
//! by a full `session.tail`, whose lines are contiguous up to its head, and the events after
//! that head are appended; so the tail shown never has a gap (`feed.rs`).

use std::collections::VecDeque;

use bosun_api::{ContextSize, Event, EventBody, OutputStream, SessionInfo, SessionTail};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::view::{clip, duration};

/// The lines a view keeps; older ones are dropped.
pub(crate) const MAX_LINES: usize = 2000;

/// The live tail of one item.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionView {
    pub(crate) identity: String,
    pub(crate) item: String,
    /// The epoch the sequences are of.
    pub(crate) epoch: Option<String>,
    /// Every output line of the item up to this sequence is in `lines` (or was dropped from
    /// its front).
    pub(crate) seen: u64,
    /// `session_output` events of the item, in sequence order.
    pub(crate) lines: VecDeque<Event>,
    /// Lines before the first one are no longer held by the daemon, or were dropped here.
    pub(crate) truncated: bool,
    /// The item's latest session.
    pub(crate) info: Option<SessionInfo>,
    pub(crate) context: Option<ContextSize>,
    /// Why the last `session.tail` failed.
    pub(crate) error: Option<String>,
    /// Full refetches made after a resync, shown on the tab.
    pub(crate) resyncs: u64,
}

impl SessionView {
    pub(crate) fn new(identity: String, item: String) -> Self {
        Self {
            identity,
            item,
            epoch: None,
            seen: 0,
            lines: VecDeque::new(),
            truncated: false,
            info: None,
            context: None,
            error: None,
            resyncs: 0,
        }
    }

    /// Takes a full `session.tail`: what the view showed is replaced.
    pub(crate) fn replace(&mut self, tail: SessionTail) {
        self.lines.clear();
        self.seen = 0;
        self.epoch = Some(tail.epoch.clone());
        self.truncated = false;
        self.take(tail);
    }

    /// Takes a `session.tail` asked from `seen + 1`. One of another epoch (the daemon
    /// restarted), or one that says the lines after `seen` are no longer all held, replaces
    /// the view.
    pub(crate) fn extend(&mut self, tail: SessionTail) {
        if tail.gap || self.epoch.as_deref() != Some(tail.epoch.as_str()) {
            self.replace(tail);
        } else {
            self.take(tail);
        }
    }

    fn take(&mut self, tail: SessionTail) {
        let SessionTail {
            head,
            lines,
            truncated,
            session,
            context,
            ..
        } = tail;
        if self.lines.is_empty() {
            self.truncated |= truncated;
        }
        for line in lines {
            self.push(line);
        }
        self.seen = self.seen.max(head);
        self.info = session;
        self.context = context;
        self.error = None;
    }

    /// Takes one event of the tail subscription; a line already held is dropped.
    pub(crate) fn push(&mut self, event: Event) {
        let EventBody::SessionOutput { item, .. } = &event.body else {
            return;
        };
        if item != &self.item || event.seq <= self.seen {
            return;
        }
        self.seen = event.seq;
        self.lines.push_back(event);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.truncated = true;
        }
    }
}

/// One row of the tail as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// A session of the item starts its lines.
    Session { attempt: u32, spawn: u32 },
    /// A line the session printed.
    Line { stderr: bool, text: String },
    /// A run of consecutive tool calls, collapsed: how many, and each tool's count in the
    /// order first called.
    Tools {
        calls: usize,
        tools: Vec<(String, usize)>,
    },
}

/// The rows of `lines`: each session's lines under its heading, a run of tool calls in one row.
pub(crate) fn rows<'a>(lines: impl IntoIterator<Item = &'a Event>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut at = None;
    for event in lines {
        let EventBody::SessionOutput {
            attempt,
            spawn,
            stream,
            line,
            tool,
            ..
        } = &event.body
        else {
            continue;
        };
        if at != Some((*attempt, *spawn)) {
            at = Some((*attempt, *spawn));
            rows.push(Row::Session {
                attempt: *attempt,
                spawn: *spawn,
            });
        }
        match (tool, rows.last_mut()) {
            (Some(name), Some(Row::Tools { calls, tools })) => {
                *calls += 1;
                match tools.iter_mut().find(|(n, _)| n == name) {
                    Some((_, count)) => *count += 1,
                    None => tools.push((name.clone(), 1)),
                }
            }
            (Some(name), _) => rows.push(Row::Tools {
                calls: 1,
                tools: vec![(name.clone(), 1)],
            }),
            (None, _) => rows.push(Row::Line {
                stderr: *stream == OutputStream::Stderr,
                text: line.clone(),
            }),
        }
    }
    rows
}

fn row_text(row: &Row) -> String {
    match row {
        Row::Session { attempt, spawn } => format!("-- attempt {attempt}, session {spawn} --"),
        Row::Line {
            stderr: false,
            text,
        } => format!("  {text}"),
        Row::Line { stderr: true, text } => format!("! {text}"),
        Row::Tools { calls, tools } => {
            let named: Vec<String> = tools
                .iter()
                .map(|(name, n)| {
                    if *n > 1 {
                        format!("{name} x{n}")
                    } else {
                        name.clone()
                    }
                })
                .collect();
            let what = if *calls == 1 {
                "1 tool call".to_owned()
            } else {
                format!("{calls} tool calls")
            };
            format!("  > {what}: {}", named.join(", "))
        }
    }
}

/// A count of tokens, short: `950`, `12.3k`, `1.23M`.
pub(crate) fn short(n: f64) -> String {
    let n = n.max(0.0);
    if n < 1_000.0 {
        format!("{n:.0}")
    } else if n < 1_000_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else if n < 1_000_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else {
        format!("{:.2}B", n / 1_000_000_000.0)
    }
}

/// A bar of `width` cells, `filled` of `full` of it in `#`, the rest in `.`.
pub(crate) fn bar(filled: f64, full: f64, width: usize) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a cell count of a ratio in [0, 1]"
    )]
    let cells = if full > 0.0 {
        ((filled / full).clamp(0.0, 1.0) * width as f64).round() as usize
    } else {
        0
    };
    format!(
        "{}{}",
        "#".repeat(cells),
        ".".repeat(width - cells.min(width))
    )
}

/// The context line: the gauge against the window, or the size alone when no window is known.
fn gauge(context: Option<&ContextSize>) -> String {
    let Some(c) = context else {
        return "context  not ingested yet".to_owned();
    };
    #[expect(clippy::cast_precision_loss, reason = "token counts shown")]
    let (prompt, peak) = (c.prompt as f64, c.peak as f64);
    match c.window {
        #[expect(clippy::cast_precision_loss, reason = "token counts shown")]
        Some(window) => {
            let window = window as f64;
            format!(
                "context  [{}] {} of {} ({:.0}%)  peak {}",
                bar(prompt, window, 24),
                short(prompt),
                short(window),
                prompt / window * 100.0,
                short(peak)
            )
        }
        None => format!("context  {}  peak {}", short(prompt), short(peak)),
    }
}

/// Draws the Session tab. `now_ms` is the daemon's clock as the view carries it on.
pub(crate) fn draw(frame: &mut Frame<'_>, view: Option<&SessionView>, area: Rect, now_ms: i64) {
    let Some(view) = view else {
        frame.render_widget(Paragraph::new("  no item is in flight"), area);
        return;
    };
    let [head, context, rule, tail] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(area);
    let heading = match &view.info {
        Some(s) => {
            let state = if s.running {
                format!("running {}", duration(now_ms - s.started_ms))
            } else {
                format!("ended {}", s.exit.as_deref().unwrap_or("-"))
            };
            format!(
                "{} ({})  attempt {}  session {}  {} {}  {state}",
                view.item, view.identity, s.attempt, s.spawn, s.provider, s.mode
            )
        }
        None => format!("{} ({})  no session yet", view.item, view.identity),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            clip(&heading, head.width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        head,
    );
    let said = match &view.error {
        Some(e) => format!("the tail cannot be read: {e}"),
        None => gauge(view.context.as_ref()),
    };
    frame.render_widget(Paragraph::new(clip(&said, context.width)), context);
    let mut marks = vec![format!("tail: {} lines", view.lines.len())];
    if view.truncated {
        marks.push("earlier lines are no longer held".to_owned());
    }
    if view.resyncs > 0 {
        marks.push(format!("refetched after {} resync(s)", view.resyncs));
    }
    let rule_text = format!("-- {} ", marks.join(", "));
    let width = usize::from(rule.width);
    let pad = width.saturating_sub(rule_text.chars().count());
    frame.render_widget(
        Paragraph::new(clip(&format!("{rule_text}{}", "-".repeat(pad)), width))
            .style(Style::default().add_modifier(Modifier::DIM)),
        rule,
    );
    let rows = rows(&view.lines);
    // The end of the tail is shown: the last rows that fit.
    let height = usize::from(tail.height);
    let lines: Vec<Line<'_>> = rows[rows.len().saturating_sub(height)..]
        .iter()
        .map(|r| {
            let text = clip(&row_text(r), tail.width);
            match r {
                Row::Session { .. } => {
                    Line::styled(text, Style::default().add_modifier(Modifier::BOLD))
                }
                Row::Tools { .. } => {
                    Line::styled(text, Style::default().add_modifier(Modifier::DIM))
                }
                Row::Line { .. } => Line::raw(text),
            }
        })
        .collect();
    if lines.is_empty() {
        frame.render_widget(Paragraph::new("  no output yet"), tail);
    } else {
        frame.render_widget(Paragraph::new(lines), tail);
    }
}

/// The names of the tools of `rows`, each with its count, for a test.
#[cfg(test)]
pub(crate) fn tool_counts(rows: &[Row]) -> std::collections::BTreeMap<String, usize> {
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        if let Row::Tools { tools, .. } = row {
            for (name, n) in tools {
                *out.entry(name.clone()).or_insert(0) += n;
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A line of item `alpha#3`'s first session.
    pub(crate) fn line(seq: u64, text: &str) -> Event {
        Event {
            seq,
            ts_ms: 0,
            body: EventBody::SessionOutput {
                item: "alpha#3".to_owned(),
                attempt: 2,
                spawn: 1,
                stream: OutputStream::Stdout,
                line: text.to_owned(),
                tool: None,
            },
        }
    }

    /// A tool call of the same session, as the daemon publishes it.
    pub(crate) fn tool(seq: u64, name: &str) -> Event {
        let mut event = line(seq, &format!("tool: {name}"));
        if let EventBody::SessionOutput { tool, .. } = &mut event.body {
            *tool = Some(name.to_owned());
        }
        event
    }

    /// What the daemon publishes for a `fake-agent` session replaying a stream fixture of the
    /// Claude runner (`fixtures/claude-stream/normal-end.jsonl` and longer): the live entries
    /// of its lines, tool calls tagged with their tool.
    pub(crate) fn fake_agent_stream() -> Vec<Event> {
        let mut seq = 0;
        let mut next = || {
            seq += 1;
            seq
        };
        let mut out = vec![line(next(), "started (claude-opus-5-5)")];
        out.push(line(next(), "Reading the frame parser first."));
        for name in ["Read", "Read", "Grep", "Read"] {
            out.push(tool(next(), name));
        }
        out.push(line(next(), "The header length is read as u16; it is u32."));
        for name in ["Edit", "Bash"] {
            out.push(tool(next(), name));
        }
        let mut err = line(next(), "warning: unused variable `len`");
        if let EventBody::SessionOutput { stream, .. } = &mut err.body {
            *stream = OutputStream::Stderr;
        }
        out.push(err);
        out.push(tool(next(), "Bash"));
        out.push(line(next(), "The flag is added and the tests pass."));
        out.push(line(next(), "ended"));
        out
    }

    #[test]
    fn a_run_of_tool_calls_is_one_row() {
        let rows = rows(&fake_agent_stream());
        assert_eq!(
            rows[3],
            Row::Tools {
                calls: 4,
                tools: vec![("Read".to_owned(), 3), ("Grep".to_owned(), 1)],
            }
        );
        let tools = rows
            .iter()
            .filter(|r| matches!(r, Row::Tools { .. }))
            .count();
        assert_eq!(tools, 3, "{rows:?}");
        assert_eq!(rows.len(), 10, "{rows:?}");
        let counts = tool_counts(&rows);
        assert_eq!(counts["Read"], 3);
        assert_eq!(counts["Bash"], 2);
        // A text line that merely reads like a tool call is not one.
        let rows = super::rows(&[line(1, "tool: not really")]);
        assert!(matches!(rows[1], Row::Line { .. }), "{rows:?}");
    }

    #[test]
    fn a_line_is_kept_once_and_only_for_the_item() {
        let mut view = SessionView::new("personal".to_owned(), "alpha#3".to_owned());
        view.push(line(1, "a"));
        view.push(line(1, "a"));
        let mut other = line(2, "b");
        if let EventBody::SessionOutput { item, .. } = &mut other.body {
            *item = "beta#8".to_owned();
        }
        view.push(other);
        view.push(line(3, "c"));
        let seqs: Vec<u64> = view.lines.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 3]);
    }

    #[test]
    fn counts_are_short() {
        assert_eq!(short(950.0), "950");
        assert_eq!(short(12_345.0), "12.3k");
        assert_eq!(short(1_234_567.0), "1.23M");
        assert_eq!(bar(1.0, 4.0, 8), "##......");
        assert_eq!(bar(9.0, 4.0, 4), "####");
        assert_eq!(bar(1.0, 0.0, 3), "...");
    }
}
