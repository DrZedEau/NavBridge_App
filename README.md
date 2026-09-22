# NavBridge

NavBridge is a from-scratch Raspberry Pi 5 infotainment application with a **Rust backend** and a separate **Slint frontend**.

Two resolutions are intentionally used for different jobs:

- **NavBridge UI / physical presentation:** `400x240`
- **CarPlay / Android Auto projection session:** `800x480`, configurable DPI (default `160`)

The Carlinkit adapter and phone are configured for 800x480 with the DPI from `[carplay].dpi` (default 160). Projection video is decoded in software in `navbridge-core`, scaled to 400x240, and presented by the independent Slint process. The dongle is never negotiated at 400x240.

## Current milestone

Implemented:

- Cargo workspace with independent `navbridge-core` and `navbridge-ui` executables
- fixed 400x240 Slint frontend
- keyboard-only navigation and separate `config/keyboard.toml`
- main menu: AA / CarPlay, System, IBUS
- automatic Carlinkit-style USB discovery and selection
- reset -> re-enumerate -> claim-interface USB lifecycle
- Carlinkit framing and 2-second heartbeat
- protocol initialization at 800x480 with configurable DPI
- Android Auto work-mode initialization and re-arm (`androidWorkMode=1`)
- phone/session event handling and automatic AA/CarPlay screen navigation
- RequestHostUI / Exit returns to NavBridge Menu without tearing down the phone session
- **software-only FFmpeg H.264 parser/decoder**
- 800x480 projection stream -> software scale -> 400x240 RGBA presentation
- frame exchange through `/dev/shm/navbridge-video.rgba` (tmpfs, outside JSON IPC)
- System page: CPU usage, RAM usage, CPU temperature, dongle status, model and firmware
- JSON-lines control IPC over a Unix domain socket
- Slint LinuxKMS no-seat build for a dedicated Pi display / SSH development
- Raspberry Pi FFmpeg pixel-format compatibility enabled by the Pi build scripts (`rpi-ffmpeg`)
- USB stereo playback and mono microphone input for CarPlay/Android Auto
- keyboard press/release commands forwarded to CarPlay/Android Auto
- Lanczos downscaling with accurate rounding and full horizontal chroma interpolation
- no touchscreen UI actions
- no hardware H.264 decoder path

Still to implement:

- Acoustic echo cancellation (use headphones or suitable microphone/speaker placement for voice calls)
- IBUS protocol
- optional lower-copy video transport after the software path is proven on the Pi

## Architecture

```text
                         400 x 240
                 +-----------------------+
                 |     navbridge-ui      |
                 |        Slint          |
                 | Menu/System/IBUS      |
                 | AA/CarPlay frame      |
                 +-----------+-----------+
                             ^
             JSON IPC        |       /dev/shm RGBA frame
          state/commands      |
                             |
                 +-----------+-----------+
                 |    navbridge-core     |
                 |         Rust          |
                 +-----------+-----------+
                             |
                +------------+------------+
                |                         |
              libusb                    Linux
                |                   system inventory
                v
          Carlinkit protocol
       heartbeat + session state
                |
         H.264 type 0x06
                |
                v
        FFmpeg H.264 parser
                |
        software H.264 decoder
                |
       software scale to RGBA
                |
                v
              400x240
```

The control socket is deliberately low bandwidth. Video does **not** travel over JSON IPC.

## Repository layout

```text
crates/
  common/                 shared constants and IPC models
  core/                   USB/protocol/system/media backend
    src/services/media.rs software H.264 decode + frame publishing
  ui/                     Slint frontend + keyboard handling
    ui/app.slint
config/
  core.toml
  frontend.toml
  keyboard.toml
packaging/
  navbridge-core.service
  navbridge-ui.service
scripts/
  install_pi5.sh
  build_pi5.sh
  build_pi5_kms.sh
  run_pi5.sh
  run_pi5_kms.sh
```

## Raspberry Pi OS Lite installation with systemd

From your normal account in this checkout, preferably over SSH:

```bash
./scripts/install_lite.sh
```

This installs dependencies, builds as the invoking user, and installs separate
core/UI systemd services under that same account (including when invoked through
sudo). Configuration lives in `/etc/navbridge` and is preserved on repeat installs.
The dedicated UI replaces the tty1 login console. Use `--dry-run` to preview or
`--no-start` to enable the services for the next boot without starting them now.

See [the Lite installation guide](docs/installation-lite.md) for configuration,
logs, updates, backups, and restoring the console. Templates in `packaging/`
contain user/group placeholders and must be installed through the script.

## Raspberry Pi 5 setup

> The FFmpeg 9 Rust wrapper requires the `format` feature alongside `codec` and `software-scaling`. Install `libavformat-dev` even though projection decoding uses the H.264 parser directly.

The video milestone adds FFmpeg development headers and `libclang`, so run the installer again even if an older NavBridge build was already working:

```bash
./scripts/install_pi5.sh
./scripts/install_udev_rules.sh
```

Desktop/X11/Wayland development build:

```bash
./scripts/build_pi5.sh
./scripts/run_pi5.sh
```

Dedicated DRM/KMS display:

```bash
./scripts/build_pi5_kms.sh
RUST_LOG=debug ./scripts/run_pi5_kms.sh
```

`run_pi5_kms.sh` uses Slint's `backend-linuxkms-noseat` path and re-execs with `sudo` for direct DRM/input access when necessary.

## Run the processes manually

Terminal 1:

```bash
./target/release/navbridge-core --config config/core.toml
```

Terminal 2:

```bash
./target/release/navbridge-ui \
  --config config/frontend.toml \
  --keyboard config/keyboard.toml
```

For an unprivileged development run, `scripts/run_pi5.sh` creates a temporary socket under `/tmp` automatically.

## Keyboard configuration

`config/keyboard.toml` belongs to the frontend and is independent of backend configuration.

Default controls:

```text
Up / W            previous item or page
Down              next item or page
Left              previous item or page
Right / D         next item or page
Return / Space    select in phone UI / retry when disconnected
Backspace         back inside phone UI (back in NavBridge pages)
Escape / Home     return to NavBridge menu
Q                 quit UI
```

There is no `TouchArea`, Slint `Button`, mouse callback, or touch callback in the UI.

## Display and projection contract

The fixed constants live in `crates/common/src/lib.rs`:

```text
UI_WIDTH       = 400
UI_HEIGHT      = 240
MEDIA_WIDTH    = 800
MEDIA_HEIGHT   = 480
MEDIA_DPI      = 160  # default; overridden by carplay.dpi
```

`navbridge-ui` validates the geometry announced by `navbridge-core` when IPC connects. A frontend-size change therefore cannot silently change the CarPlay/AA protocol geometry.

## USB/session lifecycle

NavBridge follows the adapter lifecycle verified by current CPC200-CCPA protocol research:

1. detect the adapter
2. issue a USB reset
3. drop the old libusb handle
4. wait about 3 seconds
5. rediscover the re-enumerated adapter
6. claim interface 0 and choose its bulk endpoints
7. arm the heartbeat schedule without sending at t=0
8. send initialization commands with approximately 120 ms pacing
9. send the first heartbeat around t=2000 ms and every 2 seconds after that

Reads and writes intentionally use different timeouts. Reads remain short so the loop can service heartbeat scheduling; writes allow up to 3 seconds because the gadget can briefly stall while firmware services `/dev/usb_accessory`.

## Android Auto initialization

Android Auto has a separate work mode from the iPhone/CarPlay `Open` phone mode. NavBridge therefore sends Android mode through **both** known firmware paths before/with initialization:

```text
SendFile /etc/android_work_mode = 1
BoxSettings: "androidWorkMode": 1
BoxSettings: "androidAutoSizeW": 800
BoxSettings: "androidAutoSizeH": 480
```

The adapter can reset Android work mode to zero when a phone/session ends. NavBridge re-sends the Android settings after `Unplugged` / `Phase 0` and when AA/CarPlay is manually retried.

## USB read boundaries

Read the 16-byte transport header first, then exactly the declared payload length
(in bounded chunks). Large reads spanning both transfers caused malformed control
messages and lost H.264 configuration data on the tested 1314:1521 adapter,
resulting in FFmpeg `non-existing PPS` errors and no video. Partial reads are
retained by the message parser before requesting the remaining bytes.

## Projection video

The adapter's `VideoData (0x06)` payload consists of a 20-byte video subheader followed by the H.264 stream. USB read boundaries are **not** assumed to be H.264 packet boundaries.

The media worker therefore does:

```text
0x06 payload
  -> skip 20-byte video header
  -> FFmpeg H.264 parser (av_parser_parse2)
  -> FFmpeg software H.264 decoder
  -> libswscale software conversion/downscale
  -> RGBA 400x240
  -> /dev/shm/navbridge-video.rgba
  -> Slint Image
```

The decoder queue is deliberately shallow for projection latency. If it overflows, NavBridge drops dependent data until an IDR frame appears and requests a fresh keyframe from the adapter.

The tmpfs frame format is intentionally simple:

```text
4 bytes  magic "NBRG"
4 bytes  width  (400 LE)
4 bytes  height (240 LE)
8 bytes  frame sequence
384000 bytes RGBA pixels
```

This is an initial shared-memory transport for bring-up. It can later be replaced by persistent `memfd`/`mmap` buffers without changing the USB or Slint architecture.

## Automatic navigation

The frontend reacts to backend session events:

- phone-link evidence / `Plugged (0x02)` -> automatically show AA / CarPlay
- first `VideoData (0x06)` -> mark projection streaming and keep/show AA / CarPlay
- `Unplugged (0x04)` or `Phase 0` -> return to Menu
- `Control (0x08), value 3` / RequestHostUI -> return to Menu **without disconnecting the phone**

Some firmware can expose Bluetooth/media metadata before emitting the normal `Plugged` event. NavBridge treats those live-link signals as a navigation hint while still using `0x06` as the definitive proof that projection video is streaming.

## Useful debug messages

Run:

```bash
RUST_LOG=debug ./scripts/run_pi5_kms.sh
```

During a healthy Android Auto session, useful milestones are:

```text
Carlinkit phone Plugged event
Phone link active - waiting for projection video
first projection video packet
projection video format established
Carlinkit heartbeat sent
```

The AA/CarPlay status page also exposes heartbeat, received video packet, decoded frame and drop counters.

## Build troubleshooting

### `fontconfig.pc` not found

```bash
sudo apt update
sudo apt install -y libfontconfig-dev
```

### FFmpeg / bindgen dependency errors

Run the full installer and dependency check:

```bash
./scripts/install_pi5.sh
./scripts/check_pi5_deps.sh
```

The software decoder requires `clang`, `libclang-dev`, `libavcodec-dev`, `libavformat-dev`, `libavutil-dev`, and `libswscale-dev`. The Pi build scripts enable the Rust FFmpeg binding's `rpi-ffmpeg` compatibility feature because Raspberry Pi OS ships additional `SAND*` / `RPI4*` pixel formats. Normal desktop/CI builds leave that target-specific feature off.

## License

GPL-3.0-or-later. See `LICENSE`.

## Projection keyboard controls and image quality

On the AA/CarPlay screen, arrow keys (W/D also move up/right), Enter/Space, and Backspace are
forwarded as Carlinkit navigation button press/release commands. Escape and Home
return to NavBridge without disconnecting the phone. Enter retries connection
only when no phone is connected. Input is polled independently of the status
snapshot interval so keys do not wait for a 500 ms status update.

The physical panel is 400x240. The 800x480 source is downscaled using Lanczos,
accurate rounding and full horizontal chroma interpolation to preserve fine
text and colored edges. This improves local scaling; it does not change the
phone encoder bitrate or increase the panel's native resolution.

Protocol reference: https://github.com/lvalen91/CPC200-CCPA_resources/blob/main/documentation/02_Protocol_Reference/command_ids.md

The H.264 parser may emit its previous buffered frame without consuming the
current input. NavBridge retries those same bytes instead of discarding them;
discarding them caused alternate reference frames to be lost and made the image
look heavily compressed. A synthetic 24-frame regression fixture covers this.

Returning from System or the NavBridge menu to a connected AA/CarPlay screen
reopens the local view and requests a fresh frame (command 12), following
AutoKit's video-surface restore sequence. Android Auto's
Exit action releases video focus (501), so local navigation alone leaves its
last frame frozen. Reopening does not resend Android work-mode settings or
auto-connect commands. The backend also ignores connection retries while a
phone is connected or streaming, protecting sessions from stale UI snapshots.

## USB audio (PCM2912A)

Projection audio is enabled by default through the USB card `CODEC`. The ALSA
`plug`/`dmix` path converts incoming PCM sample rates and mixes stereo music with
mono navigation/call audio. HDMI card numbering does not affect this selection.
The microphone uses mono PCM at the phone-requested rate (16 kHz by default).
Capture starts for phone recording, assistant or call requests and stops at the
corresponding stop request, phone disconnect or backend shutdown. Microphone
samples are sent to the dongle and are not saved to disk. Audio continues while
viewing NavBridge's System menu.

Override `playback_device` and `capture_device` in `[audio]` in `config/core.toml`
for another ALSA card, or set `enabled = false` to disable both paths.
Playback defaults to `plug:SLAVE="dmix:CARD=CODEC,DEV=0"`; capture defaults to
`plughw:CARD=CODEC,DEV=0`. Phone audio routing is fixed to the host USB
codec: PCM2912A stereo output and microphone input. Dongle discovery uses USB
vendor/product information automatically. Charging is handled by hardware;
NavBridge sends no charging-mode commands.
Builds require `libasound2-dev` (included in the installer).

Use `aplay -l`, `arecord -l`, and `amixer -c CODEC` to check the card and its
Speaker/Mic capture switches. Debug logs report audio formats, stream starts,
and microphone packet counts. The optional hardware test briefly plays silence
and captures/discards microphone samples while both directions are open:

```sh
cargo test --release -p navbridge-core --features rpi-ffmpeg pcm2912a_full_duplex -- --ignored
```

Audio protocol reference: [node-carplay messages](https://github.com/rhysmorgan134/node-carplay/tree/master/src/modules/messages).

### Phone shortcuts

On the connected AA/CarPlay screen, shortcuts send once per press:

| Key | Action |
| --- | --- |
| N | Next track |
| B | Previous track |
| V | Activate Siri / Android Auto voice assistant (Gemini when available) |
| A | Answer incoming call |
| H | Phone home screen |
| P | Pause music |
| S | Resume music |

A/S no longer act as Left/Down. Arrow keys navigate; Escape/Home opens the
NavBridge menu. Bindings are configurable in `config/keyboard.toml`.

### CarPlay layout and color range

Startup sends full 800x480 `HU_VIEWAREA_INFO` and `HU_SAFEAREA_INFO` before
Open. The decoder normalizes legacy YUVJ formats with explicit full-range
metadata and honors frame color-range/color-space metadata when scaling.

### System page

Use Left/Up and Right/Down to cycle through three pages:

1. **Compute Module 5** and **Dongle**: CPU/RAM usage, CPU temperature, dongle status, model and firmware.
2. **Phone** and **Audio - Video**: phone name, projection type, wireless/wired transport, hardware audio sample rate, app and phone-video resolutions.
3. **GNSS Signal**: receiver/port, connection and fix status, satellites used/in view, latitude/longitude, MSL altitude, speed (km/h), HDOP, and UTC fix time.

Entering System opens page 1. Backspace or Escape returns to the menu.
Metrics refresh once per second. Closed playback shows Idle; missing phone
names or transport flags show Unknown. Audio rate is measured at the PCM2912A
hardware, and phone-video resolution comes from incoming video packets.


### Pair a new phone

Keep NavBridge running so it initializes the USB adapter. On the disconnected
AA/CarPlay screen, use arrows to choose **Pair phone** and press Enter. Enable
Bluetooth and Wi-Fi on the phone, select the Bluetooth name shown on screen,
and accept pairing and Android Auto/CarPlay prompts. A replacement dongle needs
its own pairing even if its advertised name matches the old one; forget an old
entry with that name if it prevents a fresh pairing.

Pair phone reapplies Android projection settings and requests CarPlay BLE
advertising (command 30). Android pairing uses the adapter's Bluetooth device;
phone approval is still required. Retry connection asks for reconnection to a
saved phone. Pairing controls are unavailable during an active projection session
and never erase the adapter's saved phones.

### Dongle Bluetooth and Wi-Fi names

Set the two independent names in `config/core.toml`:

```toml
[carplay]
bluetooth_name = "NavBridge"
wifi_name = "NavBridge"
```

NavBridge applies them on each dongle initialization before auto-connect, using
AutoKit's BluetoothDeviceName (13) and WifiDeviceName (14) commands. Restart the
app after editing. Use at most 15 ASCII letters/digits, spaces, hyphens or
underscores. Set either value to an empty string to leave that radio's existing
name unchanged. Invalid names are rejected before the USB service starts.
The pairing screen continues to show the Bluetooth name reported by the dongle.
Names are not resent for phone retries or when returning from the System menu.


### USB GNSS

Run `./scripts/setup_gnss.sh` once on an existing installation, then reconnect the USB GNSS receiver. The regular Pi installer also installs GPSD. NavBridge connects to GPSD on localhost port 2947 and retries automatically if it is temporarily unavailable. GPSD handles receiver protocols and USB hotplug; NavBridge does not assume a particular model or tty number and does not probe other serial devices (such as the Arduino).

GPSD-supported receivers covered by the distribution's USB hotplug rules are detected automatically. For an unrecognized USB adapter, configure its stable `/dev/serial/by-id/...` path in GPSD's `DEVICES` setting in `/etc/default/gpsd`, following GPSD documentation, and restart GPSD. Never select the Arduino port. Keep `USBAUTO="true"` for supported USB receivers.

System page 3 distinguishes GPSD unavailable, receiver not detected, waiting for data, no fix, 2D/3D fix, and stale data. Position and UTC are shown only with a fix; MSL altitude requires a 3D fix and an `altMSL` report. Readings expire after five seconds without corresponding reports. Empty fields display `n/a`. HDOP is a dimensionless geometry indicator (lower is better), not a distance accuracy estimate. A clear view of the sky may be needed for the first fix. With multiple receivers the app selects one and keeps their reports separate.

Tab toggles AA/CarPlay day/night mode from any NavBridge screen (one toggle per press). The app starts in day mode and retains the selection across dongle reconnects until the core restarts. There is no `night_mode` setting in `core.toml`; the shortcut is mapped by `toggle_night_mode` in `keyboard.toml`.

Set `dpi = 160` in the `[carplay]` section of `config/core.toml` to control the logical density advertised to both Android Auto and CarPlay. The default remains 160. Higher density generally gives larger interface elements; video resolution stays 800x480 and the NavBridge UI stays 400x240. Restart NavBridge after changing DPI so the dongle and phone negotiate the new setting. Actual layout depends on the phone/projection software.


### External GNSS forwarding to Carlinkit

NavBridge automatically forwards a connected GPSD receiver's valid 2D/3D fixes while a phone is connected. It converts complete fixes to checksummed GPGGA and GPRMC NMEA sentences, using the dongle's type `0x29` packet with a little-endian length prefix. Updates are capped at 1 Hz. No location data is sent without a receiver, without a valid fix/time, or after five seconds without a fresh fix. Device removal or GPSD disconnect clears the fix, and forwarding is stopped. A new valid fix resumes forwarding automatically. Startup advertises GGA/RMC capability and initially stops forwarding to avoid reusing cached data. Coordinates are not written to routine logs.

This supplies external location to the dongle; the phone decides whether to use it. Some Carlinkit firmware has an Android Auto timestamp-conversion issue that can cause AA to ignore external fixes. Host-side successful USB delivery does not establish that the phone selected the external location source.
