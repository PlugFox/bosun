//! The `unsafe` rule of #12: denied in every workspace crate but `bosun-proc` (#7, D2).
//!
//! The workspace lints deny `unsafe_code`, and every member inherits them. Cargo cannot override
//! one workspace lint per member, so `bosun-proc` lifts the lint with a crate attribute. This
//! check makes the deny hold everywhere else: the root manifest denies (or forbids) the lint,
//! every member inherits the workspace lints, and no source file of a member other than
//! `bosun-proc` lowers the lint with an `allow`, `expect` or `warn` attribute. With those three,
//! `unsafe` in any other crate fails its build.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use cargo_metadata::MetadataCommand;
use regex::Regex;

/// The one crate that may lift the lint.
const UNSAFE_ALLOWED: &str = "bosun-proc";

/// `unsafe_code = "deny"` (or `"forbid"`) on a line of its own, as in `[workspace.lints.rust]`.
static ROOT_DENIES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*unsafe_code\s*=\s*"(?:deny|forbid)"\s*$"#).expect("valid regex")
});

/// A `[lints]` table whose first key is `workspace = true`.
static INHERITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\[lints\][ \t]*\r?\n\s*workspace\s*=\s*true\b").expect("valid regex")
});

/// A lint attribute that lowers `unsafe_code`, alone or in a list, inside `cfg_attr` or not.
static LOWERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:allow|expect|warn)\s*\([^)]*\bunsafe_code\b").expect("valid regex")
});

/// A workspace member as the rule sees it.
#[derive(Debug)]
pub(crate) struct Member {
    pub(crate) name: String,
    /// The text of its `Cargo.toml`.
    pub(crate) manifest: String,
    /// Its `.rs` files, by path, with their text.
    pub(crate) sources: Vec<(PathBuf, String)>,
}

/// Every violation of the rule, one line each. Empty means the rule holds.
pub(crate) fn violations(root_manifest: &str, members: &[Member]) -> Vec<String> {
    let mut out = Vec::new();
    if !ROOT_DENIES.is_match(root_manifest) {
        out.push(
            "the root manifest does not deny unsafe_code in [workspace.lints.rust]".to_owned(),
        );
    }
    for member in members {
        if !INHERITS.is_match(&member.manifest) {
            out.push(format!(
                "{} does not inherit the workspace lints ([lints] workspace = true)",
                member.name
            ));
        }
        if member.name == UNSAFE_ALLOWED {
            continue;
        }
        for (path, text) in &member.sources {
            if LOWERS.is_match(text) {
                out.push(format!(
                    "{} lowers unsafe_code in {}; only {UNSAFE_ALLOWED} may",
                    member.name,
                    path.display()
                ));
            }
        }
    }
    out
}

fn sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            sources(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((path, text));
        }
    }
    Ok(())
}

fn load() -> Result<(String, Vec<Member>), String> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .map_err(|e| format!("cargo metadata failed: {e}"))?;
    let root = metadata.workspace_root.join("Cargo.toml");
    let root_manifest = std::fs::read_to_string(&root).map_err(|e| format!("{root}: {e}"))?;
    let mut members = Vec::new();
    for package in metadata.workspace_packages() {
        let manifest = std::fs::read_to_string(&package.manifest_path)
            .map_err(|e| format!("{}: {e}", package.manifest_path))?;
        let mut files = Vec::new();
        if let Some(dir) = package.manifest_path.parent() {
            sources(dir.as_std_path(), &mut files)?;
        }
        members.push(Member {
            name: package.name.to_string(),
            manifest,
            sources: files,
        });
    }
    Ok((root_manifest, members))
}

/// Checks the rule on the workspace; part of `cargo xtask check-deps`.
pub(crate) fn run() -> Result<(), String> {
    let (root_manifest, members) = load()?;
    let found = violations(&root_manifest, &members);
    if found.is_empty() {
        println!("unsafe is denied outside {UNSAFE_ALLOWED}");
        return Ok(());
    }
    for line in &found {
        eprintln!("  {line}");
    }
    Err(format!("{} unsafe rule violation(s)", found.len()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Member, violations};

    const ROOT: &str = "[workspace.lints.rust]\nunsafe_code = \"deny\"\n";
    const INHERITING: &str = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";
    // Spelled in pieces so this file does not itself match the rule it tests.
    const LIFT: &str = concat!("#![", "allow", "(unsafe_code)]\n");
    const LIFT_IN_LIST: &str = concat!(
        "#[cfg_attr(unix, ",
        "expect",
        "(dead_code, unsafe_code))]\n"
    );

    fn member(name: &str, manifest: &str, source: &str) -> Member {
        Member {
            name: name.to_owned(),
            manifest: manifest.to_owned(),
            sources: vec![(PathBuf::from("src/lib.rs"), source.to_owned())],
        }
    }

    #[test]
    fn the_workspace_holds_the_rule() {
        let (root, members) = super::load().unwrap();
        assert!(members.iter().any(|m| m.name == "bosun-proc"));
        assert_eq!(violations(&root, &members), Vec::<String>::new());
    }

    #[test]
    fn only_bosun_proc_may_lift_the_lint() {
        let clean = [
            member("bosun-proc", INHERITING, LIFT),
            member("bosun-core", INHERITING, "//! No unsafe here.\n"),
        ];
        assert_eq!(violations(ROOT, &clean), Vec::<String>::new());

        for lift in [LIFT, LIFT_IN_LIST] {
            for name in ["bosun-core", "bosun-runner", "bosund", "xtask"] {
                let found = violations(ROOT, &[member(name, INHERITING, lift)]);
                assert_eq!(found.len(), 1, "{name}: {found:?}");
                assert!(found[0].starts_with(&format!("{name} lowers unsafe_code")));
            }
        }
    }

    #[test]
    fn every_member_inherits_the_workspace_lints() {
        for manifest in [
            "[package]\nname = \"x\"\n",
            "[lints.rust]\nunsafe_code = \"allow\"\n",
        ] {
            for name in ["bosun-proc", "bosun-queue"] {
                let found = violations(ROOT, &[member(name, manifest, "")]);
                assert_eq!(found.len(), 1, "{name}: {found:?}");
                assert!(found[0].contains("does not inherit the workspace lints"));
            }
        }
    }

    #[test]
    fn the_root_manifest_denies_unsafe() {
        let members = [member("bosun-core", INHERITING, "")];
        for root in [
            "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
            "[workspace.lints.rust]\r\nunsafe_code = \"deny\"\r\n",
        ] {
            assert_eq!(violations(root, &members), Vec::<String>::new(), "{root:?}");
        }
        for root in [
            "[workspace.lints.rust]\n",
            "[workspace.lints.rust]\nunsafe_code = \"warn\"\n",
        ] {
            assert_eq!(violations(root, &members).len(), 1, "{root:?}");
        }
    }
}
