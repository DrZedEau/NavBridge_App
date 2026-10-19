#!/usr/bin/env bash
# Builds the core and UI, and a release bundle in dist/. On Raspberry Pi OS the
# UI drives the display directly (KMS); on another Debian or Ubuntu system it
# can also open a window for development (./scripts/run_desktop.sh).
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "${1:-}" in
  --help|-h) echo "Usage: $0 (builds the binaries and a dist/navbridge release bundle)"; exit 0 ;;
  '') ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
(( EUID != 0 )) || { echo 'Build as your normal user, without sudo.' >&2; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo 'Run ./scripts/install_build_deps.sh first.' >&2; exit 1; }
INCLUDE=$(pkg-config --variable=includedir libavutil 2>/dev/null) \
  || { echo 'FFmpeg headers are missing: run ./scripts/install_build_deps.sh first.' >&2; exit 1; }
cd -- "$ROOT"
# Raspberry Pi OS's FFmpeg adds pixel formats (SAND) that the bindings must
# know about; stock FFmpeg has none, and the same feature would not build.
if grep -q 'AV_PIX_FMT_SAND' "$INCLUDE/libavutil/pixfmt.h" 2>/dev/null; then
  TARGET="Raspberry Pi"
  FEATURES=navbridge-core/rpi-ffmpeg,navbridge-ui/direct-kms
else
  TARGET="desktop"
  FEATURES=navbridge-ui/direct-kms,navbridge-ui/desktop
fi
echo "Building for $TARGET: $FEATURES"
if [[ ! -f Cargo.lock ]]; then
  cargo generate-lockfile
  echo 'Generated Cargo.lock; retain it with the source for reproducible builds.'
fi
# Use a known output directory even if the shell has a custom CARGO_TARGET_DIR.
cargo build --locked --release --target-dir "$ROOT/target" --workspace --features "$FEATURES"
ARCH=$(dpkg --print-architecture 2>/dev/null || uname -m)
mkdir -p "$ROOT/dist"
STAGE=$(mktemp -d "$ROOT/dist/.navbridge.XXXXXX")
trap 'rm -rf -- "$STAGE"' EXIT
mkdir -p "$STAGE"/{bin,config,packaging,scripts}
install -m 0755 target/release/navbridge-core target/release/navbridge-ui "$STAGE/bin/"
install -m 0644 config/*.toml "$STAGE/config/"
install -m 0644 packaging/*.service packaging/*.rules "$STAGE/packaging/"
install -m 0755 scripts/install_systemd.sh scripts/install_runtime_deps.sh "$STAGE/scripts/"
install -m 0644 docs/installation-lite.md "$STAGE/README.md"
install -m 0644 LICENSE "$STAGE/LICENSE"
install -m 0644 THIRD_PARTY_NOTICES.md "$STAGE/THIRD_PARTY_NOTICES.md"
install -m 0644 LICENSE-LGPL-3.0 "$STAGE/LICENSE-LGPL-3.0"
CODENAME=$(. /etc/os-release; printf '%s' "$VERSION_CODENAME")
printf 'os_codename=%s\narchitecture=%s\n' "$CODENAME" "$ARCH" > "$STAGE/release.info"
if [[ -e $ROOT/dist/navbridge ]]; then
  mv -- "$ROOT/dist/navbridge" "$ROOT/dist/navbridge.previous.$(date +%Y%m%d-%H%M%S).$$"
fi
mv -- "$STAGE" "$ROOT/dist/navbridge"
tar -czf "$ROOT/dist/navbridge-${CODENAME}-${ARCH}.tar.gz" -C "$ROOT/dist" navbridge
echo "Release: $ROOT/dist/navbridge-${CODENAME}-${ARCH}.tar.gz"
if [[ $TARGET == desktop ]]; then
  echo 'Run it in a window with ./scripts/run_desktop.sh.'
fi
