use navbridge_common::{DEFAULT_SOCKET_PATH, MEDIA_DPI};
use serde::Deserialize;
use std::{fs, net::SocketAddr, path::Path};
// Unknown keys are rejected so that a typo stops the core instead of being ignored.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    pub socket_path: String,
    pub snapshot_interval_ms: u64,
    pub android_auto: AndroidAutoConfig,
    pub audio: AudioConfig,
    pub ibus: IbusConfig,
}
impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.into(),
            snapshot_interval_ms: 500,
            android_auto: Default::default(),
            audio: Default::default(),
            ibus: Default::default(),
        }
    }
}
impl CoreConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let s: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if s.android_auto.dpi == 0
            || s.android_auto.dpi > 1000
            || !matches!(s.android_auto.fps, 30 | 60)
        {
            return Err("android_auto.dpi must be 1..1000; fps must be 30 or 60".into());
        }
        if s.android_auto.certificate.is_some() != s.android_auto.private_key.is_some() {
            return Err("certificate and private_key must be supplied together".into());
        }
        Ok(s)
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AndroidAutoConfig {
    pub dpi: u32,
    pub fps: u32,
    pub decoder: DecoderChoice,
    pub left_hand_drive: bool,
    pub certificate: Option<String>,
    pub private_key: Option<String>,
    /// Development: the phone's head unit server (Android Auto developer
    /// settings) as IP:port, used while no phone is plugged in by USB.
    pub head_unit_server: Option<SocketAddr>,
}
impl Default for AndroidAutoConfig {
    fn default() -> Self {
        Self {
            dpi: MEDIA_DPI,
            fps: 30,
            decoder: DecoderChoice::Auto,
            left_hand_drive: true,
            certificate: None,
            private_key: None,
            head_unit_server: None,
        }
    }
}
/// Which H.264 decoder shows the projection.
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
}
impl Default for IbusConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "/dev/serial/by-id/usb-Arduino_LLC_Arduino_Micro_HIDPC-if00".into(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shipped_config_parses_and_unknown_keys_are_rejected() {
        let c: CoreConfig = toml::from_str(include_str!("../../../config/core.toml")).unwrap();
        assert_eq!(c.android_auto.fps, 60);
        assert_eq!(c.android_auto.decoder, DecoderChoice::Auto);
        assert!(c.ibus.enabled);
        assert_eq!(c.ibus.device, IbusConfig::default().device);
        let c: CoreConfig = toml::from_str("[android_auto]\ndecoder = \"software\"").unwrap();
        assert_eq!(c.android_auto.decoder, DecoderChoice::Software);
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
            "snapshot_ms = 500",
            // An address, not a host name: resolving one could stall startup.
            "[android_auto]\nhead_unit_server = \"phone:5277\"",
        ] {
            assert!(toml::from_str::<CoreConfig>(typo).is_err(), "{typo}");
        }
    }
}
