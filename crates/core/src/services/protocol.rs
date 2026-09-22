use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusb::{DeviceHandle, UsbContext};

use crate::config::CarplayConfig;
use navbridge_common::{PhoneAction, NavigationKey, MEDIA_HEIGHT, MEDIA_WIDTH};

pub const MAGIC: u32 = 0x55aa55aa;
pub const MAGIC_ENCRYPTED: u32 = 0x55bb55bb;
pub const HEADER_SIZE: usize = 16;
pub const MAX_PAYLOAD: usize = 2 * 1024 * 1024;

pub const CMD_OPEN: u32 = 1;
pub const CMD_PLUGGED: u32 = 2;
pub const CMD_PHASE: u32 = 3;
pub const CMD_UNPLUGGED: u32 = 4;
pub const CMD_VIDEO: u32 = 6;
pub const CMD_AUDIO: u32 = 7;
pub const CMD_CONTROL: u32 = 8;
pub const CMD_BLUETOOTH_NAME: u32 = 13;
pub const CMD_WIFI_NAME: u32 = 14;
pub const CMD_DISCONNECT_PHONE: u32 = 15;
pub const CMD_JSON_CONTROL: u32 = 25;
pub const CMD_SEND_FILE: u32 = 153;
pub const CMD_MEDIA_DATA: u32 = 42;
pub const CMD_HEARTBEAT: u32 = 170;
pub const CMD_SOFTWARE_VERSION: u32 = 204;
pub const CMD_BLUETOOTH_CONNECT_START: u32 = 35;
pub const CMD_BLUETOOTH_CONNECTED: u32 = 36;
pub const CMD_BLUETOOTH_DISCONNECTED: u32 = 37;
pub const CMD_BLUETOOTH_LISTEN: u32 = 38;

pub const CONTROL_REQUEST_HOST_UI: u32 = 3;
pub const CONTROL_AUTOCONNECT: u32 = 1002;
pub const CONTROL_REQUEST_KEYFRAME: u32 = 12;
pub const CONTROL_BUTTON_RELEASE: u32 = 105;

/// One-shot type-8 commands from node-carplay's CommandMapping.
pub fn phone_action_message(action: PhoneAction) -> ProtocolMessage {
    ProtocolMessage::control(match action {
        PhoneAction::NextTrack => 204,
        PhoneAction::PreviousTrack => 205,
        PhoneAction::VoiceAssistant => 5,
        PhoneAction::AnswerCall => 300,
        PhoneAction::PhoneHome => 200,
        PhoneAction::PauseMusic => 202,
        PhoneAction::ResumeMusic => 201,
    })
}

/// Carlinkit type-8 D-pad controls, followed by a shared release command.
/// Reference: CPC200-CCPA_resources/documentation/02_Protocol_Reference/command_ids.md
#[derive(Default)]
pub struct NavigationButtons {
    held: Option<NavigationKey>,
}

impl NavigationButtons {
    pub fn update(&mut self, key: NavigationKey, pressed: bool) -> Vec<ProtocolMessage> {
        let mut messages = Vec::new();
        if pressed {
            if self.held == Some(key) {
                return messages;
            }
            if self.held.is_some() {
                messages.push(ProtocolMessage::control(CONTROL_BUTTON_RELEASE));
            }
            let command = match key {
                NavigationKey::Left => 100,
                NavigationKey::Right => 101,
                NavigationKey::Up => 102,
                NavigationKey::Down => 103,
                NavigationKey::Select => 104,
                NavigationKey::Back => 106,
            };
            messages.push(ProtocolMessage::control(command));
            self.held = Some(key);
        } else if self.held == Some(key) {
            messages.push(ProtocolMessage::control(CONTROL_BUTTON_RELEASE));
            self.held = None;
        }
        messages
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolMessage {
    pub kind: u32,
    pub payload: Vec<u8>,
    pub encrypted: bool,
}

impl ProtocolMessage {
    pub fn new(kind: u32, payload: Vec<u8>) -> Self {
        Self {
            kind,
            payload,
            encrypted: false,
        }
    }

    pub fn empty(kind: u32) -> Self {
        Self::new(kind, Vec::new())
    }

    pub fn control(value: u32) -> Self {
        Self::new(CMD_CONTROL, value.to_le_bytes().to_vec())
    }

    pub fn heartbeat() -> Self {
        Self::empty(CMD_HEARTBEAT)
    }

    pub fn disconnect_phone() -> Self {
        Self::empty(CMD_DISCONNECT_PHONE)
    }

    fn header(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0_u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&(self.payload.len() as i32).to_le_bytes());
        out[8..12].copy_from_slice(&self.kind.to_le_bytes());
        out[12..16].copy_from_slice(&(!self.kind).to_le_bytes());
        out
    }
}

pub fn send_message<T: UsbContext>(
    handle: &DeviceHandle<T>,
    endpoint: u8,
    message: &ProtocolMessage,
    timeout: Duration,
) -> Result<(), rusb::Error> {
    write_all_bulk(handle, endpoint, &message.header(), timeout)?;
    if !message.payload.is_empty() {
        write_all_bulk(handle, endpoint, &message.payload, timeout)?;
    }
    Ok(())
}

fn write_all_bulk<T: UsbContext>(
    handle: &DeviceHandle<T>,
    endpoint: u8,
    mut bytes: &[u8],
    timeout: Duration,
) -> Result<(), rusb::Error> {
    while !bytes.is_empty() {
        let written = handle.write_bulk(endpoint, bytes, timeout)?;
        if written == 0 {
            return Err(rusb::Error::Io);
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// AutoKit sendNightMode: live command followed by the adapter's runtime file.
pub fn night_mode_messages(enabled: bool) -> Vec<ProtocolMessage> {
    vec![ProtocolMessage::control(if enabled { 16 } else { 17 }),
         file_i32("/tmp/night_mode", i32::from(enabled))]
}

pub fn initialization_messages(config: &CarplayConfig, night_mode: bool) -> Vec<ProtocolMessage> {
    let mut messages = Vec::new();

    // Keep Android Auto's daemon mode persisted before Open. Newer firmware
    // also requires androidWorkMode in BoxSettings; both paths are intentional.
    messages.push(file_i32("/tmp/screen_dpi", config.dpi as i32));
    messages.push(file_i32("/etc/android_work_mode", 1));
    // CarPlay retains its safe area independently of Open's resolution.
    // Describe the 800x480 video, not the downscaled 400x240 LCD.
    messages.push(file_bytes("/etc/RiddleBoxData/HU_VIEWAREA_INFO",
        &le_words(&[MEDIA_WIDTH, MEDIA_HEIGHT, MEDIA_WIDTH, MEDIA_HEIGHT, 0, 0])));
    messages.push(file_bytes("/etc/RiddleBoxData/HU_SAFEAREA_INFO",
        &le_words(&[MEDIA_WIDTH, MEDIA_HEIGHT, 0, 0, 0])));
    messages.push(open(config.fps));

    let drive_position = if config.left_hand_drive { 0 } else { 1 };


    // Keep this order close to known-working FastCarPlay/AutoKit startup:
    // geometry/work mode -> Open -> BoxSettings -> display/files -> controls.
    // Recent firmware additionally needs androidWorkMode inside BoxSettings.
    messages.push(box_settings_message(config));
    // Start with forwarding stopped; only fresh external fixes may enable it.
    messages.push(ProtocolMessage::control(19));
    messages.extend(night_mode_messages(night_mode));
    messages.push(file_bytes("/etc/box_name", b"NavBridge"));
    // AutoKit setBoxBluetoothName/setBoxWifiName: raw name bytes, no NUL.
    if !config.bluetooth_name.is_empty() {
        messages.push(ProtocolMessage::new(CMD_BLUETOOTH_NAME, config.bluetooth_name.as_bytes().to_vec()));
    }
    if !config.wifi_name.is_empty() {
        messages.push(ProtocolMessage::new(CMD_WIFI_NAME, config.wifi_name.as_bytes().to_vec()));
    }
    messages.push(file_i32("/tmp/hand_drive_mode", drive_position));
    // Host microphone and USB audio transport route through PCM2912A.
    messages.push(ProtocolMessage::control(7));
    messages.push(ProtocolMessage::control(if config.wifi_5ghz { 25 } else { 24 }));
    messages.push(ProtocolMessage::control(23));
    if config.autoconnect {
        messages.push(ProtocolMessage::control(CONTROL_AUTOCONNECT));
    }

    messages
}

/// Full adapter settings. `androidWorkMode: 1` is required by recent CCPA
/// firmware for fresh Android Auto pairing and is reset to 0 after disconnect.
pub fn box_settings_message(config: &CarplayConfig) -> ProtocolMessage {
    let drive_position = if config.left_hand_drive { 0 } else { 1 };
    let sync_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let wifi_channel = if config.wifi_5ghz { 36 } else { 6 };

    let json = serde_json::json!({
        "syncTime": sync_time,
        "fps": config.fps,
        "mediaDelay": config.media_delay_ms,
        "mediaSound": 1,
        "autoConn": config.autoconnect,
        "bgMode": 0,
        "startDelay": 0,
        "androidWorkMode": 1,
        "drivePosition": drive_position,
        "androidAutoSizeW": MEDIA_WIDTH,
        "androidAutoSizeH": MEDIA_HEIGHT,
        "ScreenDPI": config.dpi,
        "WiFiChannel": wifi_channel,
        "HiCarConnectMode": 0,
        "GNSSCapability": 3, // GPGGA + GPRMC supplied by the external GNSS service.
        "DashboardInfo": 1,
        "UseBTPhone": 0,
        "boxName": "NavBridge"
    });
    ProtocolMessage::new(CMD_JSON_CONTROL, json.to_string().into_bytes())
}

/// AndroidWorkMode is cleared by the adapter when the phone/session ends.
/// Re-apply it without tearing down the head-unit USB session.
pub fn android_auto_rearm_messages(config: &CarplayConfig) -> Vec<ProtocolMessage> {
    let mut messages = vec![
        file_i32("/etc/android_work_mode", 1),
        box_settings_message(config),
    ];
    if config.autoconnect {
        messages.push(ProtocolMessage::control(CONTROL_AUTOCONNECT));
    }
    messages
}

fn open(fps: u32) -> ProtocolMessage {
    let values = [MEDIA_WIDTH, MEDIA_HEIGHT, fps, 5, 49152, 2, 2];
    let mut payload = Vec::with_capacity(28);
    for value in values {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    ProtocolMessage::new(CMD_OPEN, payload)
}

fn le_words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn file_i32(path: &str, value: i32) -> ProtocolMessage {
    file_bytes(path, &value.to_le_bytes())
}

fn file_bytes(path: &str, bytes: &[u8]) -> ProtocolMessage {
    let mut filename = path.as_bytes().to_vec();
    filename.push(0);

    let mut payload = Vec::with_capacity(8 + filename.len() + bytes.len());
    payload.extend_from_slice(&(filename.len() as u32).to_le_bytes());
    payload.extend_from_slice(&filename);
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
    ProtocolMessage::new(CMD_SEND_FILE, payload)
}

pub struct MessageParser {
    buffer: Vec<u8>,
    offset: usize,
}

impl MessageParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(64 * 1024),
            offset: 0,
        }
    }

    /// Remaining bytes in the current header or payload. Call `next` after
    /// each read to consume complete messages and resynchronize invalid headers.
    pub fn bytes_needed(&self) -> usize {
        let pending = &self.buffer[self.offset..];
        if pending.len() < HEADER_SIZE {
            return HEADER_SIZE - pending.len();
        }
        let length = u32::from_le_bytes(pending[4..8].try_into().unwrap()) as usize;
        (HEADER_SIZE + length.min(MAX_PAYLOAD)).saturating_sub(pending.len()).max(1)
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn next(&mut self) -> Option<ProtocolMessage> {
        loop {
            let available = self.buffer.len().saturating_sub(self.offset);
            if available < HEADER_SIZE {
                self.compact();
                return None;
            }

            let header = &self.buffer[self.offset..self.offset + HEADER_SIZE];
            let magic = u32::from_le_bytes(header[0..4].try_into().ok()?);
            if magic != MAGIC && magic != MAGIC_ENCRYPTED {
                self.offset += 1;
                continue;
            }

            let length = i32::from_le_bytes(header[4..8].try_into().ok()?);
            let kind = u32::from_le_bytes(header[8..12].try_into().ok()?);
            let check = u32::from_le_bytes(header[12..16].try_into().ok()?);
            if length < 0 || length as usize > MAX_PAYLOAD || check != !kind {
                self.offset += 1;
                continue;
            }

            let total = HEADER_SIZE + length as usize;
            if available < total {
                self.compact();
                return None;
            }

            let payload_start = self.offset + HEADER_SIZE;
            let payload_end = self.offset + total;
            let payload = self.buffer[payload_start..payload_end].to_vec();
            self.offset += total;
            self.compact_if_needed();

            return Some(ProtocolMessage {
                kind,
                payload,
                encrypted: magic == MAGIC_ENCRYPTED,
            });
        }
    }

    fn compact_if_needed(&mut self) {
        if self.offset >= 64 * 1024 || self.offset * 2 >= self.buffer.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        if self.offset > 0 {
            self.buffer.drain(..self.offset);
            self.offset = 0;
        }
    }
}

pub fn payload_u32(message: &ProtocolMessage) -> Option<u32> {
    if message.payload.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes(message.payload[0..4].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_mode_uses_live_controls_and_restores_selected_mode_at_startup() {
        for (enabled, command) in [(false, 17), (true, 16)] {
            let messages = night_mode_messages(enabled);
            assert_eq!(payload_u32(&messages[0]), Some(command));
            assert_eq!(messages[1].payload, file_i32("/tmp/night_mode", i32::from(enabled)).payload);
            let startup = initialization_messages(&CarplayConfig::default(), enabled);
            assert!(startup.iter().any(|m| m.kind == messages[0].kind && m.payload == messages[0].payload));
        }
    }

    #[test]
    fn sets_independent_names_before_autoconnect_and_skips_empty_names() {
        let config = CarplayConfig { bluetooth_name: "My Car".into(), wifi_name: "Car-WiFi".into(), ..Default::default() };
        let messages = initialization_messages(&config, false);
        let bt = messages.iter().position(|m| m.kind == CMD_BLUETOOTH_NAME).unwrap();
        let wifi = messages.iter().position(|m| m.kind == CMD_WIFI_NAME).unwrap();
        let connect = messages.iter().position(|m| m.kind == CMD_CONTROL && payload_u32(m) == Some(CONTROL_AUTOCONNECT)).unwrap();
        assert_eq!(messages[bt].payload, b"My Car");
        assert_eq!(messages[wifi].payload, b"Car-WiFi");
        assert!(bt < connect && wifi < connect);
        let config = CarplayConfig { bluetooth_name: String::new(), wifi_name: String::new(), ..Default::default() };
        assert!(!initialization_messages(&config, false).iter().any(|m| matches!(m.kind, CMD_BLUETOOTH_NAME | CMD_WIFI_NAME)));
        // Retrying a phone must not rename/restart the radios mid-session.
        assert!(!android_auto_rearm_messages(&CarplayConfig::default()).iter().any(|m| matches!(m.kind, CMD_BLUETOOTH_NAME | CMD_WIFI_NAME)));
    }

    #[test]
    fn carplay_rectangles_use_video_geometry_before_open() {
        let messages = initialization_messages(&CarplayConfig::default(), false);
        let open_index = messages.iter().position(|m| m.kind == CMD_OPEN).unwrap();
        for (name, words) in [
            ("/etc/RiddleBoxData/HU_VIEWAREA_INFO", vec![800, 480, 800, 480, 0, 0]),
            ("/etc/RiddleBoxData/HU_SAFEAREA_INFO", vec![800, 480, 0, 0, 0]),
        ] {
            let expected = file_bytes(name, &le_words(&words));
            assert!(messages[..open_index].iter().any(|m| m.kind == CMD_SEND_FILE && m.payload == expected.payload));
        }
    }

    #[test]
    fn navigation_press_release_pairs_and_overlapping_keys() {
        let mut buttons = NavigationButtons::default();
        for (key, expected) in [
            (NavigationKey::Left, 100), (NavigationKey::Right, 101),
            (NavigationKey::Up, 102), (NavigationKey::Down, 103),
            (NavigationKey::Select, 104), (NavigationKey::Back, 106),
        ] {
            let press = buttons.update(key, true);
            assert_eq!(press.len(), 1);
            assert_eq!(press[0].kind, CMD_CONTROL);
            assert_eq!(payload_u32(&press[0]), Some(expected));
            assert!(buttons.update(key, true).is_empty());
            let release = buttons.update(key, false);
            assert_eq!(release.len(), 1);
            assert_eq!(payload_u32(&release[0]), Some(105));
            assert!(buttons.update(key, false).is_empty());
        }
        buttons.update(NavigationKey::Left, true);
        let switched = buttons.update(NavigationKey::Right, true);
        assert_eq!(switched.iter().map(payload_u32).collect::<Vec<_>>(), [Some(105), Some(101)]);
        // Releasing the old physical key must not release the newer held key.
        assert!(buttons.update(NavigationKey::Left, false).is_empty());
        assert_eq!(buttons.update(NavigationKey::Right, false).len(), 1);
    }

    #[test]
    fn heartbeat_is_header_only_and_matches_protocol() {
        let heartbeat = ProtocolMessage::heartbeat();
        assert!(heartbeat.payload.is_empty());
        assert_eq!(
            heartbeat.header(),
            [
                0xaa, 0x55, 0xaa, 0x55, // magic 0x55aa55aa LE
                0x00, 0x00, 0x00, 0x00, // length
                0xaa, 0x00, 0x00, 0x00, // type 170
                0x55, 0xff, 0xff, 0xff, // ~170
            ]
        );
    }

    #[test]
    fn custom_dpi_reaches_startup_and_android_rearm() {
        let config = CarplayConfig { dpi: 240, ..Default::default() };
        let expected = file_i32("/tmp/screen_dpi", 240);
        assert!(initialization_messages(&config, false).iter().any(|m| m.kind == expected.kind && m.payload == expected.payload));
        for message in std::iter::once(box_settings_message(&config)).chain(android_auto_rearm_messages(&config).into_iter().filter(|m| m.kind == CMD_JSON_CONTROL)) {
            let value: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
            assert_eq!(value["ScreenDPI"], 240);
            assert_eq!(value["androidAutoSizeW"], MEDIA_WIDTH);
            assert_eq!(value["androidAutoSizeH"], MEDIA_HEIGHT);
        }
    }

    #[test]
    fn box_settings_keep_android_auto_enabled_at_media_geometry() {
        let config = CarplayConfig::default();
        let message = box_settings_message(&config);
        assert_eq!(message.kind, CMD_JSON_CONTROL);
        let value: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
        assert_eq!(value["androidWorkMode"].as_u64(), Some(1));
        assert_eq!(value["androidAutoSizeW"].as_u64(), Some(MEDIA_WIDTH as u64));
        assert_eq!(value["androidAutoSizeH"].as_u64(), Some(MEDIA_HEIGHT as u64));
        assert_eq!(value["ScreenDPI"].as_u64(), Some(config.dpi as u64));
    }

    #[test]
    fn read_sizes_keep_header_and_payload_transfers_separate() {
        // Model a dongle that supplies the header and body in separate transfers.
        // An oversized header read must never be requested, even for tiny bodies.
        let messages = [
            ProtocolMessage::control(1004),
            ProtocolMessage::heartbeat(),
            ProtocolMessage::new(CMD_VIDEO, vec![0x67; 70_000]),
        ];
        let mut parser = MessageParser::new();
        for message in messages {
            assert_eq!(parser.bytes_needed(), HEADER_SIZE);
            let header = message.header();
            parser.push(&header[..7]);
            assert!(parser.next().is_none());
            assert_eq!(parser.bytes_needed(), 9);
            parser.push(&header[7..]);
            let parsed = if message.payload.is_empty() {
                parser.next().unwrap()
            } else {
                assert!(parser.next().is_none());
                let mut offset = 0;
                while offset < message.payload.len() {
                    assert_eq!(parser.bytes_needed(), message.payload.len() - offset);
                    let count = parser.bytes_needed().min(32_768);
                    parser.push(&message.payload[offset..offset + count]);
                    offset += count;
                    if offset < message.payload.len() {
                        assert!(parser.next().is_none());
                    }
                }
                parser.next().unwrap()
            };
            assert_eq!(parsed.kind, message.kind);
            assert_eq!(parsed.payload, message.payload);
            assert!(parser.next().is_none());
            assert_eq!(parser.bytes_needed(), HEADER_SIZE);
        }
    }

    #[test]
    fn read_sizes_recover_after_invalid_header() {
        let message = ProtocolMessage::control(1004);
        let mut parser = MessageParser::new();
        parser.push(&[0; HEADER_SIZE]);
        assert!(parser.next().is_none());
        let bytes = [message.header().as_slice(), message.payload.as_slice()].concat();
        for byte in bytes {
            assert!(parser.bytes_needed() > 0);
            parser.push(&[byte]);
            if let Some(parsed) = parser.next() {
                assert_eq!(parsed.kind, CMD_CONTROL);
                assert_eq!(payload_u32(&parsed), Some(1004));
                assert_eq!(parser.bytes_needed(), HEADER_SIZE);
                return;
            }
        }
        panic!("did not recover the message after an invalid header");
    }

    #[test]
    fn parser_accepts_fragmented_messages() {
        let message = ProtocolMessage::new(CMD_PHASE, 8_u32.to_le_bytes().to_vec());
        let header = message.header();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&message.payload);

        let mut parser = MessageParser::new();
        parser.push(&bytes[..7]);
        assert!(parser.next().is_none());
        parser.push(&bytes[7..17]);
        assert!(parser.next().is_none());
        parser.push(&bytes[17..]);

        let parsed = parser.next().expect("complete message");
        assert_eq!(parsed.kind, CMD_PHASE);
        assert_eq!(payload_u32(&parsed), Some(8));
        assert!(parser.next().is_none());
    }
}
