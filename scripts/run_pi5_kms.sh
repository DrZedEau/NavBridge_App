#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

# Direct KMS developer mode deliberately uses Slint's no-seat backend. This is
# suitable when launching from SSH or another session that does not own a local
# VT. It requires privileged access to DRM and input device nodes.
export SLINT_BACKEND=${SLINT_BACKEND:-linuxkms-software}
export SLINT_FULLSCREEN=1

if [ "${EUID}" -ne 0 ]; then
  echo "NavBridge LinuxKMS: re-launching with sudo for direct DRM/input access..." >&2
  exec sudo --preserve-env=SLINT_BACKEND,SLINT_FULLSCREEN,SLINT_DRM_OUTPUT,SLINT_DRM_MODE,SLINT_KMS_ROTATION,RUST_LOG,RUST_BACKTRACE \
    "$0" "$@"
fi

# Helpful diagnostics before starting the application.
echo "NavBridge LinuxKMS direct mode" >&2
echo "  backend: ${SLINT_BACKEND}" >&2
if [ -n "${SLINT_DRM_OUTPUT:-}" ]; then echo "  DRM output: ${SLINT_DRM_OUTPUT}" >&2; fi
if [ -n "${SLINT_DRM_MODE:-}" ]; then echo "  DRM mode: ${SLINT_DRM_MODE}" >&2; fi

exec "$ROOT/scripts/run_pi5.sh"
