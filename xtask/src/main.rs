//! Repository tasks for bosun (#12). Run as `cargo xtask <task>`.

mod api;
mod deps;
mod fixtures;
mod mutants;
mod sqlx;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  check-deps          check the workspace dependency rules (#12)
  gen-api [--check]   write schema/bosun-api.schema.json from bosun-api, or check that it
                      is current (#12, #24)
  lint-fixtures [DIR] check that every id and path in fixtures/ (or DIR) is synthetic (#16)
  mutants [--shard K/N] [--platform-only] [NAME...]
                      run the registered mutants (or the named ones): each one's tests must
                      build, and its check must be green without it and red with it
  sqlx-prepare [--check]
                      regenerate the sqlx offline cache `.sqlx/` against a database built by
                      the store's migrations, or check that it is current (#19)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check-deps") => deps::run(),
        Some("gen-api") => api::run(&args[1..]),
        Some("lint-fixtures") => fixtures::run(&args[1..]),
        Some("mutants") => mutants::run(&args[1..]),
        Some("sqlx-prepare") => sqlx::run(&args[1..]),
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
