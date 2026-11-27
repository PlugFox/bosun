//! Metrics and the report (#6, #12, #26, #27).
//!
//! The report has two parts that never mix:
//!
//! - [`exact`]: every number is a raw token count, a sum of them, or a cost and eqv computed
//!   from them at query time with the stored prices (#3, "The bill"); the context curve is
//!   here, since prompt sizes are measured counts.
//! - [`estimates`]: the cost of tool results by tool and by file, repeated reads and the cost
//!   of finding, which guess a result's tokens from its bytes ([`BYTES_PER_TOKEN`]) and price
//!   its stay in the cache. Nothing estimated is added to an exact figure.
//!
//! The loop's sections (#72) follow the same split: an item's runs, with the commits linked to
//! them by the sessions' commit ranges, are exact; interactive sessions inferred for an item are
//! an estimate and never part of the item's runs (#3).
//!
//! The definitions are those of the `bosun_api::Report` and `bosun_api::Estimates` types; this
//! crate computes them from the rows `bosun-store` reads and holds no SQL of its own.
//!
//! [`metrics`] computes the generic metrics of heat (#11 D, `bosun_api::HeatMetrics`) per
//! repository, provider and session kind, each labelled exact or estimate and comparable
//! across providers or not; the detectors test them.

mod curve;
mod estimate;
mod exact;
mod items;
mod metrics;

pub use estimate::{BYTES_PER_TOKEN, estimates};
pub use exact::{Scope, exact};
pub use items::item_row;
pub use metrics::{Exactness, Inputs, Metric, heat_metrics, metrics};
