//! Metrics and the report (#6, #12, #26).
//!
//! Stage 0 holds the exact sections of the report ([`exact`]): every number is a raw token
//! count, a sum of them, or a cost and eqv computed from them at query time with the stored
//! prices (#3, "The bill"). The definitions are those of the `bosun_api::Report` types; this
//! crate computes them from the rows `bosun-store` reads and holds no SQL of its own.
//! Estimates (#27) and the detectors of stage 5 come later.

mod exact;

pub use exact::{Scope, exact};
