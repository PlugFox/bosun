//! The Tokens tab (#4, #74): per identity, never summed (#2), a speedometer (eqv per minute
//! against the reference pace), the odometers of today and of this run of the daemon, and a
//! sparkline of the last hour, all from `tokens.pace`.
//!
//! Until limit windows are measured (#10) the reference pace is the configured daily budget's,
//! and the tab says so: its reference line names it "configured".

use bosun_api::{Pace, PaceReference};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Sparkline};

use crate::app::App;
use crate::session::{bar, short};
use crate::view::{clip, duration};

/// The minutes the speed is averaged over.
const SPEED_MINUTES: u32 = 5;

/// The rows of one identity's section.
const SECTION: u16 = 10;

/// Eqv per minute over the last [`SPEED_MINUTES`] of `pace`'s buckets.
pub(crate) fn speed(pace: &Pace) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a bucket's width in ms")]
    let bucket_min = (pace.bucket_ms.max(1) as f64) / 60_000.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a bucket count"
    )]
    let n = ((f64::from(SPEED_MINUTES) / bucket_min).round() as usize)
        .clamp(1, pace.buckets.len().max(1));
    let last = &pace.buckets[pace.buckets.len().saturating_sub(n)..];
    #[expect(clippy::cast_precision_loss, reason = "a bucket count")]
    let minutes = last.len().max(1) as f64 * bucket_min;
    last.iter().sum::<f64>() / minutes
}

/// The reference pace, eqv per minute, and the line that says what it is.
pub(crate) fn reference(pace: &Pace) -> (Option<f64>, String) {
    match &pace.reference {
        Some(PaceReference::Configured {
            daily_eqv,
            reserve_eqv,
            cap_eqv,
            eqv_per_minute,
        }) => {
            #[expect(clippy::cast_precision_loss, reason = "eqv shown")]
            let (daily, reserve, cap) =
                (daily_eqv.0 as f64, reserve_eqv.0 as f64, cap_eqv.0 as f64);
            let until = duration(pace.reset_ms - pace.now_ms);
            // The mutant `mutant_tui_pace_unlabelled` drops what the reference is.
            let label = if cfg!(mutant_tui_pace_unlabelled) {
                ""
            } else {
                "configured "
            };
            (
                Some(*eqv_per_minute),
                format!(
                    "ref    {label}{}/min: the rest of the cap spent by its reset in \
                     {until}\n\
                     cap    {} eqv a day: budget.daily_eqv {} less budget.reserve_eqv {}",
                    short(*eqv_per_minute),
                    short(cap),
                    short(daily),
                    short(reserve),
                ),
            )
        }
        None => (
            None,
            "ref    none: no daily budget is configured (budget.daily_eqv)\ncap    none".to_owned(),
        ),
    }
}

/// A bucket's width, as the sparkline's caption names it.
fn per(bucket_ms: u64) -> String {
    match bucket_ms {
        60_000 => "minute".to_owned(),
        ms if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        ms => duration(i64::try_from(ms).unwrap_or(i64::MAX)),
    }
}

/// The lines of one identity's section, but the sparkline.
pub(crate) fn section(name: &str, pace: &Pace, width: usize) -> Vec<String> {
    let speed = speed(pace);
    let (pace_ref, ref_line) = reference(pace);
    // The scale leaves room above the reference, and above the speed.
    let full = pace_ref
        .map_or(speed, |r| r * 2.0)
        .max(speed * 1.25)
        .max(1.0);
    let cells = width.saturating_sub(34).clamp(10, 40);
    let mut meter = bar(speed, full, cells);
    if let Some(r) = pace_ref {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a cell of a ratio in [0, 1]"
        )]
        let at = ((r / full).clamp(0.0, 1.0) * (cells - 1) as f64).round() as usize;
        meter.replace_range(at..=at, "|");
    }
    let today = match &pace.reference {
        Some(PaceReference::Configured { cap_eqv, .. }) if cap_eqv.0 > 0 => {
            #[expect(clippy::cast_precision_loss, reason = "a share shown")]
            let share = pace.today_eqv / cap_eqv.0 as f64 * 100.0;
            format!("  {share:.0}% of the cap")
        }
        _ => String::new(),
    };
    let mut lines = vec![
        format!("{name}: eqv of {}", pace.reference_model),
        format!(
            "speed  {:>7}/min over {SPEED_MINUTES} min  [{meter}]",
            short(speed)
        ),
    ];
    lines.extend(ref_line.lines().map(ToOwned::to_owned));
    lines.extend([
        format!(
            "today  {:>7} eqv since local midnight{today}",
            short(pace.today_eqv)
        ),
        format!(
            "run    {:>7} eqv since the daemon started {} ago",
            short(pace.run_eqv),
            duration(pace.now_ms - pace.run_start_ms)
        ),
        format!(
            "the last hour, eqv per {} (peak {}):",
            per(pace.bucket_ms),
            short(pace.buckets.iter().copied().fold(0.0, f64::max))
        ),
    ]);
    lines.into_iter().map(|l| clip(&l, width)).collect()
}

/// Draws the Tokens tab: one section per identity.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let sections = Layout::vertical(
        app.identities
            .iter()
            .map(|_| Constraint::Length(SECTION))
            .chain([Constraint::Min(0)]),
    )
    .split(area);
    for ((name, view), area) in app.identities.iter().zip(sections.iter()) {
        let Some(pace) = &view.pace else {
            let said = match &view.pace_error {
                Some(e) => format!("{name}: the pace cannot be read: {e}"),
                None => format!("{name}: waiting for the daemon"),
            };
            frame.render_widget(Paragraph::new(clip(&said, area.width)), *area);
            continue;
        };
        let [text, spark, _] = Layout::vertical([
            Constraint::Length(7),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(*area);
        let mut lines: Vec<Line<'_>> = section(name, pace, usize::from(text.width))
            .into_iter()
            .map(Line::raw)
            .collect();
        if let Some(first) = lines.first_mut() {
            *first = first
                .clone()
                .style(Style::default().add_modifier(Modifier::BOLD));
        }
        frame.render_widget(Paragraph::new(lines), text);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "eqv per bucket, drawn"
        )]
        let data: Vec<u64> = pace
            .buckets
            .iter()
            .map(|b| b.max(0.0).round() as u64)
            .collect();
        frame.render_widget(Sparkline::default().data(&data), spark);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use bosun_api::Wide;

    use super::*;
    use crate::app::tests::NOW;

    /// A synthetic pace: an hour of spending that rises to 30k eqv per minute, under a
    /// configured daily budget.
    pub(crate) fn pace(identity: &str) -> Pace {
        let buckets = (0..60)
            .map(|i| f64::from(i % 12) * 2_500.0 + if i >= 55 { 5_000.0 } else { 0.0 })
            .collect();
        Pace {
            identity: identity.to_owned(),
            now_ms: NOW,
            reference_model: "claude-opus-5-5".to_owned(),
            day_start_ms: NOW - 9 * 3_600_000,
            reset_ms: NOW + 15 * 3_600_000 - 12 * 60_000,
            today_eqv: 4_321_000.0,
            run_start_ms: NOW - 7_500_000,
            run_eqv: 1_234_500.0,
            bucket_ms: 60_000,
            buckets,
            reference: Some(PaceReference::Configured {
                daily_eqv: Wide(40_000_000),
                reserve_eqv: Wide(5_000_000),
                cap_eqv: Wide(35_000_000),
                eqv_per_minute: 34_000.0,
            }),
        }
    }

    #[test]
    fn the_speed_is_the_mean_of_the_last_minutes() {
        let mut p = pace("personal");
        p.buckets = vec![0.0; 60];
        for b in &mut p.buckets[55..] {
            *b = 10_000.0;
        }
        assert!((speed(&p) - 10_000.0).abs() < 1e-9);
        p.bucket_ms = 120_000;
        // Five minutes are three two-minute buckets: 30k eqv over six minutes.
        assert!((speed(&p) - 5_000.0).abs() < 1e-9, "{}", speed(&p));
    }

    #[test]
    fn no_budget_is_no_reference() {
        let mut p = pace("personal");
        p.reference = None;
        let (r, line) = reference(&p);
        assert_eq!(r, None);
        assert!(line.contains("no daily budget is configured"), "{line}");
    }
}
