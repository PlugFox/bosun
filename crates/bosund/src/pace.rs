//! `tokens.pace` (#4, #74): how fast an identity spends, its odometers of today and of this run
//! of the daemon, and the reference pace from the configured budget.
//!
//! Until limit windows are measured (#10) the reference is the configured daily cap
//! (`budget.daily_eqv` less `budget.reserve_eqv`), the same cap the loop parks the queue on:
//! what is left of it today, spread evenly until local midnight. The answer names that source
//! ([`PaceReference::Configured`]).

use bosun_api::rpc::{ErrorObject, code};
use bosun_api::{Method, Pace, PaceReference, Wide, method};
use bosun_core::Cost;
use serde_json::Value;

use crate::api::{call, failed};
use crate::overview::now_ms;
use crate::state::{Served, State};

/// The width of one bucket of the sparkline.
pub(crate) const BUCKET_MS: i64 = 60_000;

/// The buckets of the sparkline: the last hour.
pub(crate) const BUCKETS: usize = 60;

/// `tokens.pace`.
pub(crate) async fn dispatch(state: &State, name: &str, p: Value) -> Result<Value, ErrorObject> {
    match name {
        method::TokensPace::NAME => {
            call::<method::TokensPace, _, _>(p, |p| async move {
                pace(state, state.identity(&p.identity)?, now_ms()).await
            })
            .await
        }
        _ => Err(ErrorObject::new(
            code::METHOD_NOT_FOUND,
            format!("no method {name}"),
        )),
    }
}

async fn pace(state: &State, served: &Served, now: i64) -> Result<Pace, ErrorObject> {
    let reader = served.store.reader();
    let day_start = bosun_proc::last_local_midnight(now);
    let reset = bosun_proc::next_local_midnight(now);
    let end = now.saturating_add(1);
    let today = reader.bill(day_start, end).await.map_err(failed)?;
    let run = reader.bill(state.started_ms, end).await.map_err(failed)?;
    let span = BUCKET_MS * i64::try_from(BUCKETS).unwrap_or(i64::MAX);
    let from = end.saturating_sub(span);
    let (table, rows) = reader.usage(from, end).await.map_err(failed)?;
    let mut costs = vec![Cost::ZERO; BUCKETS];
    for row in &rows {
        // An unpriced message is in no eqv (#3), here as in the odometers.
        if let Ok(cost) = table.cost(&row.model, row.ts_ms, &row.tokens) {
            let at = usize::try_from((row.ts_ms - from) / BUCKET_MS).unwrap_or(0);
            let bucket = &mut costs[at.min(BUCKETS - 1)];
            *bucket = *bucket + cost;
        }
    }
    let today_eqv = today.eqv.tokens();
    let budget = served.budget();
    let reference = budget.daily_eqv.map(|daily| {
        let cap = daily.saturating_sub(budget.reserve_eqv);
        #[expect(clippy::cast_precision_loss, reason = "eqv is shown, not summed")]
        let left = (cap as f64 - today_eqv).max(0.0);
        #[expect(clippy::cast_precision_loss, reason = "minutes to midnight")]
        let minutes = ((reset - now).max(1) as f64) / 60_000.0;
        PaceReference::Configured {
            daily_eqv: Wide(daily),
            reserve_eqv: Wide(budget.reserve_eqv),
            cap_eqv: Wide(cap),
            eqv_per_minute: left / minutes,
        }
    });
    Ok(Pace {
        identity: served.name.clone(),
        now_ms: now,
        reference_model: table.reference_model().to_owned(),
        day_start_ms: day_start,
        reset_ms: reset,
        today_eqv,
        run_start_ms: state.started_ms,
        run_eqv: run.eqv.tokens(),
        bucket_ms: BUCKET_MS.unsigned_abs(),
        buckets: costs.into_iter().map(|c| table.eqv(c).tokens()).collect(),
        reference,
    })
}
