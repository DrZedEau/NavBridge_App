# Install on Raspberry Pi OS Lite

Use a current **64-bit Raspberry Pi OS Lite** installation on a Pi 5 / Compute Module 5, with networking and a working KMS display. Copy or clone the current NavBridge source, including `Cargo.lock`, into a directory writable by your normal user. The script builds the application on the Pi; it is not a standalone binary downloader.

Run from SSH if you want the display services to start immediately:

```bash
cd ~/NavBridge-Slint
./scripts/install_lite.sh
```

The script requests sudo for installation. Both `./scripts/install_lite.sh` and `sudo ./scripts/install_lite.sh` select your normal invoking account for the services. A direct root login is rejected because it does not identify a normal user. Rust installation and compilation run as that account, never as root.

## What it installs

- Native build dependencies, Rust stable, GPSD and its USB hotplug support.
- Release binaries at `/opt/navbridge/bin/navbridge-core` and `/opt/navbridge/bin/navbridge-ui`.
- Configuration copied initially from the checkout to `/etc/navbridge/`.
- USB rules for Carlinkit vendors `1314` and `08e4`.
- `navbridge-core.service` and `navbridge-ui.service`, enabled at boot.

Core gets USB/audio supplementary groups. UI gets video/render/input groups and owns `tty1`. The normal tty1 login service is disabled so it cannot compete with the dedicated display. SSH and other virtual terminals remain available. If invoked from tty1 itself, installation automatically defers starting NavBridge until reboot to avoid terminating the installer session.

The UI uses Slint's software KMS backend. It does not need a desktop or a display manager. Display-specific overlays and panel timings remain your OS configuration; this script does not overwrite `/boot/firmware/config.txt`. See [Raspberry Pi display configuration](https://www.raspberrypi.com/documentation/computers/config_txt.html).

## Preview or defer startup

```bash
./scripts/install_lite.sh --dry-run
./scripts/install_lite.sh --no-start
```

`--dry-run` prints the selected user, installation plan and generated services without installing anything or requesting sudo. `--no-start` installs and enables the app for the next boot; it does not restart an already running copy. GPSD's socket is still enabled immediately. Stop manually launched developer copies before starting the services.

## Configure and operate

```bash
sudo nano /etc/navbridge/core.toml
sudo nano /etc/navbridge/keyboard.toml
sudo systemctl restart navbridge-core navbridge-ui
systemctl status navbridge-core navbridge-ui
journalctl -u navbridge-core -u navbridge-ui -f
```

`/etc/navbridge/frontend.toml` contains UI settings. Both core and frontend must use `socket_path = "/run/navbridge/core.sock"`; the installer checks this. `/etc/navbridge/display.env` accepts optional Slint display environment settings such as `SLINT_DRM_OUTPUT`, `SLINT_DRM_MODE`, `SLINT_KMS_ROTATION`, and `RUST_LOG`.

Edit installed files under `/etc/navbridge`, since service runs do not read the checkout's `config/` files. Repeat installation preserves existing configuration and saves previous installed binaries and service units under `/var/backups/navbridge/`. Stop both services before manually restoring binaries from a backup, then run `sudo systemctl daemon-reload` if you restored service files, and start both services again.

The core and UI restart after failures. The UI can remain running and reconnect if the core is temporarily unavailable. Do not run the developer launcher alongside the services: both would compete for the dongle and display.

To return tty1 to a login console:

```bash
sudo systemctl disable --now navbridge-ui navbridge-core
sudo systemctl enable --now getty@tty1
```

## Validation status

The installer has been checked with Bash syntax validation and its read-only dry run on the development Pi. Generated systemd units are checked with `systemd-analyze verify`; the installer also checks the real installed paths before enabling them. A full fresh-OS install and reboot test is still required. Creating the installer does not automatically install or activate these services on the development Pi.
