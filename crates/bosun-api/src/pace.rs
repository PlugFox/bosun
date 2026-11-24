//! `tokens.pace` (#4, #74): what the Tokens tab shows of one identity: how fast it spends, its
//! odometers and the reference pace.
//!
//! Until limit windows are measured (#10) the reference pace comes from the configured daily
//! budget: the pace that spends what is left of `budget.daily_eqv` less `budget.reserve_eqv`
//! exactly at local midnight, when the daily cap resets. [`PaceReference`] says where the
//! reference came from, so a client can say it is configured, not measured.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Wide;

/// Params of `tokens.pace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PaceParams {
    pub identity: String,
}

/// Result of `tokens.pace`. Every eqv is of the identity's messages only (#2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Pace {
    pub identity: String,
    /// The daemon's clock when it answered.
    pub now_ms: i64,
    /// The model every eqv is measured against.
    pub reference_model: String,
    /// The last local midnight: where today's odometer starts.
    pub day_start_ms: i64,
    /// The next local midnight: when the daily cap resets.
    pub reset_ms: i64,
    /// Eqv since `day_start_ms`.
    pub today_eqv: f64,
    /// When the daemon started: where this run's odometer starts.
    pub run_start_ms: i64,
    /// Eqv since `run_start_ms`.
    pub run_eqv: f64,
    /// The width of each of [`Pace::buckets`].
    pub bucket_ms: u64,
    /// Eqv per bucket, oldest first; the last bucket ends at `now_ms`.
    pub buckets: Vec<f64>,
    /// The pace the speed is measured against; absent when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<PaceReference>,
}

/// Where the reference pace came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaceReference {
    /// From the configuration: what is left of the daily cap, spread evenly until it resets.
    Configured {
        /// `budget.daily_eqv`.
        daily_eqv: Wide,
        /// `budget.reserve_eqv`.
        reserve_eqv: Wide,
        /// The cap: `daily_eqv - reserve_eqv`.
        cap_eqv: Wide,
        /// `max(cap - today, 0)` per minute until `reset_ms`.
        eqv_per_minute: f64,
    },
}
