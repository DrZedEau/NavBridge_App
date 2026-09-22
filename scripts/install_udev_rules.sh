#!/usr/bin/env bash
set -euo pipefail

RULE=/etc/udev/rules.d/70-navbridge-carlinkit.rules
TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

cat > "$TMP" <<'RULES'
# Common Carlinkit USB vendor used by NavBridge automatic discovery.
SUBSYSTEM=="usb", ATTR{idVendor}=="1314", MODE="0660", GROUP="plugdev", TAG+="uaccess"
RULES

sudo install -m 0644 "$TMP" "$RULE"
sudo udevadm control --reload-rules
sudo udevadm trigger

if ! id -nG "$USER" | tr ' ' '\n' | grep -qx plugdev; then
  sudo usermod -aG plugdev "$USER"
  echo "Added $USER to plugdev; log out and back in before testing USB access."
fi

echo "Installed $RULE"
