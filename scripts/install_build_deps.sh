#!/usr/bin/env bash
# Build tools, headers and Rust, on Raspberry Pi OS or a Debian or Ubuntu PC.
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
# For running the core as this account: phones (plugdev) and the I-Bus board
# (dialout). The services get these groups from their units instead.
GROUPS_NEEDED=()
for group in plugdev dialout; do
  id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx "$group" || GROUPS_NEEDED+=("$group")
done
if [[ ${1:-} == --dry-run ]]; then
  "$ROOT/scripts/install_runtime_deps.sh" --dry-run
  printf 'Build packages:\n'; printf '%s\n' "${PACKAGES[@]}"
  echo "Rust stable will be installed/updated for $USER_NAME in $USER_HOME/.cargo."
  (( ${#GROUPS_NEEDED[@]} == 0 )) || echo "$USER_NAME will join: ${GROUPS_NEEDED[*]} (from the next login)."
  exit 0
fi
if (( EUID != 0 )); then exec sudo -- "$0" "$@"; fi
"$ROOT/scripts/install_runtime_deps.sh"
apt-get install -y --no-install-recommends "${PACKAGES[@]}"
for group in "${GROUPS_NEEDED[@]}"; do
  usermod -aG "$group" "$USER_NAME"
done
as_user() {
  runuser -u "$USER_NAME" -- env HOME="$USER_HOME" CARGO_HOME="$USER_HOME/.cargo" \
    RUSTUP_HOME="$USER_HOME/.rustup" PATH="$USER_HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" "$@"
}
if [[ ! -x $USER_HOME/.cargo/bin/rustup ]]; then
  as_user bash -c 'set -euo pipefail; tmp=$(mktemp); trap '\''rm -f "$tmp"'\'' EXIT; curl --proto "=https" --tlsv1.2 -fsSL https://sh.rustup.rs -o "$tmp"; sh "$tmp" -y --profile minimal --no-modify-path'
fi
as_user rustup toolchain install stable --profile minimal
echo "Build dependencies and udev rules installed. Build as $USER_NAME with ./scripts/build_kms.sh."
if (( ${#GROUPS_NEEDED[@]} )); then
  echo "$USER_NAME joined ${GROUPS_NEEDED[*]}: log out and back in before running NavBridge."
fi
