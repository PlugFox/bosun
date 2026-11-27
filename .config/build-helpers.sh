#!/bin/sh
# The nextest setup script `build-helpers` (.config/nextest.toml): builds the binaries the
# bosund scenes run, once per run, and hands their paths to every test process through
# `$NEXTEST_ENV`. `cargo test` does not run it: there `built()` builds them itself.
set -eu
cargo=${CARGO:-cargo}
# The same two builds as `built()`, so both resolve the same features and share one cache.
out=$("$cargo" build --locked --message-format=json --package fake-agent --package bosun-mcp)
out="$out
$("$cargo" build --locked --message-format=json --package bosun-proc --bin bosun-shim)"
# The path cargo reports for a target's executable, its JSON escapes of `\` undone.
exe() {
  printf '%s\n' "$out" | grep "\"name\":\"$1\"" | grep -o '"executable":"[^"]*"' |
    sed -e 's/^"executable":"//' -e 's/"$//' -e 's/\\\\/\\/g' | tail -n 1
}
agent=$(exe fake-agent)
mcp=$(exe bosun-mcp)
shim=$(exe bosun-shim)
for path in "$agent" "$mcp" "$shim"; do
  if [ -z "$path" ]; then
    echo "build-helpers: cargo named no executable" >&2
    exit 1
  fi
done
{
  echo "BOSUN_TEST_FAKE_AGENT=$agent"
  echo "BOSUN_TEST_BOSUN_MCP=$mcp"
  echo "BOSUN_TEST_BOSUN_SHIM=$shim"
} >>"$NEXTEST_ENV"
