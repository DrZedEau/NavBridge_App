#!/usr/bin/env bash
# Runs the built core and UI on a desktop for development: the UI in a window,
# both as your account. On the Pi's display itself, use run_kms.sh.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CODEC=0 SCALE=1
for arg in "$@"; do
  case $arg in
    --help|-h) echo "Usage: $0 [--codec] [--800x480] (runs the built core and UI in a window; --codec plays and records through the USB codec of config/core.toml instead of the desktop's default sound device; --800x480 draws the UI twice as large, with Android Auto's video at its full size)"; exit 0 ;;
    --codec) CODEC=1 ;;
    --800x480) SCALE=2 ;;
    *) echo "Unknown argument: $arg" >&2; exit 2 ;;
  esac
done
(( EUID != 0 )) || { echo 'Run as your normal account: the window opens in your desktop session.' >&2; exit 1; }
[[ -n ${WAYLAND_DISPLAY:-}${DISPLAY:-} ]] || { echo 'No desktop session here; on a text console, use ./scripts/run_kms.sh.' >&2; exit 1; }
for app in navbridge-core navbridge-ui; do
  [[ -x $ROOT/target/release/$app ]] || { echo 'Run ./scripts/build_kms.sh first.' >&2; exit 1; }
  if pgrep -x "$app" >/dev/null; then echo "Stop the existing $app first." >&2; exit 1; fi
done
# The socket and the projection frames, private to this account.
RUN_DIR=$(mktemp -d "${XDG_RUNTIME_DIR:-/dev/shm}/navbridge-run.XXXXXX")
CORE_PID= UI_PID=
cleanup() {
  trap - EXIT INT TERM
  for pid in "$UI_PID" "$CORE_PID"; do
    if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  rm -rf -- "$RUN_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
AUDIO=()
if (( ! CODEC )); then
  # The desktop's sound server, through ALSA's default device.
  AUDIO=(-e 's|^playback_device = .*|playback_device = "default"|'
    -e 's|^capture_device = .*|capture_device = "default"|')
fi
# The car's ignition must not shut the PC down.
sed -e "s|^socket_path = .*|socket_path = \"$RUN_DIR/core.sock\"|" "${AUDIO[@]}" \
  -e 's|^shutdown = .*|shutdown = false|' "$ROOT/config/core.toml" > "$RUN_DIR/core.toml"
sed -e "s|^socket_path = .*|socket_path = \"$RUN_DIR/core.sock\"|" \
  -e 's|^fullscreen = .*|fullscreen = false|' -e "s|^scale = .*|scale = $SCALE|" \
  "$ROOT/config/frontend.toml" > "$RUN_DIR/frontend.toml"
export SLINT_BACKEND=${SLINT_BACKEND:-winit-software}
"$ROOT/target/release/navbridge-core" --config "$RUN_DIR/core.toml" & CORE_PID=$!
"$ROOT/target/release/navbridge-ui" --config "$RUN_DIR/frontend.toml" --keyboard "$ROOT/config/keyboard.toml" & UI_PID=$!
set +e
wait -n "$CORE_PID" "$UI_PID"
STATUS=$?
exit "$STATUS"
