//! Identities to ingests: one store and one transcript directory per identity (#2, rule 4).

use std::path::PathBuf;

use bosun_config::Config;

/// What one identity's ingest reads and where it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The identity's name in the configuration.
    pub identity: String,
    /// Claude Code's transcript directory: `<claude_config_dir>/projects`.
    pub claude_projects: PathBuf,
    /// The identity's own database file.
    pub database: PathBuf,
    /// The roots of the repositories bound to this identity, as configured.
    pub repo_roots: Vec<String>,
}

/// One plan per configured identity. An identity reads only its own configuration directory
/// and writes only its own database, so no row ever crosses identities.
#[must_use]
pub fn plans(config: &Config) -> Vec<Plan> {
    let first = config.identities.values().next();
    config
        .identities
        .iter()
        .map(|(name, identity)| {
            let dir = if cfg!(mutant_ingest_cross_identity) {
                // Mutant: every identity discovers the first identity's directory.
                first.map_or(&identity.claude_config_dir, |f| &f.claude_config_dir)
            } else {
                &identity.claude_config_dir
            };
            Plan {
                identity: name.clone(),
                claude_projects: dir.join("projects"),
                database: identity.database.clone(),
                repo_roots: config
                    .repos
                    .values()
                    .filter(|repo| repo.identity == *name)
                    .map(|repo| repo.path.display().to_string())
                    .collect(),
            }
        })
        .collect()
}
