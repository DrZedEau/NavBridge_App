use crate::services::ibus::Button;
use navbridge_common::{InputAction, VideoCodec, DEFAULT_SOCKET_PATH, MEDIA_DPI};
use serde::{de::IgnoredAny, Deserialize};
use std::{collections::BTreeMap, fs, net::SocketAddr, path::Path};
// Unknown keys are rejected so that a typo stops the core instead of being ignored.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    pub socket_path: String,
    pub snapshot_interval_ms: u64,
    pub android_auto: AndroidAutoConfig,
    pub audio: AudioConfig,
    pub ibus: IbusConfig,
    pub wireless: WirelessConfig,
}
impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.into(),
            snapshot_interval_ms: 500,
            android_auto: Default::default(),
            audio: Default::default(),
            ibus: Default::default(),
            wireless: Default::default(),
        }
    }
}
impl CoreConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let s: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if s.android_auto.dpi == 0 || s.android_auto.dpi > 1000 {
            return Err("android_auto.dpi must be 1..1000".into());
        }
        if s.android_auto.certificate.is_some() != s.android_auto.private_key.is_some() {
            return Err("certificate and private_key must be supplied together".into());
        }
        s.wireless.check()?;
        Ok(s)
    }
}
/// The Wi-Fi that becomes wireless Android Auto's hotspot.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WirelessConfig {
    /// The Wi-Fi interface, as `ip link` names it.
    pub interface: String,
    /// The hotspot's channel, in the 5 GHz band.
    pub channel: u32,
}
impl Default for WirelessConfig {
    fn default() -> Self {
        Self {
            interface: "wlan0".into(),
            // Usable in the EU without radar detection.
            channel: 36,
        }
    }
}
impl WirelessConfig {
    /// The 5 GHz band's 20 MHz channels.
    const CHANNELS: [u32; 25] = [
        36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
        149, 153, 157, 161, 165,
    ];

    /// An interface name as Linux allows one, and a 5 GHz channel.
    pub fn check(&self) -> Result<(), String> {
        let name = &self.interface;
        let named = (1..=15).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && name != "."
            && name != "..";
        if !named {
            return Err(format!(
                "wireless.interface is not an interface name: {name:?}"
            ));
        }
        if !Self::CHANNELS.contains(&self.channel) {
            return Err(format!(
                "wireless.channel must be a 5 GHz channel (36, 40, 44, 48, ... 165), not {}",
                self.channel
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AndroidAutoConfig {
    pub dpi: u32,
    /// No longer read: the phone is always asked for 60 frames a second. A
    /// core.toml from before still loads.
    #[serde(rename = "fps")]
    _fps: IgnoredAny,
    /// The codec the phone is asked for.
    pub codec: VideoCodec,
    pub decoder: DecoderChoice,
    pub left_hand_drive: bool,
    pub certificate: Option<String>,
    pub private_key: Option<String>,
    /// Development: the phone's head unit server (Android Auto developer
    /// settings) as IP:port, used while no phone is plugged in by USB.
    pub head_unit_server: Option<SocketAddr>,
    /// Not from core.toml: the columns the phone leaves black for the UI's
    /// shape, set for each session from the size the UI asks for.
    #[serde(skip)]
    pub margin_width: u32,
}
impl Default for AndroidAutoConfig {
    fn default() -> Self {
        Self {
            dpi: MEDIA_DPI,
            _fps: IgnoredAny,
            codec: VideoCodec::H264,
            decoder: DecoderChoice::Auto,
            left_hand_drive: true,
            certificate: None,
            private_key: None,
            head_unit_server: None,
            margin_width: 0,
        }
    }
}
/// Which decoder shows the projection.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DecoderChoice {
    /// The board's hardware decoder (Pi 4 / CM4), else FFmpeg's software one.
    #[default]
    Auto,
    Software,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    pub playback_device: String,
    pub capture_device: String,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            playback_device: "plug:SLAVE=\"dmix:CARD=CODEC,DEV=0\"".into(),
            capture_device: "plughw:CARD=CODEC,DEV=0".into(),
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IbusConfig {
    pub enabled: bool,
    /// The carrier board's microcontroller, which passes on each I-Bus frame
    /// as a line on its USB serial port.
    pub device: String,
    /// What each of the car's buttons does; a button left out does nothing.
    pub buttons: BTreeMap<Button, InputAction>,
    /// The Pi shuts down when the ignition goes off, or once it has been off
    /// for `shutdown_delay_s` seconds, unless it comes back first.
    pub shutdown: bool,
    pub shutdown_delay_s: u64,
}
impl Default for IbusConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "/dev/serial/by-id/usb-Arduino_LLC_Arduino_Micro_HIDPC-if00".into(),
            buttons: default_buttons(),
            shutdown: true,
            shutdown_delay_s: 0,
        }
    }
}
/// The keys the board's firmware typed for each button.
fn default_buttons() -> BTreeMap<Button, InputAction> {
    BTreeMap::from([
        (Button::Preset1, InputAction::PhoneHome),
        (Button::Preset2, InputAction::Back),
        (Button::Preset3, InputAction::PlayPause),
        (Button::Preset4, InputAction::OpenMedia),
        (Button::Preset5, InputAction::Up),
        (Button::Preset6, InputAction::Down),
        (Button::Next, InputAction::NextTrack),
        (Button::Previous, InputAction::PreviousTrack),
        (Button::KnobPress, InputAction::Select),
        (Button::KnobLeft, InputAction::Left),
        (Button::KnobRight, InputAction::Right),
        (Button::WheelNext, InputAction::NextTrack),
        (Button::WheelPrevious, InputAction::PreviousTrack),
        (Button::WheelSendEnd, InputAction::VoiceAssistant),
        (Button::WheelRt, InputAction::AnswerCall),
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shipped_config_parses_and_unknown_keys_are_rejected() {
        let c: CoreConfig = toml::from_str(include_str!("../../../config/core.toml")).unwrap();
        assert_eq!(c.android_auto.dpi, 160);
        assert_eq!(c.android_auto.codec, VideoCodec::H264);
        assert_eq!(c.android_auto.decoder, DecoderChoice::Auto);
        assert!(c.ibus.enabled);
        assert_eq!(c.ibus.device, IbusConfig::default().device);
        // The shipped table is the keys the board's firmware types.
        assert_eq!(c.ibus.buttons, default_buttons());
        assert!(c.ibus.shutdown);
        assert_eq!(c.ibus.shutdown_delay_s, 0);
        // The shipped Wi-Fi is the Pi's own, on the channel used so far.
        assert_eq!(c.wireless, WirelessConfig::default());
        assert_eq!(
            (c.wireless.interface.as_str(), c.wireless.channel),
            ("wlan0", 36)
        );
        assert_eq!(c.wireless.check(), Ok(()));
        let c: CoreConfig = toml::from_str("[ibus.buttons]\npreset_1 = \"home\"").unwrap();
        assert_eq!(
            c.ibus.buttons,
            BTreeMap::from([(Button::Preset1, InputAction::Home)])
        );
        // The phone's further keys can go on any button.
        for (name, action) in [
            ("phone_menu", InputAction::PhoneMenu),
            ("phone_music", InputAction::PhoneMusic),
            ("phone_contacts", InputAction::PhoneContacts),
            ("phone_calendar", InputAction::PhoneCalendar),
            ("phone_settings", InputAction::PhoneSettings),
            ("phone_notification", InputAction::PhoneNotification),
            ("phone_app_switch", InputAction::PhoneAppSwitch),
            ("phone_radio", InputAction::PhoneRadio),
            ("phone_tel", InputAction::PhoneTel),
        ] {
            let c: CoreConfig =
                toml::from_str(&format!("[ibus.buttons]\ninfo = \"{name}\"")).unwrap();
            assert_eq!(
                c.ibus.buttons,
                BTreeMap::from([(Button::Info, action)]),
                "{name}"
            );
        }
        let c: CoreConfig = toml::from_str("[android_auto]\ndecoder = \"software\"").unwrap();
        assert_eq!(c.android_auto.decoder, DecoderChoice::Software);
        let c: CoreConfig = toml::from_str("[android_auto]\ncodec = \"h265\"").unwrap();
        assert_eq!(c.android_auto.codec, VideoCodec::H265);
        assert!(toml::from_str::<CoreConfig>("[android_auto]\ncodec = \"vp9\"").is_err());
        // The frame rate, from before it was always 60, is still accepted.
        assert!(toml::from_str::<CoreConfig>("[android_auto]\nfps = 30").is_ok());
        assert!(toml::from_str::<CoreConfig>("[android_auto]\ndecoder = \"gpu\"").is_err());
        let c: CoreConfig =
            toml::from_str("[android_auto]\nhead_unit_server = \"127.0.0.1:5277\"").unwrap();
        assert_eq!(
            c.android_auto.head_unit_server,
            Some(SocketAddr::from(([127, 0, 0, 1], 5277)))
        );
        for typo in [
            "[carplay]\ndpi = 240",
            "[android_auto]\nfsp = 60",
            "[ibus]\nport = \"/dev/ttyACM0\"",
            "[ibus.buttons]\npreset_7 = \"home\"",
            "[ibus.buttons]\npreset_1 = \"jump\"",
            "snapshot_ms = 500",
            // An address, not a host name: resolving one could stall startup.
            "[android_auto]\nhead_unit_server = \"phone:5277\"",
            "[wireless]\nchanel = 44",
        ] {
            assert!(toml::from_str::<CoreConfig>(typo).is_err(), "{typo}");
        }
    }
    #[test]
    fn the_hotspot_takes_another_interface_or_5_ghz_channel() {
        let c: CoreConfig =
            toml::from_str("[wireless]\ninterface = \"wlx00c0caa5b7d1\"\nchannel = 149").unwrap();
        assert_eq!(
            (c.wireless.interface.as_str(), c.wireless.channel),
            ("wlx00c0caa5b7d1", 149)
        );
        assert_eq!(c.wireless.check(), Ok(()));
        // One of them alone keeps the other.
        let c: CoreConfig = toml::from_str("[wireless]\nchannel = 44").unwrap();
        assert_eq!(c.wireless.interface, "wlan0");
        let wireless = |interface: &str, channel| WirelessConfig {
            interface: interface.into(),
            channel,
        };
        // A 2.4 GHz channel, and one the 5 GHz band does not have.
        assert!(wireless("wlan0", 6).check().is_err());
        assert!(wireless("wlan0", 37).check().is_err());
        // Not names of an interface: the name goes into a path under /sys.
        for name in [
            "",
            "..",
            "wlan0/../eth0",
            "wlan 0",
            "an-interface-name-too-long",
        ] {
            assert!(wireless(name, 36).check().is_err(), "{name:?}");
        }
    }
}
