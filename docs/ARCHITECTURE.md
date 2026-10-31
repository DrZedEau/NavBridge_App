# NavBridge architecture

## Design rules

1. `navbridge-core` owns USB, the Android Auto session, media and future vehicle state.
2. `navbridge-ui` owns Slint presentation and keyboard and touch navigation
   only; touches on the projection go to the core, which passes them to the
   phone.
3. Control and state travel over a Unix domain socket as newline-delimited JSON.
4. Projection frames travel through tmpfs, never through the JSON socket.
5. The NavBridge UI is fixed at **400x240**. Android Auto runs at **800x480**;
   its DPI is configurable (default 160).
6. H.264 is decoded with FFmpeg: on the board's hardware decoder where there is
   one (Pi 4 / CM4), in software otherwise (Pi 5 / CM5).
7. Audio stays in the core.
8. The UI has no pointer or touch input.
9. Restarting the UI does not end the phone's session.
10. The core does not run on with a broken part: when the Android Auto worker
    or the video decoder thread stops for good, the core exits with an error,
    and a panic in any thread aborts it (release builds). systemd then starts
    it again (`Restart=on-failure`): after 2 seconds, then waiting longer each
    time it fails again, up to 30 seconds, so a failure that does not clear
    does not restart it every 2 seconds.

## Processes

```text
navbridge-ui                          navbridge-core
------------                          --------------
Slint 400x240 (KMS)                   USB discovery and AOA switch
keyboard mapping        JSON IPC      Android Auto session (TLS, channels)
menu, Settings, System <--------->    H.264 decode and scale (FFmpeg)
projection view                       ALSA playback and microphone
       ^                              GPSD client, system metrics
       |                              Wi-Fi hotspot, BlueZ (wireless)
       |                              I-Bus (ATmega32U4 serial)
       | /run/navbridge RGBA frames
       +----------------------------------------------
```

The socket defaults to `/run/navbridge/core.sock`, and the projection frames go
beside it, in `video.rgba`. systemd keeps that directory in tmpfs, open only to
the services' group (`RuntimeDirectoryMode=0770`), and keeps it while the core
restarts, so the UI's inotify watch on it holds. The run scripts use a private
temporary directory in tmpfs instead.

- UI to core: `hello`, `request_snapshot`, `set_night_mode`, `set_parking_brake`,
  `hide_projection`, `resume_projection`, `navigation_input`, `phone_action`,
  `set_wireless`, `set_dpi`, `set_volume`, `set_microphone`, `start_pairing` and
  `confirm_pairing`. Settings' choices (wireless mode, DPI, the codec's levels)
  are kept in `/var/lib/navbridge/settings.toml`, which each service changes
  without touching the others' keys. Hiding and resuming
  move the phone's video focus, so the phone stops sending video while the
  NavBridge menu is shown.
- Core to UI: `hello` (geometry check, and the DPI and frame rate requested
  from the phone), `snapshot` (every 500 ms or on request)
  and `projection_event` (phone connected or disconnected, streaming started,
  phone asked for the NavBridge menu).

The core's main loop sleeps in `poll()` on the listening socket, its clients and
two internal sockets that signal projection events and stop requests, so a
command is forwarded as soon as it arrives. The UI writes commands from its IPC
thread while a second thread blocks reading events and wakes the Slint loop for
each one.

## USB and the Android Auto session

- Discovery: devices with an MTP/PTP or ADB interface, or already in accessory
  mode, are switched to Android Open Accessory (AOA) mode. A device that cannot
  be opened or does not speak AOA is skipped until it is replugged, so it cannot
  hide a phone on another port.
- At startup, a phone still in accessory mode from an earlier run is reset so its
  first session starts fresh.
- A reader thread queues 16 KB bulk transfers. The session thread sleeps until USB
  data, a command or microphone audio arrives, and at least every 100 ms for pings.
- The session runs unchanged over TCP, with its own reader thread waking the
  session the same way: for phones on the hotspot (wireless mode), and for the
  phone's head unit server (Android Auto developer settings, through
  `adb forward`) when `head_unit_server` is set. USB comes first, then the
  hotspot, then the head unit server; one phone at a time.
- The session exchanges versions, runs TLS 1.2 (the phone's certificate chain is
  checked against the Google Automotive Link CA), answers service discovery and
  opens the input, sensor (night mode, parking brake and driving status, and
  location while a GNSS receiver is connected), video, three audio and
  microphone channels. The driving status has no restrictions while the parking
  brake is on and the car stands still, and all of them otherwise, the brake
  counting as off until first reported. While the I-Bus is read, the parking
  brake is the cluster's handbrake and any speed above 0 km/h restricts; the
  UI's parking brake is ignored. The GNSS service passes
  each GPSD fix straight to the session thread.
- On SIGTERM or SIGINT the core asks the phone to end the session and waits up to
  one second for its reply before exiting.

## Video

```text
Phone: one H.264 access unit per Android Auto message (SPS/PPS first)
    |
    v
FFmpeg decoder, one packet per message (no parser delay): h264_v4l2m2m
where the board has it, else the software h264; a failing hardware decoder
gives way to software, which starts from the last SPS/PPS and a new keyframe
    |
    v
libswscale (Lanczos): 800x480 -> 400x240 RGBA
    |
    v
/run/navbridge/video.rgba: header + pixels, replaced atomically
    |
    v
Slint image, loaded as soon as inotify reports the new file
```

The two geometries share the same aspect ratio, so no crop or letterbox is
needed. The decoder queue is shallow: if it overflows, frames are dropped until
the next IDR frame, which the core requests by taking the phone's video focus
away and giving it back.
Persistent `memfd`/`mmap` buffers would avoid the per-frame file replacement.

## Audio

Media (48 kHz stereo), speech such as guidance and the assistant (16 kHz mono)
and system sounds (16 kHz mono) go to ALSA through `plug` and `dmix`, so the
streams mix. Media is ducked while the phone holds transient audio focus. The
microphone is captured at 16 kHz mono and sent to the phone while it keeps the
microphone open. Neither audio thread polls: playback sleeps in `poll()` until
new samples arrive or ALSA has room for queued ones, and capture sleeps until
the phone opens the microphone, then blocks on each 20 ms read.

## Wireless mode

The Wi-Fi service switches `wlan0` with `nmcli` on its own thread, so the main
loop never waits for NetworkManager. On, it activates the `navbridge-hotspot`
profile: a 5 GHz access point in NetworkManager's shared mode, whose DHCP server
gives phones an address. The profile is created on first use and never
autoconnects. Off, and when the core stops, it deactivates the profile, and
NetworkManager's autoconnect returns to the saved home network; a crash or a
reboot also ends there. The choice is kept in `/var/lib/navbridge`; the core's
`--wireless` flag starts with it on without changing what is kept. A hotspot
that fails or drops is started again, waiting longer each time.

The Bluetooth service, also on its own thread, talks to BlueZ over D-Bus (zbus)
while wireless mode is on. It registers:

- a pairing agent (DisplayYesNo): BlueZ hands it the phone's 6-digit code, the
  UI shows it and the user's answer goes back. Pairing is refused unless the
  user opened it, and is closed again after 2 minutes or the first pairing;
- the Android Auto wireless service, on an RFCOMM channel the kernel finds free
  (BlueZ's own SIM access service holds 8, the channel wireless dongles use);
  phones look the channel up. Registration checks that BlueZ really listens
  there. BlueZ hands each connection over non-blocking; the handshake makes it
  wait, up to 20 seconds for each of the phone's answers. A phone that connects
  while the hotspot is still starting first waits, up to 15 seconds, for it.
  The handshake sends the phone the hotspot's address, port 5288, network name,
  password and BSSID; the phone joins the hotspot, reports whether it could,
  and opens the session on 10.42.0.1:5288, where the core listens from the
  start (IP_FREEBIND) and accepts it at once;
- a headset service: as soon as wireless mode is on, so that Bluetooth and the
  hotspot start together, again once the hotspot is up if no phone answered,
  and then every 20 seconds while no phone has a session or is setting one up,
  NavBridge connects to the paired phones as a headset, which makes the phone's
  Android Auto look for the wireless service. The headset connection is closed
  at once. Phones are called one after another until one's Android Auto
  connects; a phone that answered gets 3 seconds for that before the next is
  called, so that two phones do not set up at once.

Both services require a paired phone over an encrypted link.

## I-Bus

The I-Bus service reads the carrier board's ATmega32U4 on its own thread: one
line per frame the board hears, which the service checks (length, XOR
checksum) and decodes when the instrument cluster sends it: ignition (`11`),
road and engine speed (`18`), outside and coolant temperature (`19`),
odometer (`17`) and the handbrake (`13`, sensor status). It opens the port raw and exclusive, so GPSD cannot
probe it, and never at 1200 baud, which would restart the board into its
bootloader. It waits for bytes in `poll()`, at most 500 ms so that it can stop,
and reopens the port every 2 seconds while it is missing. Each reading keeps
when it was heard: speeds expire after 10 seconds, and every reading once the
bus has been quiet for 10 seconds or the board is gone. The `vehicle` part of
the snapshot carries the readings, the link's status and its frame counts.

The service also decodes the board monitor's buttons (`48`, its soft keys
`47` and knob `49`, from `F0`) and the steering wheel's (`3B` from `50`),
pressed, held or released, and maps them through `[ibus.buttons]` to the UI's
actions. Each press becomes a tap for the UI (holds and releases are not used
yet), sent through an event channel that wakes the main loop, which passes it on as
`ServerEvent::Input`; the UI handles it as a key, and ignores the keyboard
while the I-Bus is read. The port is opened for writing too, only to send the
board `KEYS OFF`. The board passes frames on only while NavBridge's screen
shows, except the cluster's ignition off: from that, the state keeps when the
ignition went off, through the bus falling quiet, and the main loop shuts the
Pi down through logind's `PowerOff` at its next snapshot, or once the ignition
has stayed off for `shutdown_delay_s`.

## Keyboard

`keyboard.toml` maps Slint key events to UI actions. In Android Auto, Up/Down
are D-pad keys and Left/Right are rotary steps; media, call, assistant and home
keys are sent as Android key codes, and the navigation and media app keys as
Android Auto's own car buttons. Select and Back send press and release together
when pressed, because Android acts on the release; they have no long press.

## Slint backends

- Raspberry Pi: LinuxKMS without a seat, software renderer
  (`navbridge-ui/direct-kms`).
- Desktop development: a winit window (`navbridge-ui/desktop`).

Both use the same `app.slint`.

## Next

- Bluetooth hands-free (HFP) for call audio.
- IBUS: on-board computer values (consumption, range), battery voltage and oil
  temperature.
- iPhone CarPlay.
