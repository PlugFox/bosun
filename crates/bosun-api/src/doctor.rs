//! `doctor.probe` (#131): what `bosun doctor` asks the daemon itself, so that a check of the
//! daemon's own ability to start children runs in the daemon's process, not the CLI's.
//!
//! A daemon whose console host was ended can stay alive and keep its endpoint while every
//! console child it starts fails at load; a `gh` run from the CLI then succeeds and says
//! nothing about it. The probe starts the daemon's `gh` once (`gh --version`, which reads no
//! configuration and no token) and reports whether it ran.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Result of `doctor.probe`. An outcome of the child, whatever it is, is a result, not an API
/// error: the only error is an identity the daemon does not serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    pub identity: String,
    /// The program the daemon ran, as it was given to the OS.
    pub program: String,
    /// Whether the program started, exited zero and answered within the limit.
    pub ok: bool,
    /// When `ok`, the first line of its standard output; else why not: the OS error when it
    /// could not be started, the exit code and the end of its standard error when it exited
    /// non-zero, or that it did not answer within the limit.
    pub detail: String,
}
