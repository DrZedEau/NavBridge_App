#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "${1:-}" in
  --help|-h) echo "Usage: $0 [--dry-run]"; exit 0 ;;
  --dry-run|'') ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || exit 2
USER_NAME=${SUDO_USER:-$(id -un)}
[[ $USER_NAME != root ]] || { echo 'Run as your normal account (sudo is supported).' >&2; exit 1; }
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
# libavformat-dev: the ffmpeg-next codec API needs its bindings at build time,
# although the linked core does not load libavformat.
PACKAGES=(build-essential ca-certificates curl git pkg-config cmake clang libclang-dev
  libssl-dev libasound2-dev libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
  libfontconfig-dev libusb-1.0-0-dev libudev-dev libxkbcommon-dev libinput-dev)
if [[ ${1:-} == --dry-run ]]; then
  "$ROOT/scripts/install_runtime_deps.sh" --dry-run
  printf 'Build packages:\n'; printf '%s\n' "${PACKAGES[@]}"
  echo "Rust stable will be installed/updated for $USER_NAME in $USER_HOME/.cargo."
  exit 0
fi
if (( EUID != 0 )); then exec sudo -- "$0" "$@"; fi
"$ROOT/scripts/install_runtime_deps.sh"
apt-get install -y --no-install-recommends "${PACKAGES[@]}"
as_user() {
  runuser -u "$USER_NAME" -- env HOME="$USER_HOME" CARGO_HOME="$USER_HOME/.cargo" \
    RUSTUP_HOME="$USER_HOME/.rustup" PATH="$USER_HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" "$@"
}
if [[ ! -x $USER_HOME/.cargo/bin/rustup ]]; then
  as_user bash -c 'set -euo pipefail; tmp=$(mktemp); trap '\''rm -f "$tmp"'\'' EXIT; curl --proto "=https" --tlsv1.2 -fsSL https://sh.rustup.rs -o "$tmp"; sh "$tmp" -y --profile minimal --no-modify-path'
fi
as_user rustup toolchain install stable --profile minimal
echo "Build dependencies and udev rules installed. Build as $USER_NAME with ./scripts/build_kms.sh."
