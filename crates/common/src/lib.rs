pub mod ipc_output;

use serde::{Deserialize, Serialize};

/// Physical/logical dimensions of the NavBridge shell UI.
pub const UI_WIDTH: u32 = 400;
pub const UI_HEIGHT: u32 = 240;

/// Dimensions advertised to CarPlay / Android Auto and used by the media pipeline.
pub const MEDIA_WIDTH: u32 = 800;
pub const MEDIA_HEIGHT: u32 = 480;
pub const MEDIA_DPI: u32 = 160;

/// Cross-process software-decoder frame exchange. This is a tmpfs file on Linux.
pub const VIDEO_FRAME_PATH: &str = "/dev/shm/navbridge-video.rgba";
pub const VIDEO_FRAME_MAGIC: [u8; 4] = *b"NBRG";
pub const VIDEO_FRAME_HEADER_SIZE: usize = 20;

pub const DEFAULT_SOCKET_PATH: &str = "/run/navbridge/core.sock";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsbDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bus: u8,
    pub address: u8,
    pub speed: String,
    pub manufacturer: String,
    pub product: String,
    pub selection_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsbSnapshot {
    pub selected: Option<UsbDeviceInfo>,
    pub candidates: Vec<UsbDeviceInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemSnapshot {
    pub cpu_usage: String,
    #[serde(default)]
    pub audio_output_rate: String,
    pub memory: String,
    pub cpu_temp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CarplayPhase {
    WaitingForDongle,
    DongleReady,
    Starting,
    Running,
    Error,
}

impl Default for CarplayPhase {
    fn default() -> Self {
        Self::WaitingForDongle
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarplaySnapshot {
    pub phase: CarplayPhase,
    pub status: String,
    pub negotiated_width: u32,
    pub negotiated_height: u32,
    pub dpi: u32,
    pub phone_connected: bool,
    pub streaming: bool,
    pub protocol_phase: Option<u32>,
    #[serde(default)]
    pub dongle_firmware: String,
    #[serde(default)]
    pub dongle_bluetooth_name: String,
    pub phone_name: String,
    pub connection_method: String,
    #[serde(default)]
    pub connection_transport: String,
    pub heartbeat_count: u64,
    pub video_packets: u64,
    pub video_frames: u64,
    pub video_dropped: u64,
    pub source_video_width: u32,
    pub source_video_height: u32,
}

impl Default for CarplaySnapshot {
    fn default() -> Self {
        Self {
            phase: CarplayPhase::WaitingForDongle,
            status: "Waiting for dongle".into(),
            negotiated_width: MEDIA_WIDTH,
            negotiated_height: MEDIA_HEIGHT,
            dpi: MEDIA_DPI,
            phone_connected: false,
            streaming: false,
            protocol_phase: None,
            dongle_firmware: String::new(),
            dongle_bluetooth_name: String::new(),
            phone_name: String::new(),
            connection_method: String::new(),
            connection_transport: String::new(),
            heartbeat_count: 0,
            video_packets: 0,
            video_frames: 0,
            video_dropped: 0,
            source_video_width: 0,
            source_video_height: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CarplayEvent {
    PhoneConnected,
    PhoneDisconnected,
    StreamingReady,
    RequestHostUi,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GnssSnapshot {
    pub receiver: String,
    pub status: String,
    pub satellites: String,
    pub position: String,
    pub altitude: String,
    pub speed: String,
    pub hdop: String,
    pub utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSnapshot {
    #[serde(default)]
    pub gnss: GnssSnapshot,
    pub usb: UsbSnapshot,
    pub system: SystemSnapshot,
    pub carplay: CarplaySnapshot,
    pub ibus_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationKey {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneAction {
    NextTrack,
    PreviousTrack,
    VoiceAssistant,
    AnswerCall,
    PhoneHome,
    PauseMusic,
    ResumeMusic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    Hello { client: String },
    RequestSnapshot,
    ToggleNightMode,
    StartCarplay,
    ResumeCarplay,
    PairPhone,
    StopCarplay,
    NavigationInput { key: NavigationKey, pressed: bool },
    PhoneAction { action: PhoneAction },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Hello {
        version: String,
        ui_width: u32,
        ui_height: u32,
        media_width: u32,
        media_height: u32,
        media_dpi: u32,
    },
    Snapshot { snapshot: AppSnapshot },
    CarplayEvent { event: CarplayEvent },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_media_geometry_are_intentionally_different() {
        assert_eq!((UI_WIDTH, UI_HEIGHT), (400, 240));
        assert_eq!((MEDIA_WIDTH, MEDIA_HEIGHT, MEDIA_DPI), (800, 480, 160));
        assert_eq!(MEDIA_WIDTH, UI_WIDTH * 2);
        assert_eq!(MEDIA_HEIGHT, UI_HEIGHT * 2);
    }
}
