#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
LOG=/tmp/navbridge.log
CORE_ARGS=() DETACH=0
for arg in "$@"; do
  case $arg in
    --help|-h) echo "Usage: $0 [--wireless] [--detach] (runs the built core and UI directly in KMS; --wireless starts with wireless Android Auto on; --detach keeps them running once SSH drops, logging to $LOG)"; exit 0 ;;
    --wireless) CORE_ARGS+=(--wireless) ;;
    --detach) DETACH=1 ;;
    *) echo "Unknown argument: $arg" >&2; exit 2 ;;
  esac
done
for app in navbridge-core navbridge-ui; do
  [[ -x $ROOT/target/release/$app ]] || { echo 'Run ./scripts/build_kms.sh first.' >&2; exit 1; }
  if pgrep -x "$app" >/dev/null; then echo "Stop the existing $app before a manual run." >&2; exit 1; fi
done
export SLINT_BACKEND=${SLINT_BACKEND:-linuxkms-software}
export SLINT_FULLSCREEN=1
if (( EUID != 0 )); then
  exec sudo --preserve-env=SLINT_BACKEND,SLINT_FULLSCREEN,SLINT_DRM_OUTPUT,SLINT_DRM_MODE,SLINT_KMS_ROTATION,RUST_LOG,RUST_BACKTRACE,NAVBRIDGE_FRAME_STATS "$0" "$@"
fi
if (( DETACH )); then
  # Root may not reopen another user's file in /tmp, so the log is created
  # afresh, then given to whoever ran sudo, who can also redirect into it.
  rm -f -- "$LOG"
  exec 3>"$LOG"
  chown "${SUDO_UID:-0}:${SUDO_GID:-0}" -- "$LOG"
  # In a session of its own, the run outlives this terminal.
  setsid "$0" "${CORE_ARGS[@]}" >&3 2>&3 </dev/null &
  RUN_PID=$!
  exec 3>&-
  echo "NavBridge runs in the background; log: $LOG"
  echo 'Stop it with: sudo pkill -x navbridge-core'
  # A start that fails at once is still shown here.
  for _ in {1..20}; do
    sleep 0.1
    if ! kill -0 "$RUN_PID" 2>/dev/null; then
      echo 'NavBridge stopped at once:' >&2
      tail -n 20 -- "$LOG" >&2
      exit 1
    fi
  done
  exit 0
fi
RUN_DIR=$(mktemp -d /tmp/navbridge-run.XXXXXX)
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
sed "s|^socket_path = .*|socket_path = \"$RUN_DIR/core.sock\"|" "$ROOT/config/core.toml" > "$RUN_DIR/core.toml"
sed "s|^socket_path = .*|socket_path = \"$RUN_DIR/core.sock\"|" "$ROOT/config/frontend.toml" > "$RUN_DIR/frontend.toml"
"$ROOT/target/release/navbridge-core" --config "$RUN_DIR/core.toml" "${CORE_ARGS[@]}" & CORE_PID=$!
"$ROOT/target/release/navbridge-ui" --config "$RUN_DIR/frontend.toml" --keyboard "$ROOT/config/keyboard.toml" & UI_PID=$!
set +e
wait -n "$CORE_PID" "$UI_PID"
STATUS=$?
exit "$STATUS"
