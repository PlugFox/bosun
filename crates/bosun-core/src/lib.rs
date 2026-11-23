//! Domain types, state machines and the scheduler of bosun.
//!
//! This crate performs no I/O: no async runtime, no database, no HTTP client. The rule is
//! enforced by `cargo xtask check-deps` (#12), not by convention.
