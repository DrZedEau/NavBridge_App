#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SOCKET="/tmp/navbridge-$$.sock"
CORE_PID=""

cleanup() {
  if [ -n "$CORE_PID" ]; then
    kill "$CORE_PID" 2>/dev/null || true
  fi
  rm -f "$SOCKET"
}
trap cleanup EXIT INT TERM

# Use a temporary socket for developer runs so root access to /run is not needed.
sed "s#socket_path = .*#socket_path = \"$SOCKET\"#" "$ROOT/config/core.toml" > /tmp/navbridge-core-$$.toml
sed "s#socket_path = .*#socket_path = \"$SOCKET\"#" "$ROOT/config/frontend.toml" > /tmp/navbridge-ui-$$.toml
trap 'cleanup; rm -f /tmp/navbridge-core-$$.toml /tmp/navbridge-ui-$$.toml' EXIT INT TERM

"$ROOT/target/release/navbridge-core" --config /tmp/navbridge-core-$$.toml &
CORE_PID=$!

set +e
"$ROOT/target/release/navbridge-ui" \
  --config /tmp/navbridge-ui-$$.toml \
  --keyboard "$ROOT/config/keyboard.toml"
STATUS=$?
set -e
exit "$STATUS"
