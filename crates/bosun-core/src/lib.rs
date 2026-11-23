//! Domain types of bosun: the provider-neutral event model, the merge rule, path
//! canonicalisation, prices and the bill (#3, #17).
//!
//! This crate performs no I/O: no async runtime, no database, no HTTP client. The rule is
//! enforced by `cargo xtask check-deps` (#12), not by convention.

mod commit;
mod event;
mod merge;
mod path;
mod price;

pub use commit::Commit;
pub use event::{
    ContextWindow, FileTouch, Gap, Message, ModelTotals, Origin, SessionCheck, Tokens, ToolCall,
    ToolResult, TouchKind, Usage, WindowKey, WindowKind, WindowMeta,
};
pub use merge::{KeyMismatch, merge};
pub use path::{PathStyle, canonical_path, repo_root, strip_root, style_of};
pub use price::{
    Cost, DEFAULT_REFERENCE_MODEL, Eqv, PriceRow, PriceTable, Pricing, Rates, SYNTHETIC_MODEL,
    TableError, UnknownModel, default_table,
};
