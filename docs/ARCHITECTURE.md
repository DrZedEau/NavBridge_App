# NavBridge architecture

## Design rules

1. `navbridge-core` owns USB, protocol, media and future vehicle state.
2. `navbridge-ui` owns Slint presentation and keyboard navigation only.
3. Control/state communication uses a Unix domain socket.
4. Projection frames use shared-memory/tmpfs, never JSON IPC.
5. The shell/display UI is fixed at **400x240**.
6. CarPlay/Android Auto is fixed at **800x480 @ 160 DPI**.
7. H.264 decoding is software-only on Raspberry Pi 5.
8. Audio remains in the backend.
9. The frontend has no pointer/touch actions.
10. Restarting the UI must not intentionally tear down the dongle/phone session.

## Process boundary

```text
navbridge-ui                          navbridge-core
------------                          --------------
Slint 400x240                         USB/libusb
keyboard mapping                      Carlinkit protocol
menu state              JSON IPC      Android/CarPlay session
system presentation   <--------->     system inventory
CarPlay presentation                  FFmpeg H.264 parser/decoder
       ^                              software scale
       | /dev/shm RGBA                 PCM audio future
       +-----------------------------  IBUS future
```

The control socket defaults to `/run/navbridge/core.sock` (development launchers use `/tmp`). Messages are newline-delimited JSON because the control volume is small and readable traces are useful.

## Resolution boundary

```text
PHONE / DONGLE
    |
    | protocol negotiation
    v
800 x 480 @ 160 DPI
    |
    | H.264 type 0x06
    v
FFmpeg parser + software decoder
    |
    | libswscale
    v
400 x 240 RGBA
    |
    | /dev/shm/navbridge-video.rgba
    v
Slint Image
```

The two geometries are deliberately independent. The aspect ratio is identical, so presentation needs no crop or letterbox.

## Video latency policy

Projection is interactive UI, not file playback. The decoder path therefore uses a small bounded queue. Queue overflow starts a drop-until-IDR mode and triggers a keyframe request. It does not try to accumulate and replay stale frames.

The current inter-process frame transport is an atomic file replacement on tmpfs. This is simple and robust for bring-up. A later optimization can move to persistent `memfd`/`mmap` buffers and sequence metadata without changing the USB/protocol layer.

## Audio

Audio packets do not need to enter the Slint process:

```text
USB PCM -> backend audio router -> PipeWire/ALSA -> speakers
                         |
                         +-> state/volume/source -> IPC -> UI
```

Audio routing is the next media milestone.

## Keyboard

`keyboard.toml` maps Slint logical key events to semantic UI actions. Future projection controls will use semantic IPC actions rather than exposing physical key codes to the backend.

## Slint backends

Development:

```text
Winit -> X11/Wayland -> 400x240 window
```

Dedicated Pi:

```text
Slint LinuxKMS no-seat + software renderer -> DRM/KMS -> panel
```

The Slint file is shared by both modes.

## Stages

### Stage 1 — shell (complete)

- split processes
- Slint 400x240 frontend
- IPC
- USB discovery
- System pages
- fixed 800x480/160 media contract

### Stage 2 — dongle/session transport (complete)

- reset/re-enumerate lifecycle
- endpoint discovery
- protocol framing
- heartbeat every 2 seconds
- initialization at 800x480/160
- Android work mode re-arm
- phone/session state
- RequestHostUI -> Menu while connection remains alive

### Stage 3 — projection video (current/implemented)

- raw `0x06` video handling
- FFmpeg H.264 parser
- software H.264 decoder
- software scale to 400x240 RGBA
- tmpfs frame exchange
- Slint presentation
- shallow queue + keyframe recovery

### Stage 4 — audio and controls

- PCM audio routing
- semantic control IPC
- keyboard -> CarPlay/AA controls

### Stage 5 — IBUS

- transport abstraction
- frame parsing
- vehicle state model
- UI pages and control integration
