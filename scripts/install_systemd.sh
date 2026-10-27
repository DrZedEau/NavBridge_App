#!/usr/bin/env bash
# Deploy a release bundle or existing build. No source code or Rust required.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
START=1
DRY_RUN=0
FORCE=0
BIN_DIR="$ROOT/bin"
[[ -d $BIN_DIR ]] || BIN_DIR="$ROOT/target/release"
ARGS=("$@")
while (( $# )); do
  case "$1" in
    --no-start) START=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --force) FORCE=1; shift ;;
    --binary-dir) [[ $# -ge 2 ]] || { echo 'Missing binary directory' >&2; exit 2; }; BIN_DIR=$(realpath -m -- "$2"); shift 2 ;;
    --help|-h)
      echo "Usage: $0 [--binary-dir DIR] [--no-start] [--dry-run] [--force]"
      echo "Install prebuilt binaries and runtime dependencies, never build the application."
      echo "--force: install on a machine whose desktop (display manager) uses tty1."
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

INSTALL_USER=$(id -un)
if [[ $EUID == 0 ]]; then
  INSTALL_USER=${SUDO_USER:-}
  if [[ -z $INSTALL_USER || $INSTALL_USER == root ]]; then
    echo "Run as your normal user, directly or through sudo; a root login has no invoking user." >&2
    exit 1
  fi
fi
[[ $INSTALL_USER =~ ^[a-zA-Z_][a-zA-Z0-9_.-]*\$?$ ]] || { echo "Unsupported account name" >&2; exit 1; }
INSTALL_GROUP=$(id -gn "$INSTALL_USER")
[[ $INSTALL_GROUP =~ ^[a-zA-Z_][a-zA-Z0-9_.-]*\$?$ ]] || exit 1
INSTALL_HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
[[ -d $INSTALL_HOME ]] || { echo "Invoking user has no home directory" >&2; exit 1; }
# A desktop's display manager owns the display that the UI service takes.
DESKTOP=0
if systemctl is-enabled --quiet display-manager.service 2>/dev/null; then DESKTOP=1; fi

STAGE=$(mktemp -d)
trap 'rm -rf -- "$STAGE"' EXIT
for unit in navbridge-core navbridge-ui; do
  sed -e "s/@USER@/$INSTALL_USER/g" -e "s/@GROUP@/$INSTALL_GROUP/g" \
    "$ROOT/packaging/$unit.service" > "$STAGE/$unit.service"
done
sed -e "s/@USER@/$INSTALL_USER/g" "$ROOT/packaging/50-navbridge-wifi.rules" > "$STAGE/50-navbridge-wifi.rules"
if (( DRY_RUN )); then
  echo "User: $INSTALL_USER; group: $INSTALL_GROUP; home: $INSTALL_HOME"
  echo "Source: $ROOT; binaries: /opt/navbridge/bin; configuration: /opt/navbridge/config"
  echo "Prebuilt binaries: $BIN_DIR"
  echo "Will install runtime dependencies and USB rules; no compilation or Rust installation."
  echo "Will preserve existing config and enable both services."
  echo "Will let $INSTALL_USER switch the Wi-Fi hotspot through NetworkManager (polkit rule)."
  echo "Will disable getty@tty1 so NavBridge owns the dedicated display. Start now: $START"
  if (( DESKTOP )); then
    echo "This machine runs a desktop: the install stops unless --force is given."
  fi
  cat "$STAGE/navbridge-core.service" "$STAGE/navbridge-ui.service"
  exit 0
fi
if [[ $EUID != 0 ]]; then
  # Recreate staging under root after sudo; keep the original user's identity.
  rm -rf -- "$STAGE"
  exec sudo -- "$0" "${ARGS[@]}"
fi
# The ELF machine of this computer's binaries: 64-bit ARM (the Pi) or x86-64.
case $(uname -m) in
  aarch64) MACHINE='183 0'; MACHINE_NAME='64-bit ARM' ;;
  x86_64) MACHINE='62 0'; MACHINE_NAME='x86-64' ;;
  *) echo "Unsupported processor: $(uname -m) (supported: aarch64, x86_64)." >&2; exit 1 ;;
esac
command -v apt-get >/dev/null || { echo "A Debian or Ubuntu system is required." >&2; exit 1; }
[[ -d /run/systemd/system ]] || { echo "A booted systemd system is required." >&2; exit 1; }
if (( DESKTOP && ! FORCE )); then
  echo "A desktop (display manager) uses this machine's display, which the UI service would take." >&2
  echo "For development, run ./scripts/run_desktop.sh instead; on a dedicated kiosk, rerun with --force." >&2
  exit 1
fi

if [[ $(tty 2>/dev/null || true) == /dev/tty1 ]]; then
  START=0
  echo "Installing from tty1: services will start on next boot to keep this installation session alive."
fi

for app in navbridge-core navbridge-ui; do
  [[ -f $BIN_DIR/$app ]] || { echo "Missing $BIN_DIR/$app. Supply a published release or build it separately." >&2; exit 1; }
  # Reject scripts and binaries for the wrong CPU before invoking ldd.
  header=$(od -An -tx1 -N6 "$BIN_DIR/$app" | tr -d ' \n')
  machine=$(od -An -tu1 -j18 -N2 "$BIN_DIR/$app" | xargs)
  [[ $header == 7f454c460201 && $machine == "$MACHINE" ]] || { echo "$app is not a $MACHINE_NAME ELF binary for this computer." >&2; exit 1; }
done
if [[ -f $ROOT/release.info ]]; then
  expected=$(sed -n 's/^os_codename=//p' "$ROOT/release.info")
  actual=$(. /etc/os-release; printf '%s' "${VERSION_CODENAME:-}")
  [[ $expected == "$actual" ]] || { echo "This release targets $expected; this OS is $actual. Download the matching release." >&2; exit 1; }
  expected=$(sed -n 's/^architecture=//p' "$ROOT/release.info")
  actual=$(dpkg --print-architecture)
  [[ -z $expected || $expected == "$actual" ]] || { echo "This release is for $expected; this computer is $actual." >&2; exit 1; }
fi
"$ROOT/scripts/install_runtime_deps.sh"
for app in navbridge-core navbridge-ui; do
  if ! libraries=$(ldd "$BIN_DIR/$app" 2>&1) || [[ $libraries == *'not found'* ]]; then
    printf 'Incompatible or missing runtime libraries for %s:\n%s\n' "$app" "$libraries" >&2
    exit 1
  fi
done

for group in plugdev audio dialout video render input; do
  getent group "$group" >/dev/null || groupadd --system "$group"
done
# SupplementaryGroups in each unit applies immediately without changing login groups.
install -d -m 0755 /opt/navbridge/bin /opt/navbridge/config
BACKUP="/var/backups/navbridge/$(date +%Y%m%d-%H%M%S)-$$"
install -d -m 0700 "$BACKUP"
for unit in navbridge-core navbridge-ui; do
  if [[ -f /etc/systemd/system/$unit.service ]]; then
    cp -a "/etc/systemd/system/$unit.service" "$BACKUP/"
  fi
  if [[ -f /opt/navbridge/bin/$unit ]]; then cp -a "/opt/navbridge/bin/$unit" "$BACKUP/"; fi
  # Atomic replacement works even if --no-start leaves the old process running.
  install -m 0755 "$BIN_DIR/$unit" "/opt/navbridge/bin/$unit.new"
  mv -f "/opt/navbridge/bin/$unit.new" "/opt/navbridge/bin/$unit"
done
for config in core frontend keyboard; do
  if [[ ! -e /opt/navbridge/config/$config.toml ]]; then
    source_config="$ROOT/config/$config.toml"
    [[ ! -f /etc/navbridge/$config.toml ]] || source_config="/etc/navbridge/$config.toml"
    install -m 0644 "$source_config" "/opt/navbridge/config/$config.toml"
  else
    echo "Preserving /opt/navbridge/config/$config.toml"
  fi
done
# Templates use /run/navbridge, not the developer launcher's temporary sockets.
# Fail rather than silently rewriting existing customized configurations.
for config in core frontend; do
  if ! grep -Eq '^[[:space:]]*socket_path[[:space:]]*=[[:space:]]*"/run/navbridge/core.sock"[[:space:]]*(#.*)?$' "/opt/navbridge/config/$config.toml"; then
    echo "Set socket_path = \"/run/navbridge/core.sock\" in /opt/navbridge/config/$config.toml and rerun." >&2
    exit 1
  fi
done
if [[ ! -e /opt/navbridge/config/display.env ]]; then
  if [[ -f /etc/navbridge/display.env ]]; then
    install -m 0644 /etc/navbridge/display.env /opt/navbridge/config/display.env
  else
    printf '%s\n' '# Optional: SLINT_DRM_OUTPUT, SLINT_DRM_MODE, SLINT_KMS_ROTATION, RUST_LOG' > /opt/navbridge/config/display.env
  fi
fi

RULE=/etc/polkit-1/rules.d/50-navbridge-wifi.rules
if [[ -f $RULE ]]; then cp -a "$RULE" "$BACKUP/"; fi
install -d -m 0755 /etc/polkit-1/rules.d
install -m 0644 "$STAGE/50-navbridge-wifi.rules" "$RULE"

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
echo "Installed for $INSTALL_USER. Configuration: /opt/navbridge/config; backups: $BACKUP"
echo "Logs: journalctl -u navbridge-core -u navbridge-ui -f"
echo "Restart: sudo systemctl restart navbridge-core navbridge-ui"
if (( ! START )); then echo "Services enabled for the next boot. Reboot when ready."; fi
