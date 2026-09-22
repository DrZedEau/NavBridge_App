#!/usr/bin/env bash
set -euo pipefail

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
fi

"$(dirname "$0")/check_pi5_deps.sh"

# Builds the Slint LinuxKMS backend for a dedicated display without X11/Wayland.
cargo build --release -p navbridge-core --features rpi-ffmpeg
cargo build --release -p navbridge-ui --features direct-kms
