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
//!
//! [`detect`] runs the detectors of heat (#11 D) over the same rows: deterministic rules, each
//! with a threshold and a named remedy class, silent below a minimum sample.
//!
//! [`reconcile`] turns one evaluation's findings into the writes of heat's proposals: a
//! finding is a proposal at the second evaluation in a row, deduplicated, cooled down and
//! rate-limited. [`evaluate`] reads one repository's window and returns that batch; the daemon
//! writes it.

mod curve;
mod detect;
mod estimate;
mod evaluate;
mod exact;
mod items;
mod metrics;
mod reconcile;

pub use detect::{
    Detection, Detector, Evidence, FILE_READ_ACROSS_RUNS, FILE_REREAD_IN_SESSION, FileEvidence,
    Finding, Insufficient, Thresholds, UNCHANGED_FILE_READ_BYTES, detect,
};
pub use estimate::{BYTES_PER_TOKEN, estimates};
pub use evaluate::{Evaluation, WINDOW_MS, evaluate};
pub use exact::{Scope, exact};
pub use items::item_row;
pub use metrics::{Exactness, Inputs, Metric, heat_metrics, metrics};
pub use reconcile::{EVIDENCE_CAP, MAX_ACTIVE, MAX_NEW, Reconciled, Summary, reconcile};
