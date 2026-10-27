# NavBridge on Raspberry Pi OS Lite

Use 64-bit Raspberry Pi OS Lite on Pi 5 / CM5 with a working KMS display.

| Script | Purpose |
| --- | --- |
| `install_build_deps.sh` | Native build tools, Rust, runtime libraries and USB udev rules. |
| `build_kms.sh` | Build KMS binaries and assemble a publishable release archive. |
| `run_kms.sh` | Run the locally built core and UI with temporary config and process cleanup. |
| `install_systemd.sh` | Install prebuilt binaries, runtime dependencies, udev rules and two services. Never compiles. |
| `install_runtime_deps.sh` | Runtime libraries, GPSD and udev rules only; no compiler or Rust. |

## Build and run from source

```bash
cd ~/NavBridge_App
./scripts/install_build_deps.sh
./scripts/build_kms.sh
./scripts/run_kms.sh
```

Build as your normal user, without sudo. The dependency installer requests sudo but installs Rust for the invoking account. Missing `Cargo.lock` is generated on the first build; retain it with the source. Subsequent builds use `--locked`. The manual runner escalates for direct KMS/input access, refuses to compete with another NavBridge process, and stops both children when either exits.

## Publish a release

The build produces `dist/navbridge-<bookworm-or-trixie>-arm64.tar.gz` and an unpacked `dist/navbridge/` containing:

```text
bin/navbridge-core
bin/navbridge-ui
config/*.toml
packaging/*.service
packaging/70-navbridge-android.rules
packaging/50-navbridge-wifi.rules
scripts/install_systemd.sh
scripts/install_runtime_deps.sh
release.info
README.md
THIRD_PARTY_NOTICES.md
LICENSE
```

Publish the complete archive. It contains the binaries, config and installation assets; users do not need source code, Cargo, Rust or development headers. Separate builds are required for Bookworm and Trixie because their FFmpeg/glibc versions differ. The installer checks the release's OS label, ARM64 ELF headers and available dynamic libraries. Existing unpacked bundles are moved aside on repeat builds.

## Install a published release

```bash
tar -xzf navbridge-trixie-arm64.tar.gz
cd navbridge
./scripts/install_systemd.sh
```

Run from SSH to start immediately. The installer takes binaries from `bin/` in a release, or `target/release/` in a checkout. Override with `--binary-dir /path/to/binaries` when needed. It never builds or downloads the application binaries. It installs runtime apt packages from the OS repositories; shared system libraries remain in the OS library directories.

Installed layout:

```text
/opt/navbridge/bin/navbridge-core
/opt/navbridge/bin/navbridge-ui
/opt/navbridge/config/core.toml
/opt/navbridge/config/frontend.toml
/opt/navbridge/config/keyboard.toml
/opt/navbridge/config/display.env
```

Systemd units are installed in `/etc/systemd/system/` with the invoking account as `User` and its primary group as `Group`, including when invoked through sudo. Direct root logins are rejected. Hardware groups are assigned through the services.

The installer also adds `/etc/polkit-1/rules.d/50-navbridge-wifi.rules`, which lets that account switch the Wi-Fi between the wireless Android Auto hotspot and the home network through NetworkManager. Settings that survive a restart (wireless mode, the projection's DPI, the codec's volume and microphone gain) are kept in `/var/lib/navbridge`.

Existing config under `/opt/navbridge/config` is preserved. For upgrades from the previous layout, existing `/etc/navbridge/*.toml` files are copied into the new directory if it has no corresponding file. Future edits belong under `/opt/navbridge/config`. Both core and frontend socket paths must be `/run/navbridge/core.sock`. Previous installed binaries and units are saved under `/var/backups/navbridge/`.

The UI reserves tty1 and disables its login console. SSH and other terminals remain available. Installation from tty1 automatically defers app startup until reboot. Panel overlays, timings and boot configuration are not modified.

## Preview, defer startup, and manage services

```bash
./scripts/install_systemd.sh --dry-run
./scripts/install_systemd.sh --no-start
./scripts/install_runtime_deps.sh --dry-run
sudo systemctl restart navbridge-core navbridge-ui
sudo systemctl stop navbridge-ui navbridge-core
systemctl status navbridge-core navbridge-ui
journalctl -u navbridge-core -u navbridge-ui -f
```

Dry runs do not install or activate anything. `--no-start` enables the app for next boot without restarting existing app processes; GPSD's socket is still enabled immediately. Stop manually launched apps before starting services. Optional Slint display settings belong in `/opt/navbridge/config/display.env`.

To restore the tty1 login console:

```bash
sudo systemctl disable --now navbridge-ui navbridge-core
sudo systemctl enable --now getty@tty1
```

Package installation requires internet access. A full clean-OS install and reboot test remains necessary before publishing a release.

## Wired Android Auto

Connect the Android phone directly to the Pi USB host port using a data cable.
iPhone CarPlay is not present in this release. Wireless Android Auto is switched
on from Settings, or at every start by adding `--wireless` to the
core's `ExecStart` (`sudo systemctl edit navbridge-core`); phones pair over
Bluetooth, with a code shown on screen, then connect over NavBridge's 5 GHz
hotspot. Configure `[android_auto]`
in core.toml; unknown settings stop
the core with an error. Music, navigation and assistant audio use the USB codec.
Bluetooth telephone call audio is not implemented. A connected GNSS receiver's
position is sent to Android Auto.

The Android USB rule grants plugdev access to AOA and common phone vendors; add
your phone vendor if absent, then replug the phone.
OpenSSL runtime libraries are included in the runtime dependency installer.
