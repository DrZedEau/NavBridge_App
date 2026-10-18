#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "${1:-}" in
  --help|-h) echo "Usage: $0 (builds KMS binaries and dist/navbridge release bundle)"; exit 0 ;;
  '') ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
(( EUID != 0 )) || { echo 'Build as your normal user, without sudo.' >&2; exit 1; }
[[ $(uname -m) == aarch64 && $(dpkg --print-architecture) == arm64 ]] || { echo 'Requires a native 64-bit Raspberry Pi OS build.' >&2; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo 'Run ./scripts/install_build_deps.sh first.' >&2; exit 1; }
cd -- "$ROOT"
if [[ ! -f Cargo.lock ]]; then
  cargo generate-lockfile
  echo 'Generated Cargo.lock; retain it with the source for reproducible builds.'
fi
# Use a known output directory even if the shell has a custom CARGO_TARGET_DIR.
cargo build --locked --release --target-dir "$ROOT/target" --workspace \
  --features navbridge-core/rpi-ffmpeg,navbridge-ui/direct-kms
mkdir -p "$ROOT/dist"
STAGE=$(mktemp -d "$ROOT/dist/.navbridge.XXXXXX")
trap 'rm -rf -- "$STAGE"' EXIT
mkdir -p "$STAGE"/{bin,config,packaging,scripts}
install -m 0755 target/release/navbridge-core target/release/navbridge-ui "$STAGE/bin/"
install -m 0644 config/*.toml "$STAGE/config/"
install -m 0644 packaging/*.service packaging/70-navbridge-android.rules packaging/50-navbridge-wifi.rules "$STAGE/packaging/"
install -m 0755 scripts/install_systemd.sh scripts/install_runtime_deps.sh "$STAGE/scripts/"
install -m 0644 docs/installation-lite.md "$STAGE/README.md"
install -m 0644 LICENSE "$STAGE/LICENSE"
install -m 0644 THIRD_PARTY_NOTICES.md "$STAGE/THIRD_PARTY_NOTICES.md"
install -m 0644 LICENSE-LGPL-3.0 "$STAGE/LICENSE-LGPL-3.0"
CODENAME=$(. /etc/os-release; printf '%s' "$VERSION_CODENAME")
printf 'os_codename=%s\narchitecture=arm64\n' "$CODENAME" > "$STAGE/release.info"
if [[ -e $ROOT/dist/navbridge ]]; then
  mv -- "$ROOT/dist/navbridge" "$ROOT/dist/navbridge.previous.$(date +%Y%m%d-%H%M%S).$$"
fi
mv -- "$STAGE" "$ROOT/dist/navbridge"
tar -czf "$ROOT/dist/navbridge-${CODENAME}-arm64.tar.gz" -C "$ROOT/dist" navbridge
echo "Release: $ROOT/dist/navbridge-${CODENAME}-arm64.tar.gz"
