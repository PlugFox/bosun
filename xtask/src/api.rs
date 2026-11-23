//! `gen-api`: the JSON Schema half of the contract job (#12, #24).
//!
//! The schema is generated from the protocol types of `bosun-api` and committed at
//! `schema/bosun-api.schema.json`. `--check` fails when the committed file is not what the
//! types produce, so a protocol type changed without regenerating fails CI.

use std::path::PathBuf;

use crate::mutants::workspace_root;

/// The committed schema, relative to the workspace root.
pub(crate) const SCHEMA: &str = "schema/bosun-api.schema.json";

/// The schema as committed: pretty JSON, `\n` line endings, a final newline.
pub(crate) fn render() -> String {
    let mut text =
        serde_json::to_string_pretty(&bosun_api::schema()).expect("a JSON value always serialises");
    text.push('\n');
    text
}

fn path() -> PathBuf {
    workspace_root().join(SCHEMA)
}

/// `gen-api [--check]`.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask gen-api [--check]".to_owned()),
    };
    let path = path();
    let expected = render();
    if check {
        let found = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}; run `cargo xtask gen-api`", path.display()))?;
        // A checkout with CRLF line endings holds the same schema.
        if found.replace("\r\n", "\n") != expected {
            return Err(format!(
                "{SCHEMA} does not match the protocol types of bosun-api; run `cargo xtask \
                 gen-api` and commit the result (an incompatible change also bumps \
                 bosun_api::API_VERSION, #4)"
            ));
        }
        println!("{SCHEMA} is current");
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, expected).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {SCHEMA}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_stable_and_ends_with_a_newline() {
        let text = render();
        assert_eq!(text, render());
        assert!(text.ends_with("}\n") && !text.contains('\r'));
    }
}
