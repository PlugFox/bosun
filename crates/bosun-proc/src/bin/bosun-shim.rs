//! `bosun-shim <spec>`: the session shim of #8 (#129). The logic, and its documentation, is
//! [`bosun_proc::shim`]; this is the entry point.
//!
//! Exits with the child's code. When the shim itself fails (a bad spec, a child that cannot
//! start, a record that cannot be written) it reports on stderr and exits with
//! [`SHIM_FAILED`].

use std::path::PathBuf;

/// The exit code of a shim that failed before or around its child, as `env` and `nohup` use
/// for "the command could not be run".
const SHIM_FAILED: i32 = 125;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(spec), None) = (args.next(), args.next()) else {
        eprintln!("usage: bosun-shim <spec>");
        std::process::exit(SHIM_FAILED);
    };
    match bosun_proc::shim::run(&PathBuf::from(spec)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("bosun-shim: {e}");
            std::process::exit(SHIM_FAILED);
        }
    }
}
