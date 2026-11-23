//! Transcript ingest: discovery, tailing, rescan and reingest of each identity's transcripts,
//! and the provider parsers that turn their lines into the event model of `bosun-core` (#3,
//! #20, #21).
//!
//! - [`Ingest`] discovers one provider's transcripts in one identity's directory, tails each
//!   file from its stored offset and sends the rows of each read, with the file's new offset,
//!   to the identity's store as one batch. [`spawn`] runs it on a timer and on watch events,
//!   with live files ahead of the backfill and the backfill bounded in open files and bytes
//!   (#22).
//! - [`parse_lines`] takes every complete line of the bytes read past an offset and says where
//!   it stopped; the store folds the records with [`bosun_core::merge`].
//! - Each provider is a module ([`claude`], and [`stub`] for the synthetic second provider)
//!   that implements [`LineParser`] and [`Provider`] and emits only the provider-neutral types
//!   of [`Record`], so a second provider adds a module, not a field.
//! - [`plans`] maps the configuration to one ingest per identity (#2, rule 4).

mod assemble;
pub mod claude;
pub mod git;
mod identity;
mod ingest;
mod layout;
mod line;
mod meter;
mod normalise;
mod record;
mod schedule;
pub mod stub;
mod tail;
mod time;
mod watch;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use assemble::Assembly;
pub use identity::{Plan, plans};
pub use ingest::{Error, Ingest, Options, Report};
pub use layout::{FileKind, Located, Provider};
pub use line::{Batch, LineParser, Outcome, ParsedLine, parse_lines};
pub use meter::Stats;
pub use normalise::{absolute, command_head, style_of};
pub use record::{
    FileTouch, ModelTotals, Record, SessionCheck, Source, ToolResult, TouchKind, WindowMeta,
};
pub use schedule::{Handle, spawn};
