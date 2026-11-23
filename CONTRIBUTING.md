# Contributing

The design is the GitHub issues: #1 is the map, #2–#5 and #12 are the cross-cutting contracts,
#6–#11 are the stages. Read the issue a change touches before changing code; when a decision
changes, the issue changes with it.

## Working on the code

- Only the pinned Rust toolchain (`rust-toolchain.toml`) is needed to build and test.
- `mise install` adds the pinned checkers (`mise.toml`): cargo-deny, typos, cargo-machete,
  and the sqlx CLI.
- Repository tasks are `cargo xtask <task>`: `check-deps` for the dependency rules,
  `lint-fixtures` for the synthetic-fixture templates (`fixtures/README.md`), `mutants` for the
  mutant job, `sqlx-prepare` after changing a query or a migration of `bosun-store`. The build
  reads the committed query cache `.sqlx/` (`SQLX_OFFLINE=true` in `.cargo/config.toml`) and
  needs no database.
- A check is not trusted until it has been seen failing: every acceptance item names a mutant
  (`--cfg mutant_<name>`), registered in `xtask/src/mutants.rs`.
- The repository is public. Fixtures are synthetic or scrubbed: no real transcripts, paths,
  tokens, issue text from other repositories or personal data.

## Licence

bosun is dual-licensed under the MIT licence and the Apache License, Version 2.0, at your
option (`LICENSE-MIT`, `LICENSE-APACHE`).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
