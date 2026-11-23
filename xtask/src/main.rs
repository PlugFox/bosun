//! Repository tasks for bosun (#12). Run as `cargo xtask <task>`.

mod deps;
mod mutants;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  check-deps          check the workspace dependency rules (#12)
  mutants [--shard K/N] [--platform-only] [NAME...]
                      run the registered mutants (or the named ones): each one's tests must
                      build, and its check must be green without it and red with it";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check-deps") => deps::run(),
        Some("mutants") => mutants::run(&args[1..]),
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
