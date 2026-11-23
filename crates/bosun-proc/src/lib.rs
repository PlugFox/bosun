//! Child processes (#7, #50): child tracking, tree kill, reaping, the power request and the
//! security descriptor of the local endpoint. Containment back ends and the session shim come
//! in stage 2 (#8).
//!
//! Empty until #50.
//!
//! The only crate where `unsafe` is allowed (#12): every block carries its justification in a
//! `// SAFETY:` comment. `cargo xtask check-deps` fails when any other crate lifts the
//! workspace's `unsafe_code = "deny"`.
#![allow(unsafe_code)]
