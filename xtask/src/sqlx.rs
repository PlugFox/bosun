//! Regenerates or checks the committed sqlx offline cache, `.sqlx/` (#12, #19).
//!
//! The query macros are checked against a scratch database that the embedded migrations of
//! `bosun-store` build from nothing, so the cache describes exactly the schema the binary
//! ships. Needs the sqlx CLI pinned in `mise.toml` (`cargo sqlx`).

use std::path::{Path, PathBuf};
use std::process::Command;

const MIGRATIONS: &str = "crates/bosun-store/migrations";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn cargo_sqlx(root: &Path, url: &str, args: &[&str]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .arg("sqlx")
        .args(args)
        .current_dir(root)
        .env("DATABASE_URL", url)
        .env("SQLX_OFFLINE", "false")
        .status()
        .map_err(|e| format!("cannot run cargo sqlx: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo sqlx {} failed ({status})", args.join(" ")))
    }
}

/// `sqlx-prepare [--check]`: rebuild the scratch database, then write `.sqlx/` or, with
/// `--check`, fail when it is not what the queries and migrations produce.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask sqlx-prepare [--check]".to_owned()),
    };
    let root = workspace_root();
    let dir = root.join("target").join("sqlx-prepare");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // sqlx takes everything after `sqlite:` up to `?` as the file name; forward slashes keep a
    // Windows path valid in it.
    let db = dir
        .join("prepare.db")
        .display()
        .to_string()
        .replace('\\', "/");
    let url = format!("sqlite:{db}?mode=rwc");

    cargo_sqlx(&root, &url, &["database", "setup", "--source", MIGRATIONS])?;
    let mut prepare = vec!["prepare", "--workspace"];
    if check {
        prepare.push("--check");
    }
    // Tests hold queries too.
    prepare.extend(["--", "--all-targets"]);
    cargo_sqlx(&root, &url, &prepare)
}
