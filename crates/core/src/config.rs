use std::fs;
use std::path::Path;

use serde::Deserialize;

use navbridge_common::{DEFAULT_SOCKET_PATH, MEDIA_DPI};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub socket_path: String,
    pub snapshot_interval_ms: u64,
    pub usb: UsbConfig,
    pub carplay: CarplayConfig,
    pub audio: AudioConfig,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.into(),
            snapshot_interval_ms: 500,
            usb: UsbConfig::default(),
            carplay: CarplayConfig::default(),
            audio: AudioConfig::default(),
        }
    }
}

impl CoreConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&raw)?;
        config.carplay.validate_dpi().map_err(|message|
            std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        config.carplay.validate_names().map_err(|message|
            std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        Ok(config)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UsbConfig {
    pub scan_interval_ms: u64,
    pub read_size: usize,
    /// Bulk-IN timeout. Keep this short so the event loop can service the
    /// 2-second heartbeat without a dedicated USB thread.
    pub io_timeout_ms: u64,
    /// Bulk-OUT timeout. Adapter writes can legitimately stall for much
    /// longer than reads while firmware services /dev/usb_accessory.
    pub write_timeout_ms: u64,
    /// Mandatory settle time after libusb reset before rediscovering the
    /// re-enumerated adapter.
    pub reset_delay_ms: u64,
    /// Inter-command pacing during adapter initialization.
    pub init_command_delay_ms: u64,
}

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            scan_interval_ms: 750,
            read_size: 32768,
            io_timeout_ms: 100,
            write_timeout_ms: 3000,
            reset_delay_ms: 3000,
            init_command_delay_ms: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CarplayConfig {
    /// Keepalive interval. Carlinkit adapters expect a heartbeat approximately
    /// every two seconds. Keep this comfortably below the firmware timeout.
    pub heartbeat_ms: u64,
    pub fps: u32,
    /// Logical density advertised to AA/CarPlay; independent of video resolution.
    pub dpi: u32,
    pub media_delay_ms: u32,
    pub autoconnect: bool,
    pub left_hand_drive: bool,
    pub wifi_5ghz: bool,
    /// Empty leaves the adapter name unchanged; otherwise 1-15 safe ASCII bytes.
    pub bluetooth_name: String,
    pub wifi_name: String,
}

impl Default for CarplayConfig {
    fn default() -> Self {
        Self {
            heartbeat_ms: 2000,
            fps: 60,
            dpi: MEDIA_DPI,
            media_delay_ms: 300,
            autoconnect: true,
            left_hand_drive: true,
            wifi_5ghz: true,
            bluetooth_name: "NavBridge".into(),
            wifi_name: "NavBridge".into(),
        }
    }
}

impl CarplayConfig {
    fn validate_dpi(&self) -> Result<(), String> {
        if self.dpi == 0 || self.dpi > i32::MAX as u32 {
            return Err("carplay.dpi must be a positive integer no greater than 2147483647".into());
        }
        Ok(())
    }

    fn validate_names(&self) -> Result<(), String> {
        for (key, value) in [("bluetooth_name", &self.bluetooth_name), ("wifi_name", &self.wifi_name)] {
            // Adapter firmware handles names through shell-backed configuration.
            // Restrict input to portable display names, not shell metacharacters.
            if value.len() > 15 || (!value.is_empty() && value.trim().is_empty())
                || !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'-')) {
                return Err(format!("carplay.{key}: use at most 15 ASCII letters, digits, spaces, '-' or '_'; empty keeps the current dongle name"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;
    #[test]
    fn dpi_defaults_and_validates_protocol_integer() {
        let default: CarplayConfig = toml::from_str("").unwrap();
        assert_eq!(default.dpi, 160);
        let custom: CarplayConfig = toml::from_str("dpi = 240").unwrap();
        assert_eq!(custom.dpi, 240);
        assert!(custom.validate_dpi().is_ok());
        for dpi in [0, u32::MAX] {
            assert!(CarplayConfig { dpi, ..Default::default() }.validate_dpi().is_err());
        }
    }

    #[test]
    fn validates_adapter_names_before_usb_startup() {
        for name in ["NavBridge", "My Car-5_USB", "123456789012345", ""] {
            let config = CarplayConfig { bluetooth_name: name.into(), wifi_name: name.into(), ..Default::default() };
            assert!(config.validate_names().is_ok());
        }
        for name in ["1234567890123456", "   ", "Car;reboot", "$(id)", "Car\nName", "Car\0Name", "Café"] {
            let mut config = CarplayConfig::default();
            config.bluetooth_name = name.into();
            assert!(config.validate_names().is_err());
            config.bluetooth_name = "NavBridge".into();
            config.wifi_name = name.into();
            assert!(config.validate_names().is_err());
        }
    }
}

/// ALSA names use the USB card's stable ID, independent of HDMI card ordering.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
