#!/usr/bin/env bash
# Installs the boot splash: the NavBridge image on the panel from the moment
# the display driver starts until the UI takes the display. Run it after
# install_systemd.sh, from a release bundle or a build.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BIN_DIR="$ROOT/bin"
[[ -d $BIN_DIR ]] || BIN_DIR="$ROOT/target/release"
IMAGE="$ROOT/packaging/splash.png"
DRY_RUN=0
UNINSTALL=0
ARGS=("$@")
usage() {
  cat <<EOF
Usage: $0 [--image FILE.png] [--binary-dir DIR] [--dry-run]
       $0 --uninstall [--dry-run]

Shows FILE.png (default: packaging/splash.png, 400x240) on the panel while the
Pi boots, stretched to the panel's size if it differs.

--uninstall removes the splash.
EOF
}
while (( $# )); do
  case "$1" in
    --image) [[ $# -ge 2 ]] || { echo 'Missing image file' >&2; exit 2; }; IMAGE=$(realpath -m -- "$2"); shift 2 ;;
    --binary-dir) [[ $# -ge 2 ]] || { echo 'Missing binary directory' >&2; exit 2; }; BIN_DIR=$(realpath -m -- "$2"); shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

SERVICE=/etc/systemd/system/navbridge-splash.service
RULE=/etc/udev/rules.d/60-navbridge-splash.rules
BINARY=/opt/navbridge/bin/navbridge-splash
SPLASH=/opt/navbridge/splash.png

if (( DRY_RUN )); then
  if (( UNINSTALL )); then
    echo "Will remove $SERVICE, $RULE, $BINARY and $SPLASH."
    exit 0
  fi
  echo "Binary: $BIN_DIR/navbridge-splash -> $BINARY"
  echo "Image: $IMAGE -> $SPLASH"
  echo "Will install $SERVICE and $RULE (started by udev when /dev/fb0 appears)."
  exit 0
fi
if [[ $EUID != 0 ]]; then
  exec sudo -- "$0" "${ARGS[@]}"
fi
[[ -d /run/systemd/system ]] || { echo "A booted systemd system is required." >&2; exit 1; }

if (( UNINSTALL )); then
  rm -f -- "$RULE" "$SERVICE" "$BINARY" "$SPLASH"
  systemctl daemon-reload
  udevadm control --reload-rules
  # The text console draws on the panel again.
  for console in /sys/class/vtconsole/vtcon*; do
    if grep -q 'frame buffer' "$console/name" 2>/dev/null; then echo 1 > "$console/bind" || true; fi
  done
  echo "Splash removed. Reboot to boot without it."
  exit 0
fi

case $(uname -m) in
  aarch64) MACHINE='183 0'; MACHINE_NAME='64-bit ARM' ;;
  x86_64) MACHINE='62 0'; MACHINE_NAME='x86-64' ;;
  *) echo "Unsupported processor: $(uname -m) (supported: aarch64, x86_64)." >&2; exit 1 ;;
esac
SOURCE=$BIN_DIR/navbridge-splash
[[ -f $SOURCE ]] || { echo "Missing $SOURCE. Use a release bundle or run ./scripts/build_kms.sh first." >&2; exit 1; }
header=$(od -An -tx1 -N6 "$SOURCE" | tr -d ' \n')
machine=$(od -An -tu1 -j18 -N2 "$SOURCE" | xargs)
[[ $header == 7f454c460201 && $machine == "$MACHINE" ]] || { echo "navbridge-splash is not a $MACHINE_NAME ELF binary for this computer." >&2; exit 1; }
[[ -f $IMAGE ]] || { echo "Missing image $IMAGE" >&2; exit 1; }
"$SOURCE" --check "$IMAGE"

install -d -m 0755 /opt/navbridge/bin
install -m 0755 "$SOURCE" "$BINARY.new"
mv -f -- "$BINARY.new" "$BINARY"
install -m 0644 "$IMAGE" "$SPLASH.new"
mv -f -- "$SPLASH.new" "$SPLASH"
install -m 0644 "$ROOT/packaging/navbridge-splash.service" "$SERVICE"
install -m 0644 "$ROOT/packaging/60-navbridge-splash.rules" "$RULE"
systemd-analyze verify "$SERVICE"
systemctl daemon-reload
udevadm control --reload-rules

if [[ -r /sys/class/graphics/fb0/virtual_size ]]; then
  echo "The panel's framebuffer is $(tr ',' 'x' < /sys/class/graphics/fb0/virtual_size); the splash is stretched to it."
fi
echo "Splash installed: $SPLASH. Reboot to see it."
echo "Try it now, over SSH: sudo systemctl stop navbridge-ui; sudo systemctl start navbridge-splash"
echo "Then: sudo systemctl start navbridge-ui. Remove it: $0 --uninstall"
