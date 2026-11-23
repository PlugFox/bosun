//! The workspace dependency rules of #12, enforced on the resolved graph.
//!
//! The graph comes from `cargo metadata --filter-platform <host>`, so a target-specific
//! dependency counts only when it is active, and `RUSTFLAGS=--cfg ...` is honoured: that is how
//! the mutant `mutant_core_async_runtime` reaches this check.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use cargo_metadata::{DependencyKind, MetadataCommand};

/// Crates that perform I/O. `bosun-core` may not reach any of them, directly or transitively.
const IO_CRATES: &[&str] = &[
    "async-std",
    "hyper",
    "mio",
    "reqwest",
    "rusqlite",
    "smol",
    "sqlx",
    "tokio",
    "ureq",
];

/// Clients of the local API (#4) and the only workspace crates they may depend on directly:
/// `bosun-config` so that `bosun doctor` works with the daemon down.
const CLIENTS: &[&str] = &["bosun-cli", "bosun-tui", "bosun-mcp"];
const CLIENT_ALLOWED: &[&str] = &["bosun-client", "bosun-api", "bosun-config"];

/// Crates a client may not reach even transitively: a client that can open the database is not
/// a client.
const CLIENT_FORBIDDEN: &[&str] = &["bosun-store", "bosun-queue", "bosun-runner", "bosund"];

/// Crates that may reach no workspace crate at all: the independent recount (#16) shares no
/// code with the parser it checks.
const STANDALONE: &[&str] = &["recount"];

/// The dependency graph of the workspace, normal and build edges only: dev-dependencies never
/// end up in a binary.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    /// Package name to the names of its direct dependencies.
    pub(crate) edges: BTreeMap<String, BTreeSet<String>>,
    /// Names of the workspace members.
    pub(crate) members: BTreeSet<String>,
}

impl Graph {
    fn direct(&self, name: &str) -> impl Iterator<Item = &str> {
        self.edges
            .get(name)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    fn reachable(&self, name: &str) -> BTreeSet<&str> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = self.direct(name).collect();
        while let Some(next) = stack.pop() {
            if seen.insert(next) {
                stack.extend(self.direct(next));
            }
        }
        seen
    }

    fn direct_members(&self, name: &str) -> impl Iterator<Item = &str> {
        self.direct(name).filter(|dep| self.members.contains(*dep))
    }
}

/// Every rule violation in the graph, one line each. Empty means the rules hold.
pub(crate) fn violations(graph: &Graph) -> Vec<String> {
    let mut out = Vec::new();

    if graph.members.contains("bosun-core") {
        for dep in graph.direct_members("bosun-core") {
            out.push(format!(
                "bosun-core depends on workspace crate {dep}; it may depend on none"
            ));
        }
        for dep in graph.reachable("bosun-core") {
            if IO_CRATES.contains(&dep) {
                out.push(format!(
                    "bosun-core reaches the I/O crate {dep}; bosun-core does no I/O"
                ));
            }
        }
    }

    if graph.members.contains("bosun-api") {
        for dep in graph.direct_members("bosun-api") {
            if dep != "bosun-core" {
                out.push(format!(
                    "bosun-api depends on workspace crate {dep}; it may depend on bosun-core only"
                ));
            }
        }
    }

    for client in CLIENTS
        .iter()
        .copied()
        .filter(|c| graph.members.contains(*c))
    {
        for dep in graph.direct_members(client) {
            if !CLIENT_ALLOWED.contains(&dep) {
                out.push(format!(
                    "{client} depends on workspace crate {dep}; a client may depend on {} only",
                    CLIENT_ALLOWED.join(", ")
                ));
            }
        }
        for dep in graph.reachable(client) {
            if CLIENT_FORBIDDEN.contains(&dep) {
                out.push(format!("{client} reaches {dep}; a client never links it"));
            }
        }
    }

    for standalone in STANDALONE
        .iter()
        .copied()
        .filter(|c| graph.members.contains(*c))
    {
        for dep in graph.reachable(standalone) {
            if graph.members.contains(dep) {
                out.push(format!(
                    "{standalone} reaches workspace crate {dep}; it may share no code with the \
                     workspace"
                ));
            }
        }
    }

    // Clients link `bosun-config`, so it may not reach what a client may not.
    if graph.members.contains("bosun-config") {
        for dep in graph.reachable("bosun-config") {
            if CLIENT_FORBIDDEN.contains(&dep) {
                out.push(format!(
                    "bosun-config reaches {dep}; clients link bosun-config, so it never links {dep}"
                ));
            }
        }
    }

    for member in &graph.members {
        if member != "bosund" && graph.direct(member).any(|dep| dep == "bosund") {
            out.push(format!(
                "{member} depends on bosund; only bosund links everything"
            ));
        }
    }

    out
}

fn host_triple() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| format!("cannot run rustc: {e}"))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV printed no host triple".to_owned())
}

fn load() -> Result<Graph, String> {
    let metadata = MetadataCommand::new()
        .other_options(vec!["--filter-platform".to_owned(), host_triple()?])
        .exec()
        .map_err(|e| format!("cargo metadata failed: {e}"))?;
    let resolve = metadata
        .resolve
        .as_ref()
        .ok_or("cargo metadata returned no resolve graph")?;

    let name_of: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|p| (&p.id, p.name.to_string()))
        .collect();
    let mut graph = Graph {
        members: metadata
            .workspace_members
            .iter()
            .map(|id| name_of[id].clone())
            .collect(),
        ..Graph::default()
    };
    for node in &resolve.nodes {
        let deps = node
            .deps
            .iter()
            .filter(|dep| {
                dep.dep_kinds
                    .iter()
                    .any(|k| matches!(k.kind, DependencyKind::Normal | DependencyKind::Build))
            })
            .map(|dep| name_of[&dep.pkg].clone())
            .collect();
        graph.edges.insert(name_of[&node.id].clone(), deps);
    }
    Ok(graph)
}

pub(crate) fn run() -> Result<(), String> {
    let graph = load()?;
    let found = violations(&graph);
    if found.is_empty() {
        println!(
            "dependency rules hold ({} workspace crates)",
            graph.members.len()
        );
        return Ok(());
    }
    for line in &found {
        eprintln!("  {line}");
    }
    Err(format!("{} dependency rule violation(s)", found.len()))
}

#[cfg(test)]
mod tests {
    use super::{Graph, violations};

    fn graph(members: &[&str], edges: &[(&str, &str)]) -> Graph {
        let mut g = Graph {
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            ..Graph::default()
        };
        for (from, to) in edges {
            g.edges
                .entry((*from).to_owned())
                .or_default()
                .insert((*to).to_owned());
        }
        g
    }

    #[test]
    fn a_clean_layout_passes() {
        let g = graph(
            &[
                "bosun-core",
                "bosun-api",
                "bosun-client",
                "bosun-cli",
                "bosun-store",
                "bosund",
            ],
            &[
                ("bosun-api", "bosun-core"),
                ("bosun-api", "serde"),
                ("bosun-client", "bosun-api"),
                ("bosun-client", "tokio"),
                ("bosun-cli", "bosun-client"),
                ("bosun-cli", "bosun-api"),
                ("bosund", "bosun-store"),
                ("bosun-store", "sqlx"),
            ],
        );
        assert_eq!(violations(&g), Vec::<String>::new());
    }

    #[test]
    fn core_may_not_reach_io_even_transitively() {
        let g = graph(
            &["bosun-core"],
            &[("bosun-core", "helper"), ("helper", "tokio")],
        );
        assert_eq!(violations(&g).len(), 1);
    }

    #[test]
    fn core_may_not_depend_on_a_workspace_crate() {
        let g = graph(&["bosun-core", "bosun-api"], &[("bosun-core", "bosun-api")]);
        let found = violations(&g);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn api_may_depend_on_core_only() {
        let g = graph(
            &["bosun-core", "bosun-api", "bosun-store"],
            &[("bosun-api", "bosun-store")],
        );
        assert_eq!(violations(&g).len(), 1);
    }

    #[test]
    fn a_client_may_not_reach_the_store() {
        let g = graph(
            &["bosun-client", "bosun-api", "bosun-tui", "bosun-store"],
            &[
                ("bosun-tui", "bosun-client"),
                ("bosun-client", "bosun-store"),
            ],
        );
        let found = violations(&g);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("bosun-tui reaches bosun-store"));
    }

    #[test]
    fn a_client_may_not_depend_on_other_workspace_crates() {
        let g = graph(&["bosun-cli", "bosun-heat"], &[("bosun-cli", "bosun-heat")]);
        assert_eq!(violations(&g).len(), 1);
    }

    #[test]
    fn recount_may_not_depend_on_a_workspace_crate() {
        let clean = graph(
            &["recount", "bosun-core"],
            &[("recount", "serde_json"), ("serde_json", "serde_core")],
        );
        assert_eq!(violations(&clean), Vec::<String>::new());

        let g = graph(
            &["recount", "bosun-core", "bosun-ingest"],
            &[("recount", "bosun-ingest"), ("bosun-ingest", "bosun-core")],
        );
        let found = violations(&g);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("recount reaches workspace crate bosun-core"));
    }

    #[test]
    fn a_client_may_depend_on_the_configuration() {
        let g = graph(
            &["bosun-core", "bosun-config", "bosun-cli", "bosun-tui"],
            &[
                ("bosun-config", "bosun-core"),
                ("bosun-config", "toml_edit"),
                ("bosun-cli", "bosun-config"),
                ("bosun-tui", "bosun-config"),
            ],
        );
        assert_eq!(violations(&g), Vec::<String>::new());
    }

    #[test]
    fn the_configuration_may_not_reach_what_a_client_may_not() {
        for forbidden in ["bosun-store", "bosun-queue", "bosun-runner", "bosund"] {
            let g = graph(
                &["bosun-config", "bosun-heat", forbidden],
                &[("bosun-config", "bosun-heat"), ("bosun-heat", forbidden)],
            );
            let found = violations(&g);
            assert!(
                found
                    .iter()
                    .any(|v| v.starts_with(&format!("bosun-config reaches {forbidden}"))),
                "{forbidden}: {found:?}"
            );
        }
    }

    #[test]
    fn nothing_depends_on_the_daemon() {
        let g = graph(&["bosund", "bosun-heat"], &[("bosun-heat", "bosund")]);
        assert_eq!(violations(&g).len(), 1);
    }
}
