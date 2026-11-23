//! The mutant job of #12.
//!
//! A mutant is a check that must go red when a mechanism is disabled. Each one is a cfg
//! (`--cfg mutant_<name>`, never a cargo feature) plus a cargo command. The job runs the command
//! twice: without the cfg it must pass, with the cfg it must fail. A mutant whose command stays
//! green fails the job, and so does a baseline that is already red.
//!
//! Every name registered here is also declared in the workspace's `unexpected_cfgs` lint.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Mutant {
    /// The cfg name, `mutant_<what is broken>`.
    name: &'static str,
    /// The issue whose acceptance list names this mutant.
    issue: u32,
    /// What the cfg breaks.
    breaks: &'static str,
    /// Arguments to `cargo` for the check that must go red.
    check: &'static [&'static str],
}

const MUTANTS: &[Mutant] = &[Mutant {
    name: "mutant_core_async_runtime",
    issue: 12,
    breaks: "bosun-core depends on the async runtime",
    check: &["xtask", "check-deps"],
}];

/// Runs the mutant's check, with its cfg when `mutated`. Returns whether the check passed.
fn check_passes(mutant: &Mutant, mutated: bool) -> Result<bool, String> {
    let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let label = if mutated {
        flags.push_str(" --cfg ");
        flags.push_str(mutant.name);
        mutant.name
    } else {
        "baseline"
    };
    // One target directory per cfg, so that the changed RUSTFLAGS do not invalidate the main
    // build cache.
    let target_dir = workspace_root().join("target").join("mutants").join(label);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .args(mutant.check)
        .current_dir(workspace_root())
        .env("RUSTFLAGS", flags.trim())
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    Ok(status.success())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

pub(crate) fn run(only: &[String]) -> Result<(), String> {
    let selected: Vec<&Mutant> = MUTANTS
        .iter()
        .filter(|m| only.is_empty() || only.iter().any(|o| o == m.name))
        .collect();
    if selected.is_empty() {
        return Err(format!("no mutant matches {only:?}"));
    }

    let mut failures = Vec::new();
    for mutant in selected {
        println!("== {} (#{}): {}", mutant.name, mutant.issue, mutant.breaks);
        if !check_passes(mutant, false)? {
            failures.push(format!(
                "{}: the check is red without the mutant",
                mutant.name
            ));
        } else if check_passes(mutant, true)? {
            failures.push(format!(
                "{}: the check stays green with the mutant",
                mutant.name
            ));
        } else {
            println!("== {}: red as expected", mutant.name);
        }
    }

    if failures.is_empty() {
        return Ok(());
    }
    for line in &failures {
        eprintln!("  {line}");
    }
    Err(format!("{} mutant(s) failed", failures.len()))
}
