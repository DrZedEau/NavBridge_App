#!/usr/bin/env bash
# Runtime libraries only: no Rust, compiler, or development headers.
# Raspberry Pi OS (bookworm, trixie), and other Debian or Ubuntu releases for
# development.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "${1:-}" in
  --help|-h) echo "Usage: $0 [--dry-run]"; exit 0 ;;
  --dry-run|'') ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || exit 2
command -v apt-get >/dev/null && command -v dpkg >/dev/null || { echo 'Requires a Debian or Ubuntu system (apt and dpkg).' >&2; exit 1; }
if [[ ${1:-} != --dry-run ]] && (( EUID != 0 )); then exec sudo -- "$0" "$@"; fi
# Read the OS release without executing a release-bundle supplied file.
CODENAME=$(. /etc/os-release; printf '%s' "${VERSION_CODENAME:-}")
# The runtime library that a development package depends on, as this release
# names it: libavcodec62 for libavcodec-dev on Ubuntu 26.10.
runtime_of() {
  apt-cache depends "$1" 2>/dev/null \
    | awk -v name="$2" '$1 ~ /^[|]?Depends:$/ && $2 ~ "^" name "[0-9]" { print $2; exit }'
}
# Fresh package lists, unless only looking.
[[ ${1:-} == --dry-run ]] || apt-get update
case "$CODENAME" in
  trixie) MEDIA=(libssl3t64 libasound2t64 libavcodec61 libavutil59 libswscale8) ;;
  bookworm) MEDIA=(libssl3 libasound2 libavcodec59 libavutil57 libswscale6) ;;
  *)
    # Other Debian and Ubuntu releases: ask apt.
    MEDIA=()
    for pair in libssl-dev:libssl libasound2-dev:libasound libavcodec-dev:libavcodec \
        libavutil-dev:libavutil libswscale-dev:libswscale; do
      library=$(runtime_of "${pair%%:*}" "${pair#*:}")
      [[ -n $library ]] || { echo "Cannot find the runtime library of ${pair%%:*} on $CODENAME." >&2; exit 1; }
      MEDIA+=("$library")
    done ;;
esac
# The KMS UI renders in software: no X11, Wayland, EGL or GBM libraries.
PACKAGES=("${MEDIA[@]}" libc6 libstdc++6 libgcc-s1 libusb-1.0-0 libudev1 libinput10
  libxkbcommon0 libdrm2 libfontconfig1 libfreetype6 fonts-dejavu-core
  gpsd gpsd-clients udev procps)
RULES=(70-navbridge-android.rules 71-navbridge-ibus.rules)
if [[ ${1:-} == --dry-run ]]; then
  printf 'Runtime packages for %s (%s):\n' "${CODENAME:-unknown}" "$(dpkg --print-architecture)"
  printf '%s\n' "${PACKAGES[@]}"
  echo 'Also installs Android phone and I-Bus board udev rules, creates hardware groups and enables GPSD USB hotplug.'
  exit 0
fi
for rule in "${RULES[@]}"; do
  [[ -f $ROOT/packaging/$rule ]] || { echo "Missing packaging/$rule" >&2; exit 1; }
done
apt-get install -y --no-install-recommends "${PACKAGES[@]}"
for group in plugdev audio video render input; do
  getent group "$group" >/dev/null || groupadd --system "$group"
done
for rule in "${RULES[@]}"; do
  install -m 0644 "$ROOT/packaging/$rule" "/etc/udev/rules.d/$rule"
done
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
