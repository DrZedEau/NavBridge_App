#!/usr/bin/env bash
# Run from a checkout on 64-bit Raspberry Pi OS Lite. Builds as the invoking user.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
START=1
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --no-start) START=0 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      echo "Usage: $0 [--no-start] [--dry-run]"
      echo "Installs dependencies, builds, installs and enables core/UI for the invoking user."
      echo "--no-start: enable for next boot without starting now."
      echo "--dry-run: print generated services without changing the system."
      exit 0 ;;
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done

INSTALL_USER=$(id -un)
if [[ $EUID == 0 ]]; then
  INSTALL_USER=${SUDO_USER:-}
  if [[ -z $INSTALL_USER || $INSTALL_USER == root ]]; then
    echo "Run as your normal Pi user, directly or through sudo; a root login has no invoking user." >&2
    exit 1
  fi
fi
[[ $INSTALL_USER =~ ^[a-zA-Z_][a-zA-Z0-9_.-]*\$?$ ]] || { echo "Unsupported account name" >&2; exit 1; }
INSTALL_GROUP=$(id -gn "$INSTALL_USER")
[[ $INSTALL_GROUP =~ ^[a-zA-Z_][a-zA-Z0-9_.-]*\$?$ ]] || exit 1
INSTALL_HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
[[ -d $INSTALL_HOME ]] || { echo "Invoking user has no home directory" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf -- "$STAGE"' EXIT
for unit in navbridge-core navbridge-ui; do
  sed -e "s/@USER@/$INSTALL_USER/g" -e "s/@GROUP@/$INSTALL_GROUP/g" \
    "$ROOT/packaging/$unit.service" > "$STAGE/$unit.service"
done
if (( DRY_RUN )); then
  echo "User: $INSTALL_USER; group: $INSTALL_GROUP; home: $INSTALL_HOME"
  echo "Source: $ROOT; binaries: /opt/navbridge/bin; configuration: /etc/navbridge"
  echo "Will install build dependencies, Rust if missing, USB rules and GPSD hotplug support."
  echo "Will build with Cargo.lock, preserve existing config, and enable both services."
  echo "Will disable getty@tty1 so NavBridge owns the dedicated display. Start now: $START"
  cat "$STAGE/navbridge-core.service" "$STAGE/navbridge-ui.service"
  exit 0
fi
if [[ $EUID != 0 ]]; then
  # Recreate staging under root after sudo; keep the original user's identity.
  rm -rf -- "$STAGE"
  exec sudo -- "$0" "$@"
fi
[[ $(uname -m) == aarch64 && $(dpkg --print-architecture) == arm64 ]] || { echo "64-bit Raspberry Pi OS (aarch64/arm64) is required." >&2; exit 1; }
[[ -d /run/systemd/system ]] || { echo "A booted systemd system is required." >&2; exit 1; }
[[ -f $ROOT/Cargo.lock ]] || { echo "Cargo.lock is required for a reproducible build." >&2; exit 1; }
if [[ $(tty 2>/dev/null || true) == /dev/tty1 ]]; then
  START=0
  echo "Installing from tty1: services will start on next boot to keep this installation session alive."
fi

as_user() {
  runuser -u "$INSTALL_USER" -- env HOME="$INSTALL_HOME" \
    CARGO_HOME="$INSTALL_HOME/.cargo" RUSTUP_HOME="$INSTALL_HOME/.rustup" \
    PATH="$INSTALL_HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" "$@"
}
as_user test -w "$ROOT" || { echo "The checkout must be writable by $INSTALL_USER." >&2; exit 1; }

apt-get update
# Winit is still enabled in the current manifest, so desktop development headers
# remain build dependencies even though the installed UI uses direct KMS.
apt-get install -y build-essential ca-certificates curl git pkg-config cmake clang libclang-dev \
  libasound2-dev libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
  libfontconfig-dev fonts-dejavu-core libusb-1.0-0-dev libudev-dev libxkbcommon-dev \
  libgbm-dev libseat-dev libwayland-dev libinput-dev libx11-dev libx11-xcb-dev \
  libxcb1-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcursor-dev gpsd gpsd-clients

if [[ ! -x $INSTALL_HOME/.cargo/bin/rustup ]]; then
  as_user bash -c 'set -euo pipefail; installer=$(mktemp); trap '\''rm -f "$installer"'\'' EXIT; curl --proto "=https" --tlsv1.2 -fsSL https://sh.rustup.rs -o "$installer"; sh "$installer" -y --profile minimal --no-modify-path'
fi
as_user rustup toolchain install stable --profile minimal
cd -- "$ROOT"
as_user cargo build --locked --release --workspace \
  --features navbridge-core/rpi-ffmpeg,navbridge-ui/direct-kms

for group in plugdev audio video render input; do
  getent group "$group" >/dev/null || groupadd --system "$group"
done
# SupplementaryGroups in each unit applies immediately without changing login groups.
install -d -m 0755 /opt/navbridge/bin /etc/navbridge
BACKUP="/var/backups/navbridge/$(date +%Y%m%d-%H%M%S)-$$"
install -d -m 0700 "$BACKUP"
for unit in navbridge-core navbridge-ui; do
  if [[ -f /etc/systemd/system/$unit.service ]]; then
    cp -a "/etc/systemd/system/$unit.service" "$BACKUP/"
  fi
  if [[ -f /opt/navbridge/bin/$unit ]]; then cp -a "/opt/navbridge/bin/$unit" "$BACKUP/"; fi
  # Atomic replacement works even if --no-start leaves the old process running.
  install -m 0755 "target/release/$unit" "/opt/navbridge/bin/$unit.new"
  mv -f "/opt/navbridge/bin/$unit.new" "/opt/navbridge/bin/$unit"
done
for config in core frontend keyboard; do
  if [[ ! -e /etc/navbridge/$config.toml ]]; then
    install -m 0644 "config/$config.toml" "/etc/navbridge/$config.toml"
  else
    echo "Preserving /etc/navbridge/$config.toml"
  fi
done
# Templates use /run/navbridge, not the developer launcher's temporary sockets.
# Fail rather than silently rewriting existing customized configurations.
for config in core frontend; do
  if ! grep -Eq '^[[:space:]]*socket_path[[:space:]]*=[[:space:]]*"/run/navbridge/core.sock"[[:space:]]*(#.*)?$' "/etc/navbridge/$config.toml"; then
    echo "Set socket_path = \"/run/navbridge/core.sock\" in /etc/navbridge/$config.toml and rerun." >&2
    exit 1
  fi
done
if [[ ! -e /etc/navbridge/display.env ]]; then
  printf '%s\n' '# Optional Slint display settings, e.g. SLINT_DRM_OUTPUT=HDMI-A-1' \
    '# SLINT_DRM_MODE, SLINT_KMS_ROTATION and RUST_LOG may also be set here.' > /etc/navbridge/display.env
fi

cat > "$STAGE/70-navbridge-carlinkit.rules" <<'RULES'
# USB device access for the non-root NavBridge backend.
SUBSYSTEM=="usb", ATTR{idVendor}=="1314", MODE="0660", GROUP="plugdev", TAG+="uaccess"
SUBSYSTEM=="usb", ATTR{idVendor}=="08e4", MODE="0660", GROUP="plugdev", TAG+="uaccess"
RULES
install -m 0644 "$STAGE/70-navbridge-carlinkit.rules" /etc/udev/rules.d/70-navbridge-carlinkit.rules
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --action=change
udevadm settle
if [[ -f /etc/default/gpsd ]]; then
  cp -a /etc/default/gpsd "$BACKUP/gpsd.defaults"
  if grep -q '^USBAUTO=' /etc/default/gpsd; then
    sed -i 's/^USBAUTO=.*/USBAUTO="true"/' /etc/default/gpsd
  else
    printf '\nUSBAUTO="true"\n' >> /etc/default/gpsd
  fi
fi
systemctl enable --now gpsd.socket

systemd-analyze verify "$STAGE/navbridge-core.service" "$STAGE/navbridge-ui.service"
install -m 0644 "$STAGE/navbridge-core.service" /etc/systemd/system/navbridge-core.service
install -m 0644 "$STAGE/navbridge-ui.service" /etc/systemd/system/navbridge-ui.service
systemctl daemon-reload
systemctl disable getty@tty1.service
systemctl enable navbridge-core.service navbridge-ui.service
if (( START )); then
  systemctl stop navbridge-ui.service navbridge-core.service
  if pgrep -x navbridge-core >/dev/null || pgrep -x navbridge-ui >/dev/null; then
    echo "Stop the manually launched NavBridge app, then rerun this installer to finish startup." >&2
    exit 1
  fi
  # A previous sudo developer run may have left a root-owned frame in sticky tmpfs.
  rm -f /dev/shm/navbridge-video.rgba
  systemctl start navbridge-core.service navbridge-ui.service
  sleep 3
  for unit in navbridge-core navbridge-ui; do
    if ! systemctl is-active --quiet "$unit.service"; then
      journalctl -u navbridge-core -u navbridge-ui -n 60 --no-pager
      exit 1
    fi
  done
fi
echo "Installed for $INSTALL_USER. Configuration: /etc/navbridge; backups: $BACKUP"
echo "Logs: journalctl -u navbridge-core -u navbridge-ui -f"
echo "Restart: sudo systemctl restart navbridge-core navbridge-ui"
if (( ! START )); then echo "Services enabled for the next boot. Reboot when ready."; fi
