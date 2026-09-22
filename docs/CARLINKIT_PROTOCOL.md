# Carlinkit protocol subset used by NavBridge

NavBridge implements the transport/session path required to keep the adapter alive, negotiate Android Auto/CarPlay and consume projection H.264 video.

## Framing

Each message begins with a 16-byte little-endian header:

```text
u32 magic      0x55aa55aa (cleartext)
i32 length     payload length
u32 type
u32 typecheck  bitwise NOT of type
```

The header and payload are written over the adapter's bulk OUT endpoint.

## USB lifecycle and heartbeat

`HeartBeat` is message type `170 / 0xAA`, zero-byte payload.

```text
detect -> USB reset -> close old handle -> wait ~3 s -> rediscover
       -> claim interface 0 -> arm heartbeat timer -> initialize
       -> first heartbeat at ~2000 ms -> every 2000 ms thereafter
```

The reset commonly changes the USB address; the pre-reset handle is discarded. Reads use a short timeout while writes allow 3000 ms.

The capture-verified accessory transport commonly uses bulk IN `0x83` / OUT `0x02`, but NavBridge intentionally falls back to interface-0 descriptor endpoints because the tested `1314:1521` firmware exposes `0x81` / `0x01`.

## Initialization

The sequence follows the known-working FastCarPlay/AutoKit ordering closely:

```text
SendFile /tmp/screen_dpi = 160
SendFile /etc/android_work_mode = 1
Open 800x480 @ configured FPS
BoxSettings JSON
DayNight JSON
SendFile /tmp/night_mode
SendFile /tmp/charge_mode
SendFile /etc/box_name
SendFile /tmp/hand_drive_mode
control: microphone
control: Wi-Fi band
control: audio route
control 1002: auto-connect
```

Android Auto has a separate firmware work mode from the iPhone/CarPlay phone mode in `Open`. For that reason `BoxSettings` also carries:

```json
{
  "androidWorkMode": 1,
  "androidAutoSizeW": 800,
  "androidAutoSizeH": 480
}
```

The firmware may reset Android work mode to zero after a session ends, so NavBridge re-applies the work mode + BoxSettings + auto-connect command after `Unplugged` / `Phase 0` and on a manual retry.

## Session events

```text
0x02 Plugged       phone attached
0x03 Phase 7       negotiating
0x03 Phase 8       streaming-ready hint
0x04 Unplugged     phone detached
0x08 Control = 3   RequestHostUI / Exit -> show NavBridge Menu, keep session alive
0x19 JSON          phone info / MDLinkType / btName
0x23..0x26         Bluetooth state notifications
0x2A               dashboard/media metadata
```

Because firmware revisions differ in event ordering, NavBridge also uses strong live-link metadata as an auto-navigation hint. Projection itself is considered definitive only when video packets arrive.

## VideoData 0x06

The `0x06` payload begins with a 20-byte video-specific subheader. The bytes after offset 20 are the H.264 projection stream.

USB message/chunk boundaries are not assumed to be H.264 packet boundaries. The bytes are fed into FFmpeg `av_parser_parse2`, then into the normal software H.264 decoder. Decoded frames are converted/downscaled in software to RGBA 400x240 and published to the Slint process through `/dev/shm`.

No hardware decoder selection is present.

## MediaData 0x2A

`0x2A` is dashboard/media/navigation metadata, not the main projection framebuffer. Seeing repeated `0x2A` packets is evidence that the adapter is alive and can be evidence of a phone link, but the Android Auto / CarPlay picture itself comes from `0x06`.

## References

Protocol behavior was cross-checked against:

- FastCarPlay: `niellun/FastCarPlay`
- CPC200-CCPA protocol research: `lvalen91/CPC200-CCPA_resources`
