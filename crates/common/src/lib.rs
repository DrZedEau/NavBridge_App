pub mod ipc_output;

use serde::{Deserialize, Serialize};

/// Physical/logical dimensions of the NavBridge shell UI.
pub const UI_WIDTH: u32 = 400;
pub const UI_HEIGHT: u32 = 240;

/// Dimensions advertised to Android Auto and used by the media pipeline.
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsbSnapshot {
    pub selected: Option<UsbDeviceInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemSnapshot {
    pub cpu_usage: String,
    pub memory: String,
    pub cpu_temp: String,
    #[serde(default)]
    pub audio: AudioSnapshot,
}

/// The USB codec that ALSA plays to and records from.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSnapshot {
    pub codec: String,
    /// Hardware rates while playback or the microphone runs.
    pub sample_rate: String,
    /// Output volume and microphone gain from the ALSA mixer.
    pub volume: String,
    /// The same levels in percent, for Settings; absent without the codec.
    #[serde(default)]
    pub volume_percent: Option<u8>,
    #[serde(default)]
    pub microphone_percent: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionPhase {
    WaitingForPhone,
    Starting,
    Running,
    Error,
}

impl Default for ProjectionPhase {
    fn default() -> Self {
        Self::WaitingForPhone
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionSnapshot {
    pub phase: ProjectionPhase,
    pub status: String,
    pub phone_connected: bool,
    pub streaming: bool,
    #[serde(default)]
    pub phone_name: String,
    /// The DPI the phone is asked for when it connects, as chosen in Settings.
    #[serde(default)]
    pub dpi: u32,
    /// The frame rate the phone is asked for when it connects: 30 or 60.
    #[serde(default)]
    pub fps: u32,
    /// The H.264 decoder in use, such as "FFmpeg 7.1.5 h264 (software)".
    #[serde(default)]
    pub decoder: String,
}

impl Default for ProjectionSnapshot {
    fn default() -> Self {
        Self {
            phase: ProjectionPhase::WaitingForPhone,
            status: "Connect an Android phone by USB".into(),
            phone_connected: false,
            streaming: false,
            phone_name: String::new(),
            dpi: 0,
            fps: 0,
            decoder: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionEvent {
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

/// Wireless Android Auto: whether it is on, and what the built-in Wi-Fi does.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct WirelessSnapshot {
    /// On: the built-in Wi-Fi is the hotspot; off: it is on the home network.
    pub enabled: bool,
    /// The hotspot is up, so phones can join it.
    #[serde(default)]
    pub hotspot: bool,
    pub status: String,
}

/// Bluetooth for wireless Android Auto: pairing, and reaching paired phones.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BluetoothSnapshot {
    pub status: String,
    /// NavBridge's services are registered: paired phones can reach it.
    #[serde(default)]
    pub ready: bool,
    /// Phones can find NavBridge and pair with it.
    pub pairing: bool,
    /// While pairing is open, the seconds left.
    #[serde(default)]
    pub pairing_seconds: u32,
    /// NavBridge's Bluetooth name, as phones show it.
    #[serde(default)]
    pub name: String,
    /// A phone asks to pair: the user compares the code and confirms.
    pub confirm: Option<PairingRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRequest {
    pub device: String,
    /// Six digits, as the phone shows them.
    pub code: String,
}

/// The car, as its I-Bus reports it through the carrier board's
/// microcontroller. A reading is absent until it is heard, and again once it
/// is too old to trust.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VehicleSnapshot {
    /// What the I-Bus link does, or why it does not.
    pub status: String,
    /// The interface is open, whether or not the car is talking.
    #[serde(default)]
    pub interface: bool,
    /// Frames heard since the core started, and lines that were not valid
    /// frames, which point to a wiring or interference problem.
    pub frames: u64,
    pub bad_lines: u64,
    pub ignition: Option<Ignition>,
    /// The instrument cluster's road speed, in steps of 2 km/h.
    pub speed_kmh: Option<u16>,
    /// Engine speed, in steps of 100 rpm.
    pub rpm: Option<u16>,
    pub coolant_c: Option<i8>,
    pub outside_c: Option<i8>,
    pub odometer_km: Option<u32>,
}

/// The ignition key's position, as the instrument cluster reports it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Ignition {
    Off,
    /// Position 1: radio and accessories.
    Accessory,
    /// Position 2: ignition on.
    On,
    Starting,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSnapshot {
    #[serde(default)]
    pub gnss: GnssSnapshot,
    pub usb: UsbSnapshot,
    pub system: SystemSnapshot,
    pub projection: ProjectionSnapshot,
    #[serde(default)]
    pub wireless: WirelessSnapshot,
    #[serde(default)]
    pub bluetooth: BluetoothSnapshot,
    #[serde(default)]
    pub vehicle: VehicleSnapshot,
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
    /// Hangs up, or rejects an incoming call.
    EndCall,
    PhoneHome,
    /// The phone's navigation app, such as Google Maps or Waze.
    OpenMaps,
    /// The phone's media screen.
    OpenMedia,
    /// Pauses or resumes, as the phone knows whether music is playing.
    PlayPause,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    Hello { client: String },
    RequestSnapshot,
    SetNightMode { enabled: bool },
    SetParkingBrake { engaged: bool },
    HideProjection,
    ResumeProjection,
    NavigationInput { key: NavigationKey, pressed: bool },
    PhoneAction { action: PhoneAction },
    SetWireless { enabled: bool },
    SetDpi { dpi: u32 },
    SetFps { fps: u32 },
    SetVolume { percent: u8 },
    SetMicrophone { percent: u8 },
    StartPairing,
    StopPairing,
    ConfirmPairing { accept: bool },
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
        /// Highest frame rate requested from the phone.
        #[serde(default)]
        media_fps: u32,
    },
    Snapshot {
        snapshot: AppSnapshot,
    },
    ProjectionEvent {
        event: ProjectionEvent,
    },
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
