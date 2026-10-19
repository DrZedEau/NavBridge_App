# NavBridge

NavBridge is a wired Android Auto head unit for Raspberry Pi 5 / Compute Module 5,
with a Rust backend and a separate Slint KMS interface. Connect an Android phone
directly with a USB data cable, unlock it, and accept Android Auto setup prompts.

Wireless Android Auto works over NavBridge's own 5 GHz hotspot once the phone
is paired over Bluetooth. This build does not provide iPhone CarPlay or a
hands-free Bluetooth call-audio path.
Music, navigation/assistant playback and assistant microphone capture use the
PCM2912A USB codec. An external GNSS receiver shows on the System page and, when
connected as the phone connects, gives Android Auto the car's position. The
last System page shows the car's speed, engine speed, temperatures, ignition
and odometer from its I-Bus, through the carrier board's ATmega32U4.

## Architecture

- `navbridge-core`: Android Open Accessory USB discovery, GAL version exchange,
  TLS 1.2 peer verification, protobuf channel negotiation, keyboard input, night
  sensor, audio/video focus, H.264 decoding, ALSA audio, GPSD, the I-Bus and
  system metrics.
- `navbridge-ui`: amber Slint menu with status lights and a clock, Settings,
  four System pages, keyboard navigation and the projection view. Escape/Home returns to the menu and gives video focus
  back to the car, so the phone pauses its video; returning to Phone
  restores it while retaining the phone session and audio.
- Bounded JSON-lines IPC carries commands and snapshots through a Unix socket.
  Video uses a generation-aware `/dev/shm/navbridge-video.rgba` frame exchange
  rather than JSON. See [architecture](docs/ARCHITECTURE.md).
- Projection is H.264 at 800x480, downscaled with Lanczos to the 400x240 panel.
  FFmpeg normalizes full/limited color range. DPI and 30/60 fps are configurable.
- A dedicated USB reader uses 16 KB transfers and a bounded queue. The session
  worker handles replies without interrupting reads. USB unplug/error clears
  decoder, playback and capture state; a new connection negotiates fresh TLS
  and media sessions. The ADB interface is excluded.
- Stopping the core (SIGTERM or SIGINT) first asks the phone to end its
  session; a second signal exits at once. A phone left in accessory mode by an
  earlier run is reset at startup, so its first session starts cleanly.

## Build and run

Use native 64-bit Raspberry Pi OS Lite (Bookworm or Trixie):

```bash
cd ~/NavBridge_App
./scripts/install_build_deps.sh
./scripts/build_kms.sh
./scripts/run_kms.sh
```

Build as your normal user. On Raspberry Pi OS the build enables
`navbridge-core/rpi-ffmpeg` and `navbridge-ui/direct-kms`; Raspberry Pi FFmpeg
has additional pixel formats that require this compatibility feature, which
`build_kms.sh` finds in its headers. The runner requests sudo for KMS/input and
uses private temporary configuration. Stop another running NavBridge instance
before starting it.

### Development on a Debian or Ubuntu PC

The same scripts work on a Debian or Ubuntu desktop, such as Kubuntu 26.10 on
x86-64, with the UI in a window:

```bash
./scripts/install_build_deps.sh
./scripts/build_kms.sh
./scripts/run_desktop.sh
```

`install_build_deps.sh` asks apt for this release's runtime library names, adds
your account to `plugdev` (phones) and `dialout` (the I-Bus board) if needed,
which takes effect at your next login, and installs udev rules: Android phones,
and the I-Bus board, which ModemManager then leaves alone. With stock FFmpeg,
`build_kms.sh` builds the UI with both the window (`navbridge-ui/desktop`) and
the KMS backend. `run_desktop.sh` runs both as your account and sends sound
through the desktop's default device; `--codec` uses core.toml's USB codec
instead. A phone plugged into the PC starts Android Auto as on the Pi. Settings
cannot be kept there, as `/var/lib/navbridge` belongs to root: changes last
until the core stops, with a warning in its log. Wireless mode switches `wlan0`,
which most PCs do not have. `run_kms.sh` refuses to start inside the desktop,
which owns the display, and `install_systemd.sh` refuses to take tty1 from a
display manager unless given `--force`, for a dedicated kiosk PC.

| Script | Purpose |
| --- | --- |
| `install_build_deps.sh` | Rust, native build/runtime packages and udev rules. |
| `build_kms.sh` | Build binaries and a complete release archive. |
| `run_kms.sh` | Run the local build on the display (KMS); clean up both processes together. `--detach` runs it in the background. |
| `run_desktop.sh` | Run the local build in a window on a desktop PC. |
| `install_systemd.sh` | Deploy existing binaries, runtime packages, udev rules and two services. Never builds. |
| `install_runtime_deps.sh` | Runtime packages, GPSD and udev rules only. |

See [installation guide](docs/installation-lite.md) for releases and systemd.

## Configuration

`config/core.toml`:

```toml
socket_path = "/run/navbridge/core.sock"
snapshot_interval_ms = 500

[android_auto]
dpi = 160
fps = 60
left_hand_drive = true

[ibus]
enabled = true
device = "/dev/serial/by-id/usb-Arduino_LLC_Arduino_Micro_HIDPC-if00"

[audio]
enabled = true
playback_device = 'plug:SLAVE="dmix:CARD=CODEC,DEV=0"'
capture_device = "plughw:CARD=CODEC,DEV=0"
```

The default configuration requests 60 fps. Frame-rate wire values are 60=1 and
30=2, verified on the Pixel/CM5 during live comparison. Older AASDK definitions
reverse these values. The frontend loads each frame as soon as the core
publishes it (inotify on `/dev/shm`) and shows it at the panel's next refresh.
Measured animation rates can be below the requested maximum, especially when
the phone reduces updates for static screens. For a frontend update-rate log
every 5 seconds, start with `NAVBRIDGE_FRAME_STATS=1 ./scripts/run_kms.sh`.

Restart after changing projection settings in core.toml. DPI changes logical
UI density, not the video or physical display resolution; Settings switches it
between 160 and 140, and that choice replaces core.toml's. Audio defaults use the stable
ALSA card name `CODEC`: 48 kHz stereo music, 16 kHz mono speech/system streams
and 16 kHz mono microphone. ALSA `plug` performs conversion and `dmix` mixes
playback. HDMI card numbering does not affect selection. Acoustic echo
cancellation is not implemented.

For development, `head_unit_server = "127.0.0.1:5277"` in `[android_auto]` runs
the session over TCP while no phone is plugged in by USB, the transport that
wireless Android Auto uses. On the phone, enable Android Auto's developer
settings (tap the version 10 times), choose **Start head unit server** in its menu
and turn on wireless debugging. On the Pi, after `sudo apt install adb`, run
`adb pair` and `adb connect` with the addresses the phone shows, then
`adb forward tcp:5277 tcp:5277`. The core tries the server every 5 seconds.

Optional paired `certificate` and `private_key` PEM paths in `[android_auto]`
replace the built-in accessory identity. The phone certificate chain is checked
against the Google Automotive Link CA. Unknown settings are rejected, so a
misspelled key stops the core with an error instead of being silently ignored.

Android USB rules cover AOA and common phone vendors. For a vendor absent from
`packaging/70-navbridge-android.rules`, add an equivalent vendor rule, reload
udev and replug the phone. A phone-like device that cannot be opened or does not
support AOA is skipped until it is replugged, so it cannot hide a phone on
another port. Audio, Arduino and GNSS interfaces are not probed as phone
accessories.

## Keyboard

Bindings live in `config/keyboard.toml`.

| Key | Action |
| --- | --- |
| Arrows, W/D | Navigation (W = up, D = right) |
| Enter/Space | Select |
| Backspace | Android Auto back; back in NavBridge pages |
| Escape/Home | NavBridge menu without disconnecting |
| N / B | Next / previous track |
| V | Android Auto voice assistant |
| A / E | Answer / end or reject a call (phone-dependent; Bluetooth call audio is not implemented) |
| H | Android Auto home |
| M / S | Open the navigation app / media in Android Auto |
| P | Pause or resume music (the phone knows which) |
| Page Up / Page Down | Day / night mode from any page |
| Ctrl / Alt | Parking brake on / off from any page: Android Auto's driving restrictions off / on |
| Q | Quit |

In Android Auto, Up/Down switch focus sections; Left/Right step through items
using rotary input. Enter selects and Backspace goes back.

There are no touchscreen controls. Night mode starts in day mode and survives
phone reconnects until the core restarts.

Android Auto gets the parking brake and a driving status: with the brake on, no
driving restrictions, so the keyboard, long lists and settings work; with it
off, all of them. Until the first Ctrl or Alt after the core starts, the car
counts as moving, as the phone otherwise assumes. The state survives phone
reconnects like night mode. The board should tap the key (press and release)
when the brake changes, and once more when it powers up, rather than hold it:
held, Ctrl and Alt together with Backspace would quit the UI. The phone can
still restrict itself when it notices it is moving. Without a phone, the projection page
shows the connection status while the core keeps looking for one.

## Menu

The menu shows NavBridge and the core's status light at the top, three tiles in
the middle and, at the bottom, the BT, WIFI and GNSS lights and the time.
Left/Up and Right/Down move between the tiles, Enter opens one, Back or Escape
returns:

- **Phone**: the connected phone's screen, Android Auto in this build (CarPlay
  is not in it yet). While there is none: the Enable pairing switch at the top,
  dimmed while the hotspot is off, and in the middle "Waiting for phone" with
  the hotspot off, "Waiting for paired phone" with it on, or while pairing is
  open, the name to pair with and the time left.
- **Settings**: the hotspot for wireless Android Auto, the projection's DPI and
  frame rate, and the codec's speaker volume and microphone gain (see below).
- **System**: the System pages.

Lights are green when on and red when off. CORE: the UI is connected to the
core. BT: Bluetooth is ready for wireless phones. WIFI: the hotspot is up.
GNSS: the receiver has a fix; orange while a receiver is connected without one.
The time is the Pi's local time. The tiles' icons are in `crates/ui/ui/icons`.

## Settings

One page, in three sections. Left/Right (or Up/Down) choose a row. Enter flips
the Hotspot switch at once; on the other rows, it opens the row: it is outlined
and its value takes arrows. Left/Right then change the value, which takes effect
at once, and Enter or Back close the row again.

| Section | Row | Values | Notes |
| --- | --- | --- | --- |
| Wireless | Hotspot | Switch, on / off | Wireless Android Auto (see below) |
| Video | DPI | 160 / 140 | The phone gets it when it next connects |
| | FPS | 60 / 30 | The phone gets it when it next connects |
| Audio | Speakers volume | 0-100%, 1% steps | The codec's `Speaker` level; held keys repeat |
| | Microphone volume | 0-100%, 1% steps | The codec's `Mic` capture gain; held keys repeat |

The video choices apply when a phone next connects. The codec has fewer steps than 1%
(`Speaker` 0-76, `Mic` 0-42), so a press does not always change what is heard,
and the System page, which shows the mixer's own reading, can be 1% off, such as
14% for 15%. Choices are kept in
`/var/lib/navbridge/settings.toml`; the levels are set again whenever the codec
appears, as a power cut can lose ALSA's own copy.

## System pages

1. Compute Module 5: CPU/RAM usage and temperature. USB connection: detected/
   connected status, device model and the connected phone's name.
2. Video: UI resolution, projection resolution (Android Auto; CarPlay is not in
   this build) and the DPI and frame rate requested from the phone. Audio: the
   USB codec ALSA uses (name and USB id), its hardware sampling rates while
   playing or recording, and its ALSA mixer volume and microphone gain.
3. GNSS: GPSD receiver/port, connection/fix status, satellite counts, coordinates,
   MSL altitude, speed, HDOP and UTC fix time.
4. IBUS: the car's readings (see below).

Left/Up and Right/Down change System pages; four lights at the top centre show
which is open. GNSS readings expire after five
seconds. GPSD handles supported USB receivers and hotplug; unsupported adapters
need a stable `/dev/serial/by-id/` entry in `/etc/default/gpsd`. Do not select the
Arduino.

If GPSD reports a receiver when the phone connects, Android Auto is offered the
car's location and gets every fix (usually once a second): position, accuracy,
altitude, speed, and heading while moving. The core logs `Android Auto uses the
car's GNSS position` when the phone takes it. Without a receiver at connection,
the phone uses its own GPS; a receiver plugged in later counts from the next
connection.

## IBUS (System page 4)

The carrier board's ATmega32U4 hears the car's I-Bus (BMW E46 and E85
electrics) and passes each frame to the Pi over USB serial, one line each, such
as `IBUS1 RX 7323494 8004BF110329`: bus, direction, its milliseconds since it
started, and the frame in hex. The core reads `[ibus] device` in core.toml, the
board's stable `/dev/serial/by-id/` name, checks each frame's length and
checksum, and reopens the port whenever the board is unplugged and back. It
never sends anything towards the car.

From the instrument cluster's broadcasts, System page 4 shows:

| Tile | I-Bus message | Notes |
| --- | --- | --- |
| Speed | `18`, road speed | In steps of 2 km/h; the GNSS speed beside it while there is a fix |
| Engine | `18`, engine speed | In steps of 100 rpm |
| Coolant, Outside | `19` | °C |
| Ignition | `11` | Off, Position 1, On, Starting |
| Odometer | `17` | km, once something on the bus asks the cluster for it |

Speed and engine speed go back to n/a after 10 seconds without an update, or
when the ignition is off; everything does after 10 seconds without any I-Bus
traffic, as the car is then asleep. The line below the readings shows the link
and how many frames it carried; damaged frames point to a wiring or interference problem.
With `RUST_LOG=debug`, the core logs each reading that changes and, once each,
the kinds of message it does not decode, to find out what else the car sends.
The systemd service reads the port through the `dialout` group.

## Wireless Android Auto

The phone needs Android 11 or later with 5 GHz Wi-Fi; on some phones
"Wireless Android Auto" must also be enabled in the Android Auto settings. On
NavBridge:

- **Hotspot**, the switch in Settings' Wireless section. On: the built-in Wi-Fi becomes the hotspot
  `NavBridge-XXXX` (5 GHz, channel 36, WPA2, address 10.42.0.1) and Bluetooth
  takes the same name. NetworkManager's profile `navbridge-hotspot` is created on
  first use with a random password, which
  `sudo nmcli -s -g 802-11-wireless-security.psk connection show navbridge-hotspot`
  shows. Off, and whenever NavBridge stops: NetworkManager reconnects the saved
  home network; the hotspot never starts on its own, so a reboot also returns to
  it.
- **Enable pairing**, the switch at the top of the Phone page while no phone
  is connected and the hotspot is on: Enter switches pairing on or off. For 2 minutes, phones can find
  NavBridge. Pair from the phone's Bluetooth settings; NavBridge then shows the
  6-digit code. Enter pairs if the phone shows the same code; Back refuses.
  The switch goes off by itself once the 2 minutes are up or a phone has
  paired.

A paired phone gets the hotspot's details over Bluetooth, joins it and starts
Android Auto over Wi-Fi. While wireless mode is on and no phone is connected,
NavBridge calls paired phones every 20 seconds, except while pairing is open,
so Android Auto starts once the
phone is in range with Bluetooth and Wi-Fi on. With several paired phones, the
calls stop at the first whose Android Auto connects. A phone plugged in by USB still
comes first.

The choice is kept in `/var/lib/navbridge/settings.toml`, so the next start
brings the hotspot back. To start with wireless mode on whatever was saved,
pass `--wireless` to `navbridge-core` or `./scripts/run_kms.sh`. The UI can
still turn it off; that choice is saved for starts without the flag. While
wireless mode is on, the Pi is off the home network and SSH drops; start
NavBridge with `--detach` so that it keeps running without the SSH session:

```bash
./scripts/run_kms.sh --wireless --detach
```

It checks first, showing problems such as a NavBridge already running in the
terminal, then runs in the background with its output in `/tmp/navbridge.log`.
`sudo pkill -x navbridge-core` stops it. For a debug log, start it with
`RUST_LOG=debug` in front.

For the systemd service, `sudo systemctl edit navbridge-core` and add the flag
to the core's command line:

```ini
[Service]
ExecStart=
ExecStart=/opt/navbridge/bin/navbridge-core --config /opt/navbridge/config/core.toml --wireless
```

The systemd installer adds a polkit rule so that the service, which runs as
your user, may switch the Wi-Fi; Bluetooth needs no extra permission. Log lines,
in the order a connection goes through them: `Bluetooth ready for wireless
Android Auto` (with its RFCOMM channel), `Bluetooth pairing request`,
`Bluetooth phone paired`, `called the phone over Bluetooth`, `the phone's
Android Auto connected over Bluetooth`, `the phone has the hotspot's details`,
`wireless Android Auto phone connected`. Otherwise the warning `wireless Android
Auto setup failed` gives the reason, such as a phone that could not join the
hotspot. The call logs how long the phone took to answer (`ms`), and the
handshake how long it waited for the hotspot (`waited_ms`).

## Diagnostics and validation

```bash
RUST_LOG=debug ./scripts/run_kms.sh
cargo test --workspace --features navbridge-core/rpi-ffmpeg,navbridge-ui/direct-kms
```

To see the screens without the panel, the UI renders them with sample data into
`/tmp/navbridge-*.ppm`:

```bash
cargo test -p navbridge-ui --features navbridge-ui/direct-kms -- --ignored preview
```

Useful milestones: `Android accessory connected`, `Android Auto TLS authenticated`,
`projection video format established`, `PCM playback opened`. At debug level the
core also logs `projection video frames since the core started`, with the frames
decoded and dropped, every 5 seconds while the phone sends video.
The test suite includes real TLS exchanges, incremental USB framing and fragment
bounds, microphone permits, focus restoration and UI navigation/frame generations.
Real phone/display/audio confirmation remains necessary after protocol changes.

## Dependencies and attribution

The direct backend adds OpenSSL (runtime `libssl3` on Bookworm, `libssl3t64` on
Trixie), Rust protobuf 3.7.2 and its build-time pure-Rust code generator.
Wireless mode drives the system's BlueZ through the pure-Rust zbus D-Bus
library, and NetworkManager through `nmcli`; no Qt or Bluetooth libraries are
needed.
Existing FFmpeg, ALSA, libusb and Slint dependencies remain necessary.
No `protoc` executable is needed. `Cargo.lock` pins the build dependencies.

Protocol references and reused identity material are documented in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). NavBridge is GPL-3.0-or-later;
see [LICENSE](LICENSE).
