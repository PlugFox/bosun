//! Holds and budgets in the loop (#7, #67): what the loop reads of [`crate::control::Control`]
//! and what it writes back.
//!
//! - Every step's context carries the hold in force on the repository ([`Loop::hold`]); the
//!   machine parks a session that would start under it (a pause and the daily cap let the
//!   current attempt finish its verdict), and the scheduler claims nothing under it.
//! - A change of the control state wakes the loop ([`Loop::on_control`]): the kill switch kills
//!   the child in flight at once and parks the item; a lifted hold wakes a parked item.
//! - Every few ticks ([`Watch::due`]) the loop reads the store: the interactive sessions in the
//!   repository's checkout (the automatic pause), the identity's eqv since local midnight (the
//!   daily cap, less the reserve), and the item's own eqv and active wall-clock time against its
//!   caps (the card's, else `[budget]`). An item over a cap is killed and goes to Needs you; the
//!   figure is the `budget_exceeded` event, the daemon's log, and the item's note, which its
//!   Needs-you comment and sticky report show (#111).

use std::time::Duration;

use bosun_api::{EventBody, Wide};
use bosun_core::{Event, Hold, ItemId, State};
use bosun_store::{By, Group, Write};

use super::{Loop, Signal};

/// The loop's own state for holds and budgets.
#[derive(Debug, Default)]
pub(super) struct Watch {
    /// The item in flight's meter.
    meter: Option<Meter>,
    /// When the store was last read for usage and activity (the loop's monotonic clock).
    last_read: Option<Duration>,
}

/// The active time of the item in flight: from its claim, less its time parked.
#[derive(Debug)]
struct Meter {
    item: ItemId,
    start_ms: i64,
    parked_ms: i64,
    parked_since: Option<i64>,
}

impl Meter {
    fn observe(&mut self, state: State, now_ms: i64) {
        match (state, self.parked_since) {
            (State::Parked(_), None) => self.parked_since = Some(now_ms),
            (State::Parked(_), Some(_)) | (_, None) => {}
            (_, Some(since)) => {
                self.parked_ms += now_ms - since;
                self.parked_since = None;
            }
        }
    }

    fn active_ms(&self, now_ms: i64) -> i64 {
        let parked = self.parked_ms + self.parked_since.map_or(0, |s| now_ms - s);
        (now_ms - self.start_ms - parked).max(0)
    }
}

impl Watch {
    /// The store is read every five ticks.
    fn due(&mut self, now: Duration, poll: Duration) -> bool {
        let every = poll * 5;
        if self
            .last_read
            .is_some_and(|last| now.saturating_sub(last) < every)
        {
            return false;
        }
        self.last_read = Some(now);
        true
    }
}

/// Input-equivalent tokens, rounded down.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole(eqv: f64) -> u64 {
    eqv.max(0.0) as u64
}

fn ms_u64(ms: i64) -> u64 {
    u64::try_from(ms).unwrap_or(0)
}

impl Loop {
    /// The identity that owns this loop's repository.
    fn identity_name(&self) -> Option<String> {
        self.config()
            .repos
            .get(&self.shared.options.repo)
            .map(|r| r.identity.clone())
    }

    /// The active time the item in flight has used so far: what its wall-clock cap is
    /// compared with. Zero before its meter starts.
    pub(super) fn active_time(&self) -> Duration {
        let now = self.shared.deps.clock.wall_ms();
        match (&self.current, &self.watch.meter) {
            (Some(cur), Some(meter)) if meter.item == cur.item.id => {
                Duration::from_millis(ms_u64(meter.active_ms(now)))
            }
            _ => Duration::ZERO,
        }
    }

    /// The hold in force on this repository at `now_ms`.
    pub(super) fn hold(&self, now_ms: i64) -> Option<Hold> {
        let identity = self.identity_name()?;
        self.shared
            .deps
            .control
            .hold(&identity, &self.shared.options.repo, now_ms)
    }

    /// The scheduler's gates of this repository at `now_ms`.
    pub(super) fn gates(&self, now_ms: i64) -> bosun_core::Gates {
        match self.identity_name() {
            Some(identity) => {
                self.shared
                    .deps
                    .control
                    .gates(&identity, &self.shared.options.repo, now_ms)
            }
            None => bosun_core::Gates::default(),
        }
    }

    /// The control state changed: the kill switch kills the child in flight and parks the
    /// item; a lifted hold wakes a parked item.
    pub(super) async fn on_control(&mut self) {
        let now = self.shared.deps.clock.wall_ms();
        let hold = self.hold(now);
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        // Mutant (`mutant_control_pause_kills`): a pause stops the session like the kill switch.
        let kills = hold == Some(Hold::Kill)
            || (cfg!(mutant_control_pause_kills) && hold == Some(Hold::Pause));
        match cur.item.state {
            State::Running | State::Verifying if kills => {
                self.apply(Event::KillSwitch).await;
            }
            // The claim's own child (the verdict on the base commit) is killed; the claim ends
            // without a board write and the card goes back to Ready.
            State::Claimed if kills => {
                if let Some(task) = cur.task.as_ref() {
                    let _ = task.kill.send(Signal::Kill);
                }
            }
            State::Parked(_) if hold.is_none() => self.apply(Event::Tick).await,
            _ => {}
        }
    }

    /// At every tick: the meter of the item in flight, and every few ticks the store's view of
    /// activity and usage.
    pub(super) async fn watch_tick(&mut self) {
        let clock = self.shared.deps.clock.clone();
        let now = clock.wall_ms();
        match (&self.current, &mut self.watch.meter) {
            (Some(cur), Some(meter)) if meter.item == cur.item.id => {
                meter.observe(cur.item.state, now);
            }
            (Some(cur), meter) => {
                *meter = Some(Meter {
                    item: cur.item.id.clone(),
                    start_ms: now,
                    parked_ms: 0,
                    parked_since: None,
                });
            }
            (None, meter) => *meter = None,
        }
        if !self.watch.due(clock.mono(), self.shared.options.poll) {
            return;
        }
        self.auto_pause(now).await;
        self.daily_cap(now).await;
        self.item_caps(now).await;
    }

    /// The automatic pause: an interactive session (one bosun did not spawn) with a message in
    /// the checkout within `daemon.auto_pause_idle` pauses the repository until it has been
    /// idle that long. Activity before a resume by hand does not count.
    async fn auto_pause(&mut self, now: i64) {
        let config = self.config();
        let repo_name = self.shared.options.repo.clone();
        let Some(repo) = config.repos.get(&repo_name) else {
            return;
        };
        let control = &self.shared.deps.control;
        let mut until = None;
        if config.daemon.auto_pause {
            let idle = i64::try_from(config.daemon.auto_pause_idle.as_millis()).unwrap_or(i64::MAX);
            let since = now.saturating_sub(idle);
            let resumed = control.resumed_at(&repo_name).unwrap_or(i64::MIN);
            match self
                .shared
                .deps
                .reader
                .recent_sessions(&repo.key, since)
                .await
            {
                Ok(sessions) => {
                    until = sessions
                        .iter()
                        // Mutant (`mutant_control_autopause_own_sessions`): bosun's own run
                        // sessions pause the repository too.
                        .filter(|s| !s.run || cfg!(mutant_control_autopause_own_sessions))
                        .map(|s| s.last_ms)
                        .filter(|&last| last > resumed)
                        .max()
                        .map(|last| last.saturating_add(idle))
                        .filter(|&t| now < t);
                }
                Err(e) => {
                    self.shared
                        .trouble(format!("the interactive sessions cannot be read: {e}"));
                    return;
                }
            }
        }
        if control.set_auto(&repo_name, until) {
            let state = if until.is_some() { "paused" } else { "running" };
            crate::log::log!(
                "bosund: repository {repo_name}: {}",
                if until.is_some() {
                    "an interactive session works in the checkout: paused"
                } else {
                    "the interactive session is idle: the automatic pause is lifted"
                }
            );
            self.shared.publish(EventBody::ControlChanged {
                state: state.to_owned(),
                repo: Some(repo_name),
            });
        }
    }

    /// The daily cap: the identity's eqv since local midnight against `budget.daily_eqv` less
    /// `budget.reserve_eqv` parks the queue until the next local midnight.
    async fn daily_cap(&mut self, now: i64) {
        let config = self.config();
        let Some(identity) = self.identity_name() else {
            return;
        };
        let control = &self.shared.deps.control;
        let Some(daily) = config.budget.daily_eqv else {
            control.set_daily(&identity, None);
            return;
        };
        let cap = daily.saturating_sub(config.budget.reserve_eqv);
        let midnight = bosun_proc::last_local_midnight(now);
        let used = match self
            .shared
            .deps
            .reader
            .bill(midnight, now.saturating_add(1))
            .await
        {
            Ok(bill) => whole(bill.eqv.tokens()),
            Err(e) => {
                self.shared
                    .trouble(format!("today's usage cannot be read: {e}"));
                return;
            }
        };
        let until = (used >= cap).then(|| bosun_proc::next_local_midnight(now));
        if control.set_daily(&identity, until)
            && let Some(until) = until
        {
            crate::log::log!(
                "bosund: identity {identity}: the daily cap is reached ({used} of {cap} eqv); \
                 the queue parks until local midnight"
            );
            self.shared.publish(EventBody::BudgetExceeded {
                cap: "daily_eqv".to_owned(),
                item: None,
                used: Wide(used),
                limit: Wide(cap),
                until_ms: Some(until),
            });
        }
    }

    /// The item's caps: the card's, else `[budget]`. Over one, the item is killed and goes to
    /// Needs you.
    async fn item_caps(&mut self, now: i64) {
        if cfg!(mutant_budget_ignore_item_cap) {
            // Mutant: no item is held to its caps.
            return;
        }
        let config = self.config();
        let Some(cur) = self.current.as_ref() else {
            return;
        };
        if !matches!(cur.item.state, State::Running | State::Verifying) {
            return;
        }
        let (Some(claimed), Some(meter)) = (cur.claimed.as_ref(), self.watch.meter.as_ref()) else {
            return;
        };
        let budget = claimed.snapshot.contract.budget;
        let wall_cap = budget.wall.or(config.budget.item_wall_clock);
        let eqv_cap = budget.eqv.or(config.budget.item_eqv);
        let item = cur.item.id.clone();
        let mut over = None;
        if let Some(cap) = wall_cap {
            let used = ms_u64(meter.active_ms(now));
            let limit = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX);
            if used >= limit {
                over = Some(("item_wall_clock", used, limit));
            }
        }
        if over.is_none()
            && let Some(limit) = eqv_cap
        {
            match self.item_eqv(&item, meter.start_ms).await {
                Ok(used) if used >= limit => over = Some(("item_eqv", used, limit)),
                Ok(_) => {}
                Err(e) => self
                    .shared
                    .trouble(format!("the usage of {item} cannot be read: {e}")),
            }
        }
        let Some((cap, used, limit)) = over else {
            return;
        };
        let unit = if cap == "item_eqv" { "eqv" } else { "ms" };
        self.shared.trouble(format!(
            "{item} is over its budget: {cap} {used} {unit} of {limit}; killed, Needs you"
        ));
        self.shared.publish(EventBody::BudgetExceeded {
            cap: cap.to_owned(),
            item: Some(item.to_string()),
            used: Wide(used),
            limit: Wide(limit),
            until_ms: None,
        });
        // Stored before the transition queues the comment that shows it.
        let figure = super::outbox::BudgetFigure {
            cap: cap.to_owned(),
            used,
            limit,
        };
        let note = Write::ItemNote {
            item,
            kind: super::outbox::NOTE_BUDGET.to_owned(),
            body: serde_json::to_string(&figure).unwrap_or_default(),
            at_ms: now,
        };
        if let Err(e) = self.shared.deps.writer.write(vec![note]).await {
            self.halt(&e.to_string());
            return;
        }
        self.apply(Event::BudgetExceeded).await;
    }

    /// The eqv of the item's sessions (sub-agents included) since `from_ms`.
    async fn item_eqv(&self, item: &ItemId, from_ms: i64) -> Result<u64, bosun_store::Error> {
        let reader = &self.shared.deps.reader;
        let sessions: Vec<String> = reader
            .runs(item)
            .await?
            .into_iter()
            .map(|r| r.session_key)
            .collect();
        if sessions.is_empty() {
            return Ok(0);
        }
        // A minute of slack for a clock set back a little.
        let (table, bills) = reader
            .bills(from_ms.saturating_sub(60_000), i64::MAX, By::Session)
            .await?;
        let mut cost = bosun_core::Cost::ZERO;
        for (group, bill) in bills {
            if let Group::Session { session, .. } = group
                && sessions.contains(&session)
            {
                cost = cost + bill.cost;
            }
        }
        Ok(whole(table.eqv(cost).tokens()))
    }
}

#[cfg(test)]
mod tests {
    use bosun_core::{Next, Park, ParkReason};

    use super::*;

    #[test]
    fn the_meter_leaves_out_time_parked() {
        let mut meter = Meter {
            item: ItemId::new("demo#1"),
            start_ms: 0,
            parked_ms: 0,
            parked_since: None,
        };
        let parked = State::Parked(Park {
            reason: ParkReason::Limit,
            until_ms: None,
            next: Next::Resume,
        });
        meter.observe(State::Running, 10);
        meter.observe(parked, 100);
        assert_eq!(meter.active_ms(1_000), 100);
        meter.observe(parked, 500);
        meter.observe(State::Running, 1_100);
        assert_eq!(meter.active_ms(1_200), 200);
    }
}
