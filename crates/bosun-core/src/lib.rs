//! Domain types of bosun: the provider-neutral event model, the merge rule, path
//! canonicalisation, prices and the bill (#3, #17); the item and attempt state machines, the
//! scheduler and the crash recovery plan of the loop (#7, #49).
//!
//! This crate performs no I/O: no async runtime, no database, no HTTP client. The rule is
//! enforced by `cargo xtask check-deps` (#12), not by convention.

mod attempt;
mod commit;
mod event;
mod item;
mod merge;
mod path;
mod price;
mod recover;
mod schedule;

pub use attempt::{Attempt, AttemptEvent, AttemptState, RunResult};
pub use commit::Commit;
pub use event::{
    ContextWindow, FileTouch, Gap, Message, ModelTotals, Origin, SessionCheck, Tokens, ToolCall,
    ToolResult, TouchKind, Usage, WindowKey, WindowKind, WindowMeta,
};
pub use item::{
    Actor, Column, Ctx, Effect, EffectKey, EffectKind, Event, Exit, Guard, Hold, Item, ItemId,
    Next, Outcome, Park, ParkReason, Reason, Rules, SpawnMode, State, Stepped, Transition,
    TransitionKey, UserCommand, step,
};
pub use merge::{KeyMismatch, merge};
pub use path::{PathStyle, canonical_path, repo_root, strip_root, style_of};
pub use price::{
    Cost, DEFAULT_REFERENCE_MODEL, Eqv, PriceRow, PriceTable, Pricing, Rates, SYNTHETIC_MODEL,
    TableError, UnknownModel, default_table,
};
pub use recover::{Action, Child, Facts, recover};
pub use schedule::{Card, Gates, Plan, plan};
