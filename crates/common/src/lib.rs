pub mod ipc_output;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Physical/logical dimensions of the NavBridge shell UI.
pub const UI_WIDTH: u32 = 400;
pub const UI_HEIGHT: u32 = 240;

/// Dimensions advertised to Android Auto and used by the media pipeline.
pub const MEDIA_WIDTH: u32 = 800;
pub const MEDIA_HEIGHT: u32 = 480;
pub const MEDIA_DPI: u32 = 160;

/// The projection frame, which the core decodes and the UI shows: a file
/// beside the IPC socket, in a tmpfs directory open only to NavBridge:
/// /run/navbridge for the services, a private one for the run scripts.
pub fn video_frame_path(socket_path: impl AsRef<Path>) -> PathBuf {
    socket_path.as_ref().with_file_name("video.rgba")
}
pub const VIDEO_FRAME_MAGIC: [u8; 4] = *b"NBRG";
pub const VIDEO_FRAME_HEADER_SIZE: usize = 20;

pub const DEFAULT_SOCKET_PATH: &str = "/run/navbridge/core.sock";

/// How much louder than the speakers' volume calls play, in dB: phones send a
/// call's audio well below music's loudness.
pub const CALL_VOLUME_DB: std::ops::RangeInclusive<i8> = -12..=18;

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
    /// The USB devices plugged in, by name, in the order of their ports;
    /// hubs are left out.
    #[serde(default)]
    pub usb_devices: Vec<String>,
}

/// The USB codec that ALSA plays to and records from.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSnapshot {
    pub codec: String,
    /// The codec's output and input as its hardware runs them; absent while
    /// nothing plays or records.
    #[serde(default)]
    pub output: Option<StreamFormat>,
    #[serde(default)]
    pub input: Option<StreamFormat>,
    /// Output volume and microphone gain from the ALSA mixer.
    pub volume: String,
    /// The same levels in percent, for Settings; absent without the codec.
    #[serde(default)]
    pub volume_percent: Option<u8>,
    #[serde(default)]
    pub microphone_percent: Option<u8>,
    /// The call volume, in dB over the speakers' volume; absent while calls
    /// do not play in the car.
    #[serde(default)]
    pub call_volume_db: Option<i8>,
}

/// A running stream's channels and sampling rate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamFormat {
    pub channels: u32,
    pub rate: u32,
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
    /// The session runs over the hotspot: wireless Android Auto.
    #[serde(default)]
    pub wireless: bool,
    /// The DPI the phone is asked for when it connects, as chosen in Settings.
    #[serde(default)]
    pub dpi: u32,
    /// The codec the phone is asked for when it connects, as chosen in Settings.
    #[serde(default)]
    pub codec: VideoCodec,
    /// The video decoder in use, such as "FFmpeg 7.1.5 h264 (software)".
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
            wireless: false,
            dpi: 0,
            codec: VideoCodec::default(),
            decoder: String::new(),
        }
    }
}

/// The video codec Android Auto is asked for: H.264, which every phone
/// encodes, or H.265, which some do. A phone without H.265 stays on H.264.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
}

impl VideoCodec {
    /// As Settings shows it: "H.264" or "H.265".
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
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
    /// The paired phones, by name.
    #[serde(default)]
    pub phones: Vec<BluetoothPhone>,
    /// The one phone called for wireless Android Auto, by its address, as
    /// chosen in Settings; without one, every paired phone is called.
    #[serde(default)]
    pub default_phone: Option<String>,
    /// Settings' Disconnect phone ended the wireless session: no phone is
    /// called, and that one is turned away, until Connect phone.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BluetoothPhone {
    /// As "64:9D:38:E1:B6:24".
    pub address: String,
    pub name: String,
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
    /// The range left, in km, from the cluster's on-board computer.
    #[serde(default)]
    pub range_km: Option<u16>,
    /// Average consumption ("Consumption 1"), in tenths of a litre per
    /// 100 km, from the cluster's on-board computer.
    #[serde(default)]
    pub consumption_dl: Option<u16>,
    /// The handbrake is pulled, as the cluster last reported it.
    #[serde(default)]
    pub handbrake: Option<bool>,
    /// The headlights' low beam is on, as the light module last reported it.
    #[serde(default)]
    pub low_beam: Option<bool>,
    /// The gearbox, manual or automatic, is in reverse, as the cluster last
    /// reported it.
    #[serde(default)]
    pub reverse: Option<bool>,
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
    /// The UI's colours, as chosen in Settings.
    #[serde(default)]
    pub theme: Theme,
}

/// The UI's colours: amber, blue or green on black.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Amber,
    Blue,
    Green,
}

/// A finger or the mouse on the projection: x and y are in Android Auto's
/// 800x480 pixels, whatever size the UI shows it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchKind {
    Down,
    Move,
    Up,
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
    Hello {
        client: String,
    },
    SetFrameSize {
        width: u32,
        height: u32,
    },
    RequestSnapshot,
    SetNightMode {
        enabled: bool,
    },
    SetParkingBrake {
        engaged: bool,
    },
    HideProjection,
    ResumeProjection,
    NavigationInput {
        key: NavigationKey,
        pressed: bool,
    },
    Touch {
        kind: TouchKind,
        x: u32,
        y: u32,
    },
    PhoneAction {
        action: PhoneAction,
    },
    SetWireless {
        enabled: bool,
    },
    SetDpi {
        dpi: u32,
    },
    SetVideoCodec {
        codec: VideoCodec,
    },
    SetVolume {
        percent: u8,
    },
    SetMicrophone {
        percent: u8,
    },
    SetCallVolume {
        db: i8,
    },
    SetTheme {
        theme: Theme,
    },
    StartPairing,
    StopPairing,
    ConfirmPairing {
        accept: bool,
    },
    /// The phone called for wireless Android Auto, by its address; none for
    /// every paired phone.
    SetDefaultPhone {
        address: Option<String>,
    },
    /// Ends the wireless session, and calls no phone until ConnectPhone.
    DisconnectPhone,
    ConnectPhone,
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
    Snapshot {
        snapshot: AppSnapshot,
    },
    ProjectionEvent {
        event: ProjectionEvent,
    },
    /// A car button, from the I-Bus, for the UI to act on as on a key: its
    /// press, then its release.
    Input {
        action: InputAction,
        pressed: bool,
    },
}

/// What a car button does, named as keyboard.toml names the UI's actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAction {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    Home,
    NormalMode,
    NightMode,
    ParkingBrakeOn,
    ParkingBrakeOff,
    NextTrack,
    PreviousTrack,
    VoiceAssistant,
    AnswerCall,
    EndCall,
    PhoneHome,
    OpenMaps,
    OpenMedia,
    PlayPause,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_video_frame_sits_beside_the_socket() {
        assert_eq!(
            video_frame_path(DEFAULT_SOCKET_PATH),
            Path::new("/run/navbridge/video.rgba")
        );
        assert_eq!(
            video_frame_path("/run/user/1000/navbridge-run.x/core.sock"),
            Path::new("/run/user/1000/navbridge-run.x/video.rgba")
        );
    }

    #[test]
    fn display_and_media_geometry_are_intentionally_different() {
        assert_eq!((UI_WIDTH, UI_HEIGHT), (400, 240));
        assert_eq!((MEDIA_WIDTH, MEDIA_HEIGHT, MEDIA_DPI), (800, 480, 160));
        assert_eq!(MEDIA_WIDTH, UI_WIDTH * 2);
        assert_eq!(MEDIA_HEIGHT, UI_HEIGHT * 2);
    }

    #[test]
    fn codecs_read_as_core_toml_names_them() {
        let codec = |name: &str| serde_json::from_str::<VideoCodec>(&format!("\"{name}\""));
        assert_eq!(codec("h264").unwrap(), VideoCodec::H264);
        assert_eq!(codec("h265").unwrap(), VideoCodec::H265);
        assert!(codec("hevc").is_err());
    }
}
