#!/usr/bin/env bash
set -euo pipefail
sudo apt-get update
sudo apt-get install -y gpsd gpsd-clients
sudo systemctl enable --now gpsd.socket
printf '%s\n' 'Reconnect the USB GNSS receiver. GPSD uses its packaged USB hotplug rules.' 'If not detected, check /etc/default/gpsd has USBAUTO="true".'
