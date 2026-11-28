# Rules of an unattended card

A card on the board "AI" whose `## Rules` section says "The rules of `docs/unattended.md`
apply" runs under the rules below, as they stand at the commit its session starts from. The
card's own `## Rules` lines come after these and win over them. Before 2026-11-28 every card
carried these lines itself; one file means a change of a rule (the registration of mutants, for
example) is one commit, not an edit of every queued card.

- You cannot read GitHub from the session (`gh` answers 401 there by design, #139): everything the card depends on is quoted in it or in this file. Read the card in full first. Follow the style of the crate you touch (module docs, test names as sentences, `// Mutant (`name`): ...` comments at the mutated line).
- Register each mutant in the root `Cargo.toml` (`check-cfg` list) and in `xtask/src/mutants.rs` (`issue`: the card's number; `check`: the test that goes red; `platform` only when it is truly platform-only), then watch it go red: `cargo xtask mutants <name>`. Afterwards delete `target/mutants/<name>` (each is 0.1-4 GB).
- Before you finish: `cargo fmt --all`, `cargo clippy -p <each crate you touched> --all-targets -- -D warnings`, `cargo xtask check-deps`, `cargo test -p xtask`, and the tests of the crates you touched.
- Tests: follow the `## Tests` section of `CLAUDE.md` (cargo-nextest, #234).
- For `bosun-cli` run only its unit tests: `cargo test -p bosun-cli --bin bosun`. Never run its `tests/install.rs` (it registers a real scheduled task, #117), and never run the whole workspace's tests without nextest's default filter.
- Tests work only in `tempfile` directories. Never touch `%LOCALAPPDATA%\bosun`, the default bosun endpoint, the OS credential store outside a test double, or another repository.
- Never run `git worktree`, `git branch`, `git stash`, `git config` or `git update-ref` in this checkout itself (only inside temp repositories your tests create): the daemon's guards treat a moved ref or a changed git config as tampering.
- Never kill a process by name; kill only pids your test spawned.
- `.github`, `deny.toml`, `rust-toolchain.toml` and `mise.toml` are protected: do not edit them. Use only dependencies already in `Cargo.lock`.
- After adding or changing any `sqlx::query!`/`query_as!`/`query_scalar!` or a migration: `mise exec -- cargo xtask sqlx-prepare`, and commit the cache (`--check` must pass). A change of a protocol type in `bosun-api` needs `cargo xtask gen-api` (`--check` must pass).
- Run the tests you touch as the verdict does: `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 cargo test ...` (git then has no identity; a test that needs one sets it in its own temporary repository).
- The machine is low on memory: never run two cargo commands at once.
- Commit with a message that says what changed and why. One commit per numbered part is welcome.
- Check that each mutant is registered before you finish: `git grep -q "<name>" -- xtask` must
  find it (a card's verdict checks it that way).
