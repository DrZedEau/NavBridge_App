# Third-party notices

## Android Auto protocol definitions

The minimal `crates/core/src/services/android_auto/aa.proto` uses field layouts
from Open Android Auto (mrmees/open-android-auto), GPL-3.0-or-later, copyright
2024-2026 Open Android Auto contributors; based on f1x.studio/aasdk,
opencardev/crankshaft and aa-proxy/aa-proxy-rs. Definitions were checked against
the protocol research and implementation on 13 October 2026.

- https://github.com/mrmees/open-android-auto
- https://github.com/mrmees/openauto-prodigy (reference revision
  3afc5dd8bbacb5ce68e109189b18b14b251fabef)
- https://github.com/uglyoldbob/android-auto

Legacy `config_index` in AV setup field 1 is interpreted as the media codec type
following the current Open Android Auto verified schema. Supported codecs are
H.264 (3) and PCM (1). Only the implemented wired channels are advertised.

## Wireless Android Auto setup

The Bluetooth service UUID, message ids, framing and the
`WifiStartRequest`/`WifiInfoResponse` field layouts in `aa.proto` and
`crates/core/src/services/android_auto/wireless.rs` follow WirelessAndroidAutoDongle
by Nisarg Jhaveri (MIT License, copyright 2023), checked on 16 October 2026. No
code was copied; the protocol facts were reimplemented. Unlike that project,
NavBridge does not use RFCOMM channel 8, which BlueZ's SIM access service holds
on Raspberry Pi OS. The `WifiConnectStatus` layout and meaning (0 when the
phone joined the hotspot) follow aa-proxy/aa-proxy-rs, checked on 16 October
2026; no code was copied.

https://github.com/nisargjhaveri/WirelessAndroidAutoDongle
https://github.com/aa-proxy/aa-proxy-rs

## Menu icons

The menu's icons in `crates/ui/ui/icons` are the Android Auto logo (Google LLC),
the CarPlay logo (Apple Inc.), a settings gear and the Raspberry Pi logo
(Raspberry Pi Ltd), supplied by the project owner and scaled down. These logos
are trademarks of their owners, used only to name the matching features; they
are not covered by NavBridge's licence. Check their owners' brand guidelines
before distributing builds that include them.

## Accessory TLS identity

`crates/core/src/services/android_auto/identity.rs` contains the public Google
Automotive Link trust certificate and example accessory certificate/key from
uglyoldbob/android-auto `src/cert.rs`, licensed LGPL-3.0-or-later. The source
notice is retained in that file. The example accessory key is publicly distributed
protocol compatibility material, not a per-device private credential.

Source: https://github.com/uglyoldbob/android-auto/blob/06c68d5ea8ac4f0ef0ab3b97dc60b9665f7074cb/src/cert.rs
License: LICENSE-LGPL-3.0 (https://www.gnu.org/licenses/lgpl-3.0.html)

NavBridge's distributed source includes this reused material and modifications.
The containing application is GPL-3.0-or-later. Preserve this notice and provide
the corresponding source when distributing binaries.

## Frame-rate mapping validation

The current Open Android Auto VideoFPS mapping is 60=1 and 30=2. Original AASDK
definitions use the reverse mapping. On 13 October 2026, the same Pixel/CM5
pipeline produced up to roughly 30 frontend updates/s with value 2 and
52-56 frontend updates/s with value 1, confirming the current mapping for the
tested phone. Values are covered by a serialized service-discovery regression
test. Other phone models have not yet been tested.

https://github.com/mrmees/open-android-auto/blob/main/oaa/video/VideoFPSEnum.proto
https://github.com/f1xpl/aasdk/blob/master/aasdk_proto/VideoFPSEnum.proto
