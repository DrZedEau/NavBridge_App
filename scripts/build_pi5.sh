#!/usr/bin/env bash
set -euo pipefail

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
fi

"$(dirname "$0")/check_pi5_deps.sh"

cargo build --release --workspace --features navbridge-core/rpi-ffmpeg
