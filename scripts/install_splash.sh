#!/usr/bin/env bash
# Installs the boot splash: the NavBridge image on the panel from the moment
# the display driver starts until the UI takes the display. Run it after
# install_systemd.sh, from a release bundle or a build.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BIN_DIR="$ROOT/bin"
[[ -d $BIN_DIR ]] || BIN_DIR="$ROOT/target/release"
IMAGE="$ROOT/packaging/splash.png"
BOOT_CONFIG=1
DRY_RUN=0
UNINSTALL=0
ARGS=("$@")
usage() {
  cat <<EOF
Usage: $0 [--image FILE.png] [--binary-dir DIR] [--no-boot-config] [--dry-run]
       $0 --uninstall [--dry-run]

Shows FILE.png (default: packaging/splash.png, 400x240) on the panel while the
Pi boots, stretched to the panel's size if it differs.

Unless --no-boot-config is given, it also edits the boot configuration, keeping
a copy of each file: in cmdline.txt, the kernel's console moves from tty1 to
tty3 and quiet, loglevel=3, logo.nologo and vt.global_cursor_default=0 are set,
so no text or cursor shows before the splash; config.txt gets
disable_splash=1 (no firmware rainbow square).

--uninstall removes the splash and puts back the boot files it changed, unless
they were edited since.
EOF
}
while (( $# )); do
  case "$1" in
    --image) [[ $# -ge 2 ]] || { echo 'Missing image file' >&2; exit 2; }; IMAGE=$(realpath -m -- "$2"); shift 2 ;;
    --binary-dir) [[ $# -ge 2 ]] || { echo 'Missing binary directory' >&2; exit 2; }; BIN_DIR=$(realpath -m -- "$2"); shift 2 ;;
    --no-boot-config) BOOT_CONFIG=0; shift ;;
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
# The boot files as they were before the first install, and a checksum of
# what this script last wrote, so --uninstall restores only unedited files.
RECORD=/var/backups/navbridge/splash

BOOT=""
for dir in /boot/firmware /boot; do
  if [[ -f $dir/cmdline.txt ]]; then BOOT=$dir; break; fi
done

# The kernel command line with the splash's settings: changed where they
# are, the missing ones added at the end.
splash_cmdline() {
  local words=() kept=() word
  read -r -a words < "$1"
  for word in "${words[@]}"; do
    case $word in
      console=tty1) word=console=tty3 ;;
      loglevel=*) word=loglevel=3 ;;
      vt.global_cursor_default=*) word=vt.global_cursor_default=0 ;;
    esac
    case $word in
      console=tty3|quiet|loglevel=3|logo.nologo|vt.global_cursor_default=0)
        if [[ " ${kept[*]} " == *" $word "* ]]; then continue; fi ;;
    esac
    kept+=("$word")
  done
  for word in quiet loglevel=3 logo.nologo vt.global_cursor_default=0; do
    [[ " ${kept[*]} " == *" $word "* ]] || kept+=("$word")
  done
  printf '%s\n' "${kept[*]}"
}

# config.txt with disable_splash=1.
splash_config() {
  if grep -Eq '^[[:space:]]*disable_splash[[:space:]]*=' "$1"; then
    sed -E 's/^([[:space:]]*disable_splash[[:space:]]*=).*/\11/' "$1"
  else
    cat -- "$1"
    printf '\n# NavBridge boot splash: no firmware rainbow square.\n[all]\ndisable_splash=1\n'
  fi
}

if (( DRY_RUN )); then
  if (( UNINSTALL )); then
    echo "Will remove $SERVICE, $RULE, $BINARY and $SPLASH."
    echo "Will put back the boot files recorded in $RECORD, where they were not edited since."
    exit 0
  fi
  echo "Binary: $BIN_DIR/navbridge-splash -> $BINARY"
  echo "Image: $IMAGE -> $SPLASH"
  echo "Will install $SERVICE and $RULE (started by udev when /dev/fb0 appears)."
  if (( ! BOOT_CONFIG )); then
    echo "Will leave the boot configuration as it is."
  elif [[ -z $BOOT ]]; then
    echo "No cmdline.txt in /boot/firmware or /boot: not a Raspberry Pi; the boot configuration stays as it is."
  else
    echo "$BOOT/cmdline.txt now:"
    sed 's/^/  /' "$BOOT/cmdline.txt"
    echo "$BOOT/cmdline.txt after:"
    splash_cmdline "$BOOT/cmdline.txt" | sed 's/^/  /'
    if [[ -f $BOOT/config.txt ]]; then
      echo "$BOOT/config.txt changes:"
      diff <(cat -- "$BOOT/config.txt") <(splash_config "$BOOT/config.txt") | sed 's/^/  /' || true
    fi
  fi
  exit 0
fi
if [[ $EUID != 0 ]]; then
  exec sudo -- "$0" "${ARGS[@]}"
fi
[[ -d /run/systemd/system ]] || { echo "A booted systemd system is required." >&2; exit 1; }

# Writes `content` (stdin) to the boot file `name`, keeping the original from
# before the first install and a checksum of what was written.
write_boot_file() {
  local name=$1 file=$BOOT/$1 next
  install -d -m 0700 "$RECORD"
  next=$(mktemp)
  cat > "$next"
  if cmp -s "$next" "$file"; then
    echo "$file already has the splash's settings."
  else
    [[ -f $RECORD/$name ]] || cp -a -- "$file" "$RECORD/$name"
    cp -a -- "$file" "$RECORD/$name.previous"
    # Keeps the file's owner and mode; the FAT boot partition has neither.
    cat -- "$next" > "$file"
    sync -f "$file" 2>/dev/null || sync
    echo "Updated $file (original kept in $RECORD/$name)."
  fi
  sha256sum < "$file" | cut -d' ' -f1 > "$RECORD/$name.written"
  rm -f -- "$next"
}

if (( UNINSTALL )); then
  rm -f -- "$RULE" "$SERVICE" "$BINARY" "$SPLASH"
  systemctl daemon-reload
  udevadm control --reload-rules
  for name in cmdline.txt config.txt; do
    [[ -n $BOOT && -f $RECORD/$name ]] || continue
    if [[ $(sha256sum < "$BOOT/$name" | cut -d' ' -f1) == "$(cat -- "$RECORD/$name.written" 2>/dev/null)" ]]; then
      cat -- "$RECORD/$name" > "$BOOT/$name"
      rm -f -- "$RECORD/$name" "$RECORD/$name.written" "$RECORD/$name.previous"
      echo "Restored $BOOT/$name."
    else
      echo "$BOOT/$name was edited since the splash was installed: left as it is; the original is $RECORD/$name." >&2
    fi
  done
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
if (( BOOT_CONFIG )); then
  if [[ -z $BOOT ]]; then
    echo "No cmdline.txt in /boot/firmware or /boot: the boot configuration stays as it is."
  else
    splash_cmdline "$BOOT/cmdline.txt" | write_boot_file cmdline.txt
    if [[ -f $BOOT/config.txt ]]; then splash_config "$BOOT/config.txt" | write_boot_file config.txt; fi
    if grep -qw splash "$BOOT/cmdline.txt" && command -v plymouth >/dev/null; then
      echo "Plymouth is installed and cmdline.txt asks for its splash: remove 'splash' from it to keep Plymouth off the panel." >&2
    fi
  fi
fi
echo "Splash installed: $SPLASH. Reboot to see it."
echo "Try it now, over SSH: sudo systemctl stop navbridge-ui; sudo systemctl start navbridge-splash"
echo "Then: sudo systemctl start navbridge-ui. Remove it: $0 --uninstall"
