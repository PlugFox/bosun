#!/bin/sh
# The nextest setup script `build-daemon` (.config/nextest.toml): builds the binaries that
# `bosun-cli`'s `tests/cli.rs` and `tests/loop_commands.rs` run (`bosund`, `bosun-mcp`,
# `fake-agent`, `recount`), once per run, and hands their directory to every test process in
# `BOSUN_TEST_BIN_DIR` through `$NEXTEST_ENV`. `cargo test` does not run it: there `bin_dir()`
# builds them itself, with the same packages.
#
# Without it each test process ran this build itself. Its packages unify features apart from
# the workspace build nextest made, so the first test recompiled `sqlx` and `bosund` inside its
# own 240 s limit and timed out on CI's Windows runner.
set -eu
cargo=${CARGO:-cargo}
out=$("$cargo" build --locked --message-format=json \
  --package bosund --package bosun-mcp --package fake-agent --package recount)
path=$(printf '%s\n' "$out" | grep '"name":"bosund"' | grep -o '"executable":"[^"]*"' |
  sed -e 's/^"executable":"//' -e 's/"$//' -e 's/\\\\/\\/g' | tail -n 1)
if [ -z "$path" ]; then
  echo "build-daemon: cargo named no bosund executable" >&2
  exit 1
fi
echo "BOSUN_TEST_BIN_DIR=$(dirname "$path")" >>"$NEXTEST_ENV"
