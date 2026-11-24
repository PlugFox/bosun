//! `bosun-shim SPEC`: runs the child of the spec file in a container, records it and exits with
//! the child's exit code. The spec and the record format are in `bosun_proc::shim` (#8).

use std::path::Path;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(spec), None) = (args.next(), args.next()) else {
        eprintln!("usage: bosun-shim <spec>");
        std::process::exit(2);
    };
    match bosun_proc::shim::run(Path::new(&spec)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("bosun-shim: {e}");
            std::process::exit(2);
        }
    }
}
