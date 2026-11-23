# bosun

A per-user daemon that runs coding agents unattended against a kanban queue of fully specified
tasks, across repositories, providers (Claude, Codex, agy) and identities (personal and work).
Rust monorepo: the daemon core, the CLI and the ratatui TUI first; the TypeScript + SolidJS web
dashboard and the MCP server later.

Talk to the user in Russian. Code, comments, docs, issues, commits: English.

**Status: design.** The design is the GitHub issues, not files in this tree: #1 is the vision and
the architecture (pinned), #2–#5 and #12 are the cross-cutting contracts, #6–#11 are stages 0–5.
Read the issue a task touches (`gh issue view N`) before changing code; when a decision changes,
edit the issue body or comment on it, so the issues stay the single copy of the design.

The work queue is the private board "AI" (a GitHub Projects v2 user project). Columns: Backlog,
Ready (only the user moves cards here), In progress, Verify, Done, Needs you.

## Rules
- The repository is public: no real transcripts, paths, issue text from other repositories,
  tokens, or personal data in code, fixtures or docs. Fixtures are synthetic or scrubbed.
- Identity isolation is a design invariant (#2), and it applies to working on bosun too: act on
  GitHub only as the identity that owns this repository (see CLAUDE.local.md).
- A green check is green for the wrong reason until you have watched it go red: every acceptance
  item has a mutant (see the issues).
- Consult Fable for architecture and plan decisions; verify what it claims.
- Windows is the development machine and the first platform a feature must work on.
