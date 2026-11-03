//! Android Auto's numbers: channels, message ids, codecs, sensors and key codes.

use crate::services::audio::Stream;
use protobuf::Message;

pub const INPUT: u8 = 1;

pub const SENSOR: u8 = 2;

pub const VIDEO: u8 = 3;

pub const MEDIA: u8 = 4;

pub const SPEECH: u8 = 5;

pub const SYSTEM: u8 = 6;

pub const MIC: u8 = 7;

/// The car's Bluetooth, for calls: offered only while something can take them.
pub const BLUETOOTH: u8 = 8;

/// Message ids. The same number means different things on different kinds of
/// channel, so they are grouped by the channel that carries them.
pub(super) mod msg {
    /// On any channel, about the channel itself.
    pub const CHANNEL_OPEN_REQUEST: u16 = 7;
    pub const CHANNEL_OPEN_RESPONSE: u16 = 8;
    pub const CHANNEL_CLOSE: u16 = 9;
    /// Channel 0.
    pub mod control {
        pub const VERSION_REQUEST: u16 = 1;
        pub const VERSION_RESPONSE: u16 = 2;
        pub const SSL_HANDSHAKE: u16 = 3;
        pub const AUTH_COMPLETE: u16 = 4;
        pub const SERVICE_DISCOVERY_REQUEST: u16 = 5;
        pub const SERVICE_DISCOVERY_RESPONSE: u16 = 6;
        pub const PING_REQUEST: u16 = 11;
        pub const PING_RESPONSE: u16 = 12;
        pub const NAVIGATION_FOCUS_REQUEST: u16 = 13;
        pub const NAVIGATION_FOCUS_RESPONSE: u16 = 14;
        pub const SHUTDOWN_REQUEST: u16 = 15;
        pub const SHUTDOWN_RESPONSE: u16 = 16;
        pub const AUDIO_FOCUS_REQUEST: u16 = 18;
        pub const AUDIO_FOCUS_RESPONSE: u16 = 19;
    }
    /// Video, audio and microphone channels.
    pub mod media {
        /// Media after an 8-byte timestamp.
        pub const DATA: u16 = 0;
        /// Codec configuration, without a timestamp.
        pub const CODEC_CONFIG: u16 = 1;
        pub const SETUP_REQUEST: u16 = 0x8000;
        pub const START: u16 = 0x8001;
        pub const STOP: u16 = 0x8002;
        pub const SETUP_RESPONSE: u16 = 0x8003;
        pub const ACK: u16 = 0x8004;
        pub const MIC_OPEN: u16 = 0x8005;
        pub const MIC_RESPONSE: u16 = 0x8006;
        pub const VIDEO_FOCUS_REQUEST: u16 = 0x8007;
        pub const VIDEO_FOCUS: u16 = 0x8008;
    }
    pub mod input {
        pub const EVENT: u16 = 0x8001;
        pub const BINDING_REQUEST: u16 = 0x8002;
        pub const BINDING_RESPONSE: u16 = 0x8003;
    }
    pub mod sensor {
        pub const START_REQUEST: u16 = 0x8001;
        pub const START_RESPONSE: u16 = 0x8002;
        pub const EVENT: u16 = 0x8003;
    }
    pub mod bluetooth {
        pub const PAIRING_REQUEST: u16 = 0x8001;
        pub const PAIRING_RESPONSE: u16 = 0x8002;
    }
}

/// Media codecs, as discovery offers them and the phone's setup request
/// chooses one.
pub(super) mod codec {
    pub const PCM: u32 = 1;
    pub const H264: u32 = 3;
    pub const H265: u32 = 7;
}

/// How phones pair with the car's Bluetooth: comparing the code both show,
/// as NavBridge's own pairing does.
pub(super) const PAIRING_NUMERIC_COMPARISON: u32 = 2;

/// Video focus: the phone's picture, or the car's own screen.
pub(super) const FOCUS_PROJECTED: u32 = 1;

pub(super) const FOCUS_NATIVE: u32 = 2;

/// Sensors the car can offer the phone.
pub(super) mod sensor_type {
    pub const LOCATION: u32 = 1;
    pub const SPEED: u32 = 3;
    pub const PARKING_BRAKE: u32 = 7;
    pub const NIGHT: u32 = 10;
    pub const DRIVING_STATUS: u32 = 13;
}

/// Android Auto's driving restrictions: none while parked and standing still,
/// all of them (video, keyboard, voice, setup, long messages) otherwise.
pub(super) const UNRESTRICTED: i32 = 0;

pub(super) const FULLY_RESTRICTED: i32 = 31;

/// Android key codes, then Android Auto's own car buttons above 65535.
pub(super) mod keycode {
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const CALL: u32 = 5;
    pub const ENDCALL: u32 = 6;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const DPAD_CENTER: u32 = 23;
    pub const MEDIA_PLAY_PAUSE: u32 = 85;
    pub const MEDIA_NEXT: u32 = 87;
    pub const MEDIA_PREVIOUS: u32 = 88;
    /// The car's voice button, which opens the phone's assistant: head
    /// units send Android's search key for it, not KEYCODE_VOICE_ASSIST.
    pub const SEARCH: u32 = 84;
    pub const ROTARY_CONTROLLER: u32 = 65536;
    pub const MEDIA: u32 = 65537;
    pub const NAVIGATION: u32 = 65538;
}

/// The keys the car tells the phone it has.
pub const KEYS: &[u32] = &[
    keycode::HOME,
    keycode::BACK,
    keycode::CALL,
    keycode::ENDCALL,
    keycode::DPAD_UP,
    keycode::DPAD_DOWN,
    keycode::DPAD_LEFT,
    keycode::DPAD_RIGHT,
    keycode::DPAD_CENTER,
    keycode::MEDIA_PLAY_PAUSE,
    keycode::MEDIA_NEXT,
    keycode::MEDIA_PREVIOUS,
    keycode::SEARCH,
    keycode::ROTARY_CONTROLLER,
    keycode::MEDIA,
    keycode::NAVIGATION,
];

pub(super) fn parse<M: Message>(data: &[u8]) -> Result<M, String> {
    M::parse_from_bytes(data).map_err(|e| e.to_string())
}

pub(super) fn audio_stream(id: u8) -> Option<Stream> {
    match id {
        MEDIA => Some(Stream::Media),
        SPEECH => Some(Stream::Speech),
        SYSTEM => Some(Stream::System),
        _ => None,
    }
}
