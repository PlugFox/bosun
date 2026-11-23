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
//! The definitions are those of the `bosun_api::Report` and `bosun_api::Estimates` types; this
//! crate computes them from the rows `bosun-store` reads and holds no SQL of its own. The
//! detectors of stage 5 come later.

mod curve;
mod estimate;
mod exact;

pub use estimate::{BYTES_PER_TOKEN, estimates};
pub use exact::{Scope, exact};
