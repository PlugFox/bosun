# bosun

A per-user daemon that runs coding agents unattended against a queue of fully specified tasks,
decides for itself whether each task is done, and accounts for every token spent.

**Status: design.** Nothing here runs yet. The design lives in the issues below; code lands
stage by stage.

## What it does

- Takes tasks from a board (GitHub Projects v2 first). You move a fully specified card to
  *Ready*; bosun moves it forward to *Done* or back to *Needs you* with the reason.
- Runs the agent headless (Claude Code first; Codex and others behind the same trait), one
  fresh session per attempt. The loop lives in the daemon, not in the agent.
- Decides "done" by running the task's own verdict command from a snapshotted definition. The
  agent never grades its own work.
- Ingests every session transcript, interactive ones included, and reports the real weighted
  bill per item, per commit and per file.
- Keeps identities apart: a personal and a work identity never share a token, an account, a
  database or a notification channel.
- Is a headless core with one local API. The TUI, the CLI, the web dashboard and the MCP server
  are all clients of it.

## What it is not

- Not a system service: it is a logon process of the user whose credentials it uses.
- Not a hosted product and not multi-user.
- Not a way around provider limits: one identity has one account per provider, and bosun never
  fails over between accounts.

## Design

| Topic | Issue |
| --- | --- |
| Vision, architecture, glossary, staging | #1 |
| Identity isolation and security | #2 |
| Storage schema and accounting | #3 |
| Local API, CLI, TUI, configuration | #4 |
| Queue trait and the Projects v2 backend | #5 |
| Monorepo layout, shared API types, toolchain, CI | #12 |

## Repository

A monorepo. `crates/` is a Cargo workspace: the daemon (`bosund`), the protocol types every
client shares (`bosun-api`), storage, queue backends, runners, the CLI (`bosun`) and the TUI
(`bosun tui`). `web/` is the TypeScript + SolidJS dashboard, built separately and embedded into
the daemon's release binary; its API types are generated from `bosun-api`. A plain
`cargo build` needs only the pinned Rust toolchain.

## Stages

| Stage | Delivers | Issue |
| --- | --- | --- |
| 0 | Accounting only: transcripts into the database, the correct bill | #6 |
| 1 | The loop: board, headless sessions, verdict, TUI and CLI | #7 |
| 2 | Worktrees, resource leases, process cleanup | #8 |
| 3 | Web dashboard, Telegram and webhook notifications | #9 |
| 4 | Limits across providers, more runners, many repositories | #10 |
| 5 | MCP, device thermals, propose-only supervisor, heat proposals | #11 |

## Platforms

Windows, macOS and Linux. Windows is the development machine, so it is the first platform a
feature must work on, not the last.

## Licence

MIT OR Apache-2.0.
