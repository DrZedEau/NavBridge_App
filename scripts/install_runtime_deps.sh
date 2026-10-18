#!/usr/bin/env bash
# Runtime libraries only: no Rust, compiler, or development headers.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "${1:-}" in
  --help|-h) echo "Usage: $0 [--dry-run]"; exit 0 ;;
  --dry-run|'') ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || exit 2
[[ $(uname -m) == aarch64 && $(dpkg --print-architecture) == arm64 ]] || { echo 'Requires 64-bit Raspberry Pi OS.' >&2; exit 1; }
# Read the OS release without executing a release-bundle supplied file.
CODENAME=$(. /etc/os-release; printf '%s' "${VERSION_CODENAME:-}")
case "$CODENAME" in
  trixie) MEDIA=(libssl3t64 libasound2t64 libavcodec61 libavutil59 libswscale8) ;;
  bookworm) MEDIA=(libssl3 libasound2 libavcodec59 libavutil57 libswscale6) ;;
  *) echo "Unsupported OS release: $CODENAME (supported: bookworm, trixie)." >&2; exit 1 ;;
esac
# The KMS UI renders in software: no X11, Wayland, EGL or GBM libraries.
PACKAGES=("${MEDIA[@]}" libc6 libstdc++6 libgcc-s1 libusb-1.0-0 libudev1 libinput10
  libxkbcommon0 libdrm2 libfontconfig1 libfreetype6 fonts-dejavu-core
  gpsd gpsd-clients udev procps)
if [[ ${1:-} == --dry-run ]]; then
  printf 'Runtime packages for %s:\n' "$CODENAME"
  printf '%s\n' "${PACKAGES[@]}"
  echo 'Also installs Android phone udev rules, creates hardware groups and enables GPSD USB hotplug.'
  exit 0
fi
if (( EUID != 0 )); then exec sudo -- "$0" "$@"; fi
[[ -f $ROOT/packaging/70-navbridge-android.rules ]] || { echo 'Missing packaging/70-navbridge-android.rules' >&2; exit 1; }
apt-get update
apt-get install -y --no-install-recommends "${PACKAGES[@]}"
for group in plugdev audio video render input; do
  getent group "$group" >/dev/null || groupadd --system "$group"
done
install -m 0644 "$ROOT/packaging/70-navbridge-android.rules" /etc/udev/rules.d/70-navbridge-android.rules
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --action=change
udevadm settle
if [[ -f /etc/default/gpsd ]]; then
  if grep -q '^USBAUTO=' /etc/default/gpsd; then
    sed -i 's/^USBAUTO=.*/USBAUTO="true"/' /etc/default/gpsd
  else
    printf '\nUSBAUTO="true"\n' >> /etc/default/gpsd
  fi
fi
systemctl enable --now gpsd.socket
echo 'Runtime dependencies and udev rules installed.'
