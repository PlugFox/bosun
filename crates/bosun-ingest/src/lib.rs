//! Transcript ingest: provider parsers that turn transcript lines into the event model of
//! `bosun-core` (#3, #20).
//!
//! This crate parses; it does not watch, tail or store. A tailer (#21) hands a parser the bytes
//! it has read past its stored offset, [`parse_lines`] takes every complete line of them and
//! says where it stopped, and the store folds the records with [`bosun_core::merge`].
//!
//! Each provider is a module ([`claude`], and [`stub`] for the synthetic second provider) that
//! implements [`LineParser`] and emits only the provider-neutral types of [`Record`], so a
//! second provider adds a module, not a field.

mod assemble;
pub mod claude;
pub mod git;
mod line;
mod normalise;
mod record;
pub mod stub;
mod time;

#[cfg(test)]
mod testing;

pub use assemble::Assembly;
pub use line::{Batch, LineParser, Outcome, ParsedLine, parse_lines};
pub use normalise::{absolute, command_head, style_of};
pub use record::{
    FileTouch, ModelTotals, Record, SessionCheck, Source, ToolResult, TouchKind, WindowMeta,
};
