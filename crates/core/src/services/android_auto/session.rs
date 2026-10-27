use super::{
    framing::{self, Assembler, Parser},
    tls::Tls,
};
use crate::config::AndroidAutoConfig;
use crate::services::audio::Stream;
use crate::services::gnss::Fix;
use navbridge_common::{NavigationKey, PhoneAction, TouchKind, MEDIA_HEIGHT, MEDIA_WIDTH};
use protobuf::{Message, MessageField};
use std::time::{Duration, Instant};
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/aa/mod.rs"));
}
use generated::aa as p;
pub const INPUT: u8 = 1;
pub const SENSOR: u8 = 2;
pub const VIDEO: u8 = 3;
pub const MEDIA: u8 = 4;
pub const SPEECH: u8 = 5;
pub const SYSTEM: u8 = 6;
pub const MIC: u8 = 7;
/// Message ids. The same number means different things on different kinds of
/// channel, so they are grouped by the channel that carries them.
mod msg {
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
}
/// Video focus: the phone's picture, or the car's own screen.
const FOCUS_PROJECTED: u32 = 1;
const FOCUS_NATIVE: u32 = 2;
/// Sensors the car can offer the phone.
mod sensor_type {
    pub const LOCATION: u32 = 1;
    pub const PARKING_BRAKE: u32 = 7;
    pub const NIGHT: u32 = 10;
    pub const DRIVING_STATUS: u32 = 13;
}
/// Android Auto's driving restrictions: none while parked, all of them
/// (video, keyboard, voice, setup, long messages) otherwise.
const UNRESTRICTED: i32 = 0;
const FULLY_RESTRICTED: i32 = 31;
/// How old the last fix may be to answer the phone's request for location.
const FRESH_FIX: Duration = Duration::from_secs(2);
/// Android key codes, then Android Auto's own car buttons above 65535.
mod keycode {
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
    pub const VOICE_ASSIST: u32 = 231;
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
    keycode::VOICE_ASSIST,
    keycode::ROTARY_CONTROLLER,
    keycode::MEDIA,
    keycode::NAVIGATION,
];
pub enum Action {
    Connected(String),
    Video(Vec<u8>),
    Audio(Stream, Vec<u8>),
    StopAudio(Stream),
    Microphone(bool),
    Duck(bool),
    HostUi,
    /// The phone ended the session, or acknowledged our request to end it.
    Shutdown,
}
pub struct Session {
    config: AndroidAutoConfig,
    parser: Parser,
    assembler: Assembler,
    tls: Tls,
    pub outgoing: Vec<Vec<u8>>,
    pub opened: [bool; 8],
    sessions: [Option<i32>; 8],
    pub discovered: bool,
    night: bool,
    night_sensor: bool,
    /// The parking brake is on, so the phone lifts its driving restrictions.
    parked: bool,
    parking_brake_sensor: bool,
    driving_status_sensor: bool,
    /// A GNSS receiver was connected when the session started, so the phone
    /// is offered the car's location.
    gnss: bool,
    location_sensor: bool,
    last_fix: Option<(Fix, Instant)>,
    mic: bool,
    mic_window: u32,
    mic_unacked: u32,
    started: Instant,
    last_ping: Instant,
    pong: Option<(i64, Instant)>,
    clock: Instant,
}
fn parse<M: Message>(data: &[u8]) -> Result<M, String> {
    M::parse_from_bytes(data).map_err(|e| e.to_string())
}
pub fn discovery(config: &AndroidAutoConfig, gnss: bool) -> p::Discovery {
    let mut d = p::Discovery::new();
    d.set_head_unit_name("NavBridge".into());
    d.set_car_model("BMW".into());
    d.set_car_year("".into());
    d.set_car_serial("NavBridge".into());
    d.set_left_hand_drive_vehicle(config.left_hand_drive);
    d.set_headunit_manufacturer("NavBridge".into());
    d.set_headunit_model("CM5".into());
    d.set_sw_build("wired-rust".into());
    d.set_sw_version(env!("CARGO_PKG_VERSION").into());
    d.set_can_play_native_media_during_vr(false);
    let mut input = p::Channel::new();
    input.set_channel_id(INPUT as u32);
    let mut ic = p::InputChannel::new();
    ic.supported_keycodes = KEYS.to_vec();
    // Touch and the mouse work on the projection, in its own pixels.
    let mut touch = p::TouchConfig::new();
    touch.set_width(MEDIA_WIDTH);
    touch.set_height(MEDIA_HEIGHT);
    ic.touch_screen = MessageField::some(touch);
    input.input_channel = MessageField::some(ic);
    d.channels.push(input);
    let mut sensor = p::Channel::new();
    sensor.set_channel_id(SENSOR as u32);
    let mut sc = p::SensorChannel::new();
    let offered: &[u32] = if gnss {
        &[
            sensor_type::NIGHT,
            sensor_type::PARKING_BRAKE,
            sensor_type::DRIVING_STATUS,
            sensor_type::LOCATION,
        ]
    } else {
        &[
            sensor_type::NIGHT,
            sensor_type::PARKING_BRAKE,
            sensor_type::DRIVING_STATUS,
        ]
    };
    for &kind in offered {
        let mut s = p::Sensor::new();
        s.set_type(kind);
        sc.sensors.push(s);
    }
    sensor.sensor_channel = MessageField::some(sc);
    d.channels.push(sensor);
    for id in VIDEO..=SYSTEM {
        let mut c = p::Channel::new();
        c.set_channel_id(id as u32);
        let mut av = p::AVChannel::new();
        av.set_stream_type(if id == VIDEO { 3 } else { 1 });
        if id == VIDEO {
            let mut vc = p::VideoConfig::new();
            vc.set_video_resolution(1);
            // Current phone profile: 60=1, 30=2 (Open Android Auto).
            // Older AASDK definitions reverse these values. Keep this mapping
            // covered by the serialized discovery test and live-rate checks.
            vc.set_video_fps(if config.fps == 60 { 1 } else { 2 });
            vc.set_margin_width(0);
            vc.set_margin_height(0);
            vc.set_dpi(config.dpi);
            av.video_configs.push(vc);
        } else {
            let stream = audio_stream(id).unwrap();
            let fmt = stream.format();
            av.set_audio_type(match stream {
                Stream::Media => 3,
                Stream::Speech => 1,
                Stream::System => 2,
            });
            let mut a = p::AudioConfig::new();
            a.set_sample_rate(fmt.rate);
            a.set_channel_count(fmt.channels);
            a.set_bit_depth(16);
            av.audio_configs.push(a);
        }
        c.av_channel = MessageField::some(av);
        d.channels.push(c);
    }
    let mut c = p::Channel::new();
    c.set_channel_id(MIC as u32);
    let mut a = p::AVInputChannel::new();
    a.set_stream_type(1);
    let mut fmt = p::AudioConfig::new();
    fmt.set_sample_rate(16000);
    fmt.set_bit_depth(16);
    fmt.set_channel_count(1);
    a.audio_config = MessageField::some(fmt);
    c.av_input_channel = MessageField::some(a);
    d.channels.push(c);
    d
}
fn audio_stream(id: u8) -> Option<Stream> {
    match id {
        MEDIA => Some(Stream::Media),
        SPEECH => Some(Stream::Speech),
        SYSTEM => Some(Stream::System),
        _ => None,
    }
}
impl Session {
    pub fn new(
        config: AndroidAutoConfig,
        night: bool,
        parked: bool,
        gnss: bool,
    ) -> Result<Self, String> {
        let cert = config
            .certificate
            .as_ref()
            .map(std::fs::read)
            .transpose()
            .map_err(|e| e.to_string())?;
        let key = config
            .private_key
            .as_ref()
            .map(std::fs::read)
            .transpose()
            .map_err(|e| e.to_string())?;
        let tls = Tls::new(cert.as_deref(), key.as_deref())?;
        let now = Instant::now();
        let mut s = Self {
            config,
            parser: Parser::default(),
            assembler: Assembler::default(),
            tls,
            outgoing: vec![],
            opened: [false; 8],
            sessions: [None; 8],
            discovered: false,
            night,
            night_sensor: false,
            parked,
            parking_brake_sensor: false,
            driving_status_sensor: false,
            gnss,
            location_sensor: false,
            last_fix: None,
            mic: false,
            mic_window: 1,
            mic_unacked: 0,
            started: now,
            last_ping: now,
            pong: None,
            clock: now,
        };
        // Protocol version 1.1.
        s.raw(0, msg::control::VERSION_REQUEST, &[0, 1, 0, 1])?;
        Ok(s)
    }
    fn raw(&mut self, channel: u8, id: u16, body: &[u8]) -> Result<(), String> {
        let payload = [id.to_be_bytes().as_slice(), body].concat();
        let encrypted = self.tls.ready
            && !(channel == 0
                && matches!(
                    id,
                    msg::control::VERSION_REQUEST
                        | msg::control::VERSION_RESPONSE
                        | msg::control::SSL_HANDSHAKE
                        | msg::control::AUTH_COMPLETE
                        | msg::control::PING_REQUEST
                        | msg::control::PING_RESPONSE
                ));
        // Frame flags: the whole message in one frame (3), encrypted (8), and
        // a channel's own open response (4).
        let channel_open = channel != 0 && id == msg::CHANNEL_OPEN_RESPONSE;
        let flags = 3 | if encrypted { 8 } else { 0 } | if channel_open { 4 } else { 0 };
        let payload = if encrypted {
            self.tls.encrypt(&payload)?
        } else {
            payload
        };
        if self.outgoing.iter().map(Vec::len).sum::<usize>() + payload.len() > 2 * 1024 * 1024 {
            return Err("AA output backlog limit".into());
        }
        self.outgoing
            .push(framing::frame(channel, flags, None, &payload)?);
        Ok(())
    }
    fn send(&mut self, channel: u8, id: u16, msg: &impl Message) -> Result<(), String> {
        self.raw(
            channel,
            id,
            &msg.write_to_bytes().map_err(|e| e.to_string())?,
        )
    }
    fn status(&mut self, channel: u8, id: u16, ok: bool) -> Result<(), String> {
        let mut m = p::Status::new();
        m.set_status(if ok { 0 } else { 1 });
        self.send(channel, id, &m)
    }
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<Action>, String> {
        let mut actions = vec![];
        for mut f in self.parser.feed(bytes)? {
            tracing::trace!(
                channel = f.channel,
                flags = f.flags,
                bytes = f.data.len(),
                "AA frame received"
            );
            let encrypted = f.flags & 8 != 0;
            if encrypted {
                f.data = self.tls.decrypt(&f.data)?;
            }
            if let Some((channel, data)) = self.assembler.push(f)? {
                let id = u16::from_be_bytes(data[..2].try_into().unwrap());
                let body = &data[2..];
                // Media data arrives too often to log.
                if !matches!(id, msg::media::DATA | msg::media::CODEC_CONFIG) {
                    tracing::debug!(channel, id, bytes = body.len(), "AA message received");
                }
                let plaintext_allowed = channel == 0
                    && matches!(
                        id,
                        msg::control::AUTH_COMPLETE
                            | msg::control::PING_REQUEST
                            | msg::control::PING_RESPONSE
                    );
                if self.tls.ready && !encrypted && !plaintext_allowed {
                    return Err("plaintext message after authentication".into());
                }
                actions.extend(self.message(channel, id, body)?);
            }
        }
        Ok(actions)
    }
    fn message(&mut self, channel: u8, id: u16, body: &[u8]) -> Result<Vec<Action>, String> {
        let mut actions = vec![];
        if channel == 0 && id == msg::control::VERSION_RESPONSE {
            if self.tls.ready
                || body.len() < 6
                || u16::from_be_bytes(body[4..6].try_into().unwrap()) != 0
            {
                return Err("AA version rejected".into());
            }
            let output = self.tls.handshake(&[])?;
            if !output.is_empty() {
                self.raw(0, msg::control::SSL_HANDSHAKE, &output)?;
            }
            return Ok(actions);
        }
        if channel == 0 && id == msg::control::SSL_HANDSHAKE && !self.tls.ready {
            let output = self.tls.handshake(body)?;
            if !output.is_empty() {
                self.raw(0, msg::control::SSL_HANDSHAKE, &output)?;
            }
            if self.tls.ready {
                self.status(0, msg::control::AUTH_COMPLETE, true)?;
                tracing::info!("Android Auto TLS authenticated");
            }
            return Ok(actions);
        }
        if !self.tls.ready {
            return Err("AA message before TLS authentication".into());
        }
        if channel == 0 {
            match id {
                msg::control::SERVICE_DISCOVERY_REQUEST => {
                    let request = parse::<p::DiscoveryRequest>(body)?;
                    self.send(
                        0,
                        msg::control::SERVICE_DISCOVERY_RESPONSE,
                        &discovery(&self.config, self.gnss),
                    )?;
                    self.discovered = true;
                    actions.push(Action::Connected(request.device_name().into()));
                }
                msg::CHANNEL_OPEN_REQUEST => {
                    let r = parse::<p::Open>(body)?;
                    let target = r.channel_id();
                    let ok = self.discovered && (1..=7).contains(&target);
                    let reply_on = if ok { target as u8 } else { 0 };
                    self.status(reply_on, msg::CHANNEL_OPEN_RESPONSE, ok)?;
                    if ok {
                        self.opened[target as usize] = true;
                    }
                }
                msg::control::PING_REQUEST => {
                    let ping = parse::<p::Ping>(body)?;
                    self.send(0, msg::control::PING_RESPONSE, &ping)?;
                }
                msg::control::PING_RESPONSE => {
                    let p = parse::<p::Ping>(body)?;
                    if self.pong.is_some_and(|(ts, _)| ts == p.timestamp()) {
                        self.pong = None;
                    }
                }
                msg::control::NAVIGATION_FOCUS_REQUEST => {
                    // The phone's navigation, not the car's, has focus.
                    let mut f = p::Focus::new();
                    f.set_type(2);
                    self.send(0, msg::control::NAVIGATION_FOCUS_RESPONSE, &f)?;
                }
                msg::control::SHUTDOWN_REQUEST => {
                    self.raw(0, msg::control::SHUTDOWN_RESPONSE, &[])?;
                    actions.push(Action::Shutdown);
                }
                msg::control::SHUTDOWN_RESPONSE => actions.push(Action::Shutdown),
                msg::control::AUDIO_FOCUS_REQUEST => {
                    let f = parse::<p::Focus>(body)?;
                    let mut r = p::Focus::new();
                    // Gain, transient gain and guidance (which ducks media) are
                    // granted as asked; anything else is a release.
                    r.set_type(match f.type_() {
                        1 => 1,
                        2 => 2,
                        3 => 7,
                        _ => 3,
                    });
                    self.send(0, msg::control::AUDIO_FOCUS_RESPONSE, &r)?;
                    actions.push(Action::Duck(matches!(f.type_(), 2 | 3)));
                }
                // Other known control messages need no reply.
                8..=10 | 14 | 17 | 20..=26 => (),
                _ => tracing::debug!(channel, id, "ignored AA control message"),
            }
            return Ok(actions);
        }
        if !(1..=7).contains(&channel) {
            return Err("unadvertised AA channel".into());
        }
        if id == msg::CHANNEL_OPEN_REQUEST {
            let _ = parse::<p::Open>(body)?;
            self.opened[channel as usize] = true;
            self.status(channel, msg::CHANNEL_OPEN_RESPONSE, true)?;
            return Ok(actions);
        }
        if id == msg::CHANNEL_CLOSE {
            self.opened[channel as usize] = false;
            self.sessions[channel as usize] = None;
            if channel == MIC {
                self.mic = false;
                actions.push(Action::Microphone(false));
            }
            if let Some(s) = audio_stream(channel) {
                actions.push(Action::StopAudio(s));
            }
            return Ok(actions);
        }
        if !self.opened[channel as usize] {
            return Err("AA traffic on unopened channel".into());
        }
        match channel {
            INPUT => {
                if id == msg::input::BINDING_REQUEST {
                    let r = parse::<p::Binding>(body)?;
                    self.status(
                        INPUT,
                        msg::input::BINDING_RESPONSE,
                        r.scan_codes.iter().all(|k| KEYS.contains(&(*k as u32))),
                    )?;
                }
            }
            SENSOR => {
                if id == msg::sensor::START_REQUEST {
                    // The phone starts each offered sensor with its own request.
                    let r = parse::<p::SensorStart>(body)?;
                    match r.sensor_type() {
                        sensor_type::NIGHT => {
                            self.night_sensor = true;
                            self.status(SENSOR, msg::sensor::START_RESPONSE, true)?;
                            self.night(self.night)?;
                        }
                        sensor_type::PARKING_BRAKE => {
                            self.parking_brake_sensor = true;
                            self.status(SENSOR, msg::sensor::START_RESPONSE, true)?;
                            self.send_parking_brake()?;
                        }
                        sensor_type::DRIVING_STATUS => {
                            self.driving_status_sensor = true;
                            self.status(SENSOR, msg::sensor::START_RESPONSE, true)?;
                            self.send_driving_status()?;
                        }
                        sensor_type::LOCATION if self.gnss => {
                            self.location_sensor = true;
                            self.status(SENSOR, msg::sensor::START_RESPONSE, true)?;
                            tracing::info!("Android Auto uses the car's GNSS position");
                            // Answer at once rather than at the receiver's next fix.
                            if let Some((fix, _)) = self
                                .last_fix
                                .take()
                                .filter(|(_, at)| at.elapsed() < FRESH_FIX)
                            {
                                self.location(fix)?;
                            }
                        }
                        _ => self.status(SENSOR, msg::sensor::START_RESPONSE, false)?,
                    }
                }
            }
            VIDEO | MEDIA | SPEECH | SYSTEM | MIC => match id {
                msg::media::SETUP_REQUEST => {
                    let r = parse::<p::SetupRequest>(body)?;
                    let mut m = p::SetupResponse::new();
                    // Codecs: H.264 video (3) and PCM audio (1).
                    let supported = r.media_codec_type() == if channel == VIDEO { 3 } else { 1 };
                    // Status: ready (2) or failed (1).
                    m.set_media_status(if supported { 2 } else { 1 });
                    m.set_max_unacked(if channel == VIDEO { 2 } else { 10 });
                    if supported {
                        m.configs.push(0);
                    }
                    self.send(channel, msg::media::SETUP_RESPONSE, &m)?;
                    if channel == VIDEO && supported {
                        self.resume()?;
                    }
                }
                msg::media::START => {
                    let r = parse::<p::Start>(body)?;
                    if r.config() != 0 {
                        return Err("unsupported AA media config".into());
                    }
                    self.sessions[channel as usize] = Some(r.session());
                }
                msg::media::STOP => {
                    self.sessions[channel as usize] = None;
                    if let Some(s) = audio_stream(channel) {
                        actions.push(Action::StopAudio(s));
                    }
                    if channel == MIC {
                        self.mic = false;
                        actions.push(Action::Microphone(false));
                    }
                }
                msg::media::DATA | msg::media::CODEC_CONFIG => {
                    let data = if id == msg::media::DATA {
                        body.get(8..).ok_or("truncated media timestamp")?
                    } else {
                        body
                    };
                    let session = self.sessions[channel as usize];
                    if session.is_none() && !(channel == VIDEO && id == msg::media::CODEC_CONFIG) {
                        return Err("AA media before START".into());
                    }
                    if channel == VIDEO {
                        actions.push(Action::Video(data.to_vec()));
                    } else if let Some(s) = audio_stream(channel) {
                        actions.push(Action::Audio(s, data.to_vec()));
                    }
                    if let Some(session) = session {
                        let mut ack = p::Ack::new();
                        ack.set_session(session);
                        ack.set_value(1);
                        self.send(channel, msg::media::ACK, &ack)?;
                    }
                }
                msg::media::MIC_OPEN if channel == MIC => {
                    let m = parse::<p::MicOpen>(body)?;
                    self.mic = m.open();
                    self.mic_window = m.max_unacked().clamp(1, 10) as u32;
                    self.mic_unacked = 0;
                    actions.push(Action::Microphone(self.mic));
                    let mut r = p::MicResponse::new();
                    r.set_session(self.sessions[MIC as usize].unwrap_or(0));
                    r.set_value(0);
                    self.send(MIC, msg::media::MIC_RESPONSE, &r)?;
                }
                msg::media::ACK if channel == MIC => {
                    let ack = parse::<p::Ack>(body)?;
                    if ack.session() == self.sessions[MIC as usize].unwrap_or(0) {
                        let n = if ack.receive_timestamps.is_empty() {
                            ack.value()
                        } else {
                            ack.receive_timestamps.len() as u32
                        };
                        if n <= self.mic_unacked {
                            self.mic_unacked -= n;
                        }
                    }
                }
                msg::media::VIDEO_FOCUS_REQUEST if channel == VIDEO => {
                    let r = parse::<p::VideoFocusRequest>(body)?;
                    let mut f = p::VideoFocus::new();
                    f.set_mode(r.mode());
                    f.set_unrequested(false);
                    self.send(VIDEO, msg::media::VIDEO_FOCUS, &f)?;
                    if r.mode() == FOCUS_NATIVE {
                        actions.push(Action::HostUi);
                    }
                }
                _ => tracing::debug!(channel, id, "ignored optional AA media message"),
            },
            _ => (),
        }
        Ok(actions)
    }
    /// The car's own screen covers the projection, so the phone can stop
    /// sending video, as when it asks for the car's screen itself.
    pub fn hide(&mut self) -> Result<(), String> {
        self.video_focus(FOCUS_NATIVE)
    }
    pub fn resume(&mut self) -> Result<(), String> {
        self.video_focus(FOCUS_PROJECTED)
    }
    /// Android Auto restarts its video with a keyframe when it gets video
    /// focus back, so the decoder can recover after dropping frames.
    pub fn request_keyframe(&mut self) -> Result<(), String> {
        self.hide()?;
        self.resume()
    }
    fn video_focus(&mut self, mode: u32) -> Result<(), String> {
        if self.opened[VIDEO as usize] {
            let mut f = p::VideoFocus::new();
            f.set_mode(mode);
            f.set_unrequested(true);
            self.send(VIDEO, msg::media::VIDEO_FOCUS, &f)?;
        }
        Ok(())
    }
    pub fn night(&mut self, enabled: bool) -> Result<(), String> {
        self.night = enabled;
        if self.night_sensor && self.opened[SENSOR as usize] {
            let mut n = p::Night::new();
            n.set_is_night(enabled);
            let mut e = p::SensorEvent::new();
            e.night_mode.push(n);
            self.send(SENSOR, msg::sensor::EVENT, &e)?;
        }
        Ok(())
    }
    /// The parking brake, and with it the driving restrictions: none while it
    /// is on, all of them while it is off.
    pub fn parking_brake(&mut self, engaged: bool) -> Result<(), String> {
        self.parked = engaged;
        self.send_parking_brake()?;
        self.send_driving_status()
    }
    fn send_parking_brake(&mut self) -> Result<(), String> {
        if !(self.parking_brake_sensor && self.opened[SENSOR as usize]) {
            return Ok(());
        }
        let mut brake = p::ParkingBrake::new();
        brake.set_parking_brake(self.parked);
        let mut e = p::SensorEvent::new();
        e.parking_brake.push(brake);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }
    fn send_driving_status(&mut self) -> Result<(), String> {
        if !(self.driving_status_sensor && self.opened[SENSOR as usize]) {
            return Ok(());
        }
        let mut status = p::DrivingStatus::new();
        status.set_status(if self.parked {
            UNRESTRICTED
        } else {
            FULLY_RESTRICTED
        });
        let mut e = p::SensorEvent::new();
        e.driving_status.push(status);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }
    /// Passes the receiver's fix to the phone once it has started the
    /// location sensor, and keeps the latest one to answer that start.
    pub fn location(&mut self, fix: Fix) -> Result<(), String> {
        if !(self.location_sensor && self.opened[SENSOR as usize]) {
            self.last_fix = Some((fix, Instant::now()));
            return Ok(());
        }
        let scaled = |value: f64, scale: f64| (value * scale).round() as i32;
        let mut l = p::Location::new();
        l.set_latitude(scaled(fix.latitude, 1e7));
        l.set_longitude(scaled(fix.longitude, 1e7));
        if let Some(accuracy) = fix.accuracy {
            l.set_accuracy((accuracy * 1e3).round() as u32);
        }
        if let Some(altitude) = fix.altitude {
            l.set_altitude(scaled(altitude, 1e2));
        }
        if let Some(speed) = fix.speed {
            l.set_speed(scaled(speed, 1e3));
        }
        if let Some(bearing) = fix.bearing {
            l.set_bearing(scaled(bearing, 1e6));
        }
        let mut e = p::SensorEvent::new();
        e.location.push(l);
        self.send(SENSOR, msg::sensor::EVENT, &e)
    }
    pub fn navigation_input(&mut self, key: NavigationKey, pressed: bool) -> Result<(), String> {
        // AA uses D-pad up/down to switch focus regions and rotary input to
        // walk the items within a region (taskbar and launcher included).
        let keycode = match key {
            NavigationKey::Up => keycode::DPAD_UP,
            NavigationKey::Down => keycode::DPAD_DOWN,
            NavigationKey::Select | NavigationKey::Back => {
                // Android clicks and goes back when the key is released, so
                // the whole tap goes out on press: holding adds no delay.
                if pressed {
                    let keycode = if key == NavigationKey::Select {
                        keycode::DPAD_CENTER
                    } else {
                        keycode::BACK
                    };
                    self.key(keycode, true)?;
                    self.key(keycode, false)?;
                }
                return Ok(());
            }
            NavigationKey::Left | NavigationKey::Right => {
                if pressed && self.opened[INPUT as usize] {
                    let mut step = p::RelativeInput::new();
                    step.set_scan_code(keycode::ROTARY_CONTROLLER);
                    step.set_delta(if key == NavigationKey::Left { -1 } else { 1 });
                    let mut steps = p::RelativeInputs::new();
                    steps.events.push(step);
                    let mut input = p::Input::new();
                    input.set_timestamp(self.clock.elapsed().as_micros() as u64);
                    input.relative = MessageField::some(steps);
                    self.send(INPUT, msg::input::EVENT, &input)?;
                }
                return Ok(());
            }
        };
        self.key(keycode, pressed)
    }
    /// One finger, or the mouse, at a point of the 800x480 projection.
    pub fn touch(&mut self, kind: TouchKind, x: u32, y: u32) -> Result<(), String> {
        if !self.opened[INPUT as usize] {
            return Ok(());
        }
        let mut location = p::TouchLocation::new();
        location.set_x(x.min(MEDIA_WIDTH - 1));
        location.set_y(y.min(MEDIA_HEIGHT - 1));
        location.set_pointer_id(0);
        let mut touch = p::TouchEvent::new();
        touch.locations.push(location);
        touch.set_action_index(0);
        touch.set_action(match kind {
            TouchKind::Down => 0,
            TouchKind::Up => 1,
            TouchKind::Move => 2,
        });
        let mut input = p::Input::new();
        input.set_timestamp(self.clock.elapsed().as_micros() as u64);
        input.touch = MessageField::some(touch);
        self.send(INPUT, msg::input::EVENT, &input)
    }
    pub fn key(&mut self, key: u32, pressed: bool) -> Result<(), String> {
        if self.opened[INPUT as usize] && KEYS.contains(&key) {
            let mut b = p::Button::new();
            b.set_keycode(key);
            b.set_pressed(pressed);
            b.set_meta(0);
            let mut bs = p::Buttons::new();
            bs.events.push(b);
            let mut e = p::Input::new();
            e.set_timestamp(self.clock.elapsed().as_micros() as u64);
            e.buttons = MessageField::some(bs);
            self.send(INPUT, msg::input::EVENT, &e)?;
        }
        Ok(())
    }
    pub fn microphone(&mut self, pcm: &[u8], time: u64) -> Result<bool, String> {
        if self.mic && self.opened[MIC as usize] && self.mic_unacked < self.mic_window {
            let body = [time.to_be_bytes().as_slice(), pcm].concat();
            self.raw(MIC, msg::media::DATA, &body)?;
            self.mic_unacked += 1;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn tick(&mut self) -> Result<(), String> {
        if !self.discovered && self.started.elapsed() > Duration::from_secs(30) {
            return Err("AA negotiation timeout".into());
        }
        if self
            .pong
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(10))
        {
            return Err("AA pong timeout".into());
        }
        if self.discovered
            && self.last_ping.elapsed() > Duration::from_secs(2)
            && self.pong.is_none()
        {
            let ts = self.clock.elapsed().as_micros() as i64;
            let mut p = p::Ping::new();
            p.set_timestamp(ts);
            self.send(0, msg::control::PING_REQUEST, &p)?;
            self.pong = Some((ts, Instant::now()));
            self.last_ping = Instant::now();
        }
        Ok(())
    }
    pub fn shutdown(&mut self) -> Result<(), String> {
        // Reason: the user quit.
        let mut f = p::Focus::new();
        f.set_type(1);
        self.send(0, msg::control::SHUTDOWN_REQUEST, &f)
    }
}
pub fn phone_key(action: PhoneAction) -> u32 {
    match action {
        PhoneAction::NextTrack => keycode::MEDIA_NEXT,
        PhoneAction::PreviousTrack => keycode::MEDIA_PREVIOUS,
        PhoneAction::VoiceAssistant => keycode::VOICE_ASSIST,
        PhoneAction::AnswerCall => keycode::CALL,
        PhoneAction::EndCall => keycode::ENDCALL,
        PhoneAction::PhoneHome => keycode::HOME,
        PhoneAction::OpenMedia => keycode::MEDIA,
        PhoneAction::OpenMaps => keycode::NAVIGATION,
        PhoneAction::PlayPause => keycode::MEDIA_PLAY_PAUSE,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn connected_session() -> Session {
        let mut s = Session::new(AndroidAutoConfig::default(), false, false, false).unwrap();
        s.tls = super::super::tls::tests::pair().0;
        s.outgoing.clear();
        s.discovered = true;
        s
    }
    #[test]
    fn shutdown_request_and_reply_both_end_the_session() {
        let mut s = connected_session();
        // The phone asks to stop: we reply, then end the session.
        assert!(matches!(
            s.message(0, 15, &[8, 1]).unwrap().as_slice(),
            [Action::Shutdown]
        ));
        assert_eq!(s.outgoing.len(), 1);
        // The phone acknowledges our own request to stop.
        assert!(matches!(
            s.message(0, 16, &[]).unwrap().as_slice(),
            [Action::Shutdown]
        ));
    }
    #[test]
    fn focus_restore_preserves_media_session_and_controls() {
        let mut s = connected_session();
        s.opened[VIDEO as usize] = true;
        s.opened[INPUT as usize] = true;
        s.sessions[VIDEO as usize] = Some(42);
        let mut focus = p::VideoFocusRequest::new();
        focus.set_mode(2);
        let actions = s
            .message(VIDEO, 0x8007, &focus.write_to_bytes().unwrap())
            .unwrap();
        assert!(matches!(actions.as_slice(), [Action::HostUi]));
        s.resume().unwrap();
        s.key(19, true).unwrap();
        s.key(19, false).unwrap();
        assert_eq!(s.sessions[VIDEO as usize], Some(42));
        assert_eq!(s.outgoing.len(), 4);
        let fresh = Session::new(AndroidAutoConfig::default(), false, false, false).unwrap();
        assert!(!fresh.opened[VIDEO as usize]);
        assert!(fresh.sessions.iter().all(Option::is_none));
        assert!(!fresh.night_sensor);
        assert_eq!(fresh.mic_unacked, 0);
    }
    #[test]
    fn microphone_window_ack_and_stop_are_generation_bound() {
        let mut s = connected_session();
        s.opened[MIC as usize] = true;
        s.sessions[MIC as usize] = Some(12);
        let mut open = p::MicOpen::new();
        open.set_open(true);
        open.set_max_unacked(1);
        s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
            .unwrap();
        assert!(s.microphone(&[0; 640], 10).unwrap());
        assert!(!s.microphone(&[0; 640], 20).unwrap());
        let mut ack = p::Ack::new();
        ack.set_session(11);
        ack.set_value(1);
        s.message(MIC, 0x8004, &ack.write_to_bytes().unwrap())
            .unwrap();
        assert_eq!(s.mic_unacked, 1);
        ack.set_session(12);
        s.message(MIC, 0x8004, &ack.write_to_bytes().unwrap())
            .unwrap();
        assert!(s.microphone(&[0; 640], 30).unwrap());
        open.set_open(false);
        s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
            .unwrap();
        assert!(!s.microphone(&[0; 640], 40).unwrap());
    }
    #[test]
    fn horizontal_navigation_uses_signed_rotary_and_vertical_uses_dpad() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[INPUT as usize] = true;
        for key in [NavigationKey::Left, NavigationKey::Right, NavigationKey::Up] {
            s.navigation_input(key, true).unwrap();
            s.navigation_input(key, false).unwrap();
        }
        assert_eq!(s.outgoing.len(), 4);
        let inputs = sent_inputs(&mut s, &mut peer);
        for (input, delta) in inputs[..2].iter().zip([-1, 1]) {
            let step = &input.relative.as_ref().unwrap().events[0];
            assert_eq!(step.scan_code(), 65536);
            assert_eq!(step.delta(), delta);
        }
        assert!(inputs[2].buttons.as_ref().unwrap().events[0].pressed());
        assert!(!inputs[3].buttons.as_ref().unwrap().events[0].pressed());
        assert_eq!(inputs[2].buttons.as_ref().unwrap().events[0].keycode(), 19);
        assert_eq!(phone_key(PhoneAction::VoiceAssistant), 231);
    }
    #[test]
    fn select_and_back_send_the_whole_tap_on_press() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[INPUT as usize] = true;
        for (key, keycode) in [(NavigationKey::Select, 23), (NavigationKey::Back, 4)] {
            s.navigation_input(key, true).unwrap();
            let buttons = sent_inputs(&mut s, &mut peer)
                .iter()
                .map(|input| {
                    let button = &input.buttons.as_ref().unwrap().events[0];
                    (button.keycode(), button.pressed())
                })
                .collect::<Vec<_>>();
            assert_eq!(buttons, [(keycode, true), (keycode, false)]);
            // The release already went out with the press.
            s.navigation_input(key, false).unwrap();
            assert!(s.outgoing.is_empty());
        }
    }
    #[test]
    fn touches_go_out_in_the_projections_pixels() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        // Nothing before the phone opens its input channel.
        s.touch(TouchKind::Down, 10, 10).unwrap();
        assert!(s.outgoing.is_empty());
        s.opened[INPUT as usize] = true;
        for (kind, x, y) in [
            (TouchKind::Down, 100, 200),
            (TouchKind::Move, 120, 210),
            (TouchKind::Up, 900, 600),
        ] {
            s.touch(kind, x, y).unwrap();
        }
        let touches = sent_inputs(&mut s, &mut peer)
            .iter()
            .map(|input| {
                let touch = input.touch.as_ref().unwrap();
                let at = &touch.locations[0];
                (touch.action(), at.x(), at.y(), at.pointer_id())
            })
            .collect::<Vec<_>>();
        // Android's down, move and up; a point past the edge stays on it.
        assert_eq!(
            touches,
            [(0, 100, 200, 0), (2, 120, 210, 0), (1, 799, 479, 0)]
        );
    }
    /// Decrypts the messages queued for the phone, as (message id, body).
    fn sent(
        s: &mut Session,
        peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>,
    ) -> Vec<(u16, Vec<u8>)> {
        let mut parser = Parser::default();
        let frames = parser
            .feed(&s.outgoing.drain(..).flatten().collect::<Vec<_>>())
            .unwrap();
        frames
            .into_iter()
            .map(|f| {
                let data = super::super::tls::tests::decode(peer, &f.data);
                (u16::from_be_bytes([data[0], data[1]]), data[2..].to_vec())
            })
            .collect()
    }
    /// Decrypts the input reports queued for the phone.
    fn sent_inputs(
        s: &mut Session,
        peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>,
    ) -> Vec<p::Input> {
        sent(s, peer)
            .into_iter()
            .map(|(id, body)| {
                assert_eq!(id, 0x8001);
                parse::<p::Input>(&body).unwrap()
            })
            .collect()
    }
    #[test]
    fn hiding_resuming_and_keyframe_requests_move_video_focus() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[VIDEO as usize] = true;
        s.hide().unwrap();
        s.resume().unwrap();
        s.request_keyframe().unwrap();
        let focus = sent(&mut s, &mut peer)
            .into_iter()
            .map(|(id, body)| {
                assert_eq!(id, 0x8008);
                let f = parse::<p::VideoFocus>(&body).unwrap();
                (f.mode(), f.unrequested())
            })
            .collect::<Vec<_>>();
        // Hidden, shown, then taken and given back for a keyframe.
        assert_eq!(focus, [(2, true), (1, true), (2, true), (1, true)]);
    }
    #[test]
    fn setup_negotiates_codec_not_legacy_config_index() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[VIDEO as usize] = true;
        s.opened[MEDIA as usize] = true;
        let mut setup = p::SetupRequest::new();
        for (channel, codec, status) in [(VIDEO, 3, 2), (MEDIA, 1, 2), (MEDIA, 3, 1)] {
            setup.set_media_codec_type(codec);
            s.message(channel, 0x8000, &setup.write_to_bytes().unwrap())
                .unwrap();
            let mut parser = Parser::default();
            let frames = parser
                .feed(&s.outgoing.drain(..).flatten().collect::<Vec<_>>())
                .unwrap();
            for (index, frame) in frames.into_iter().enumerate() {
                let data = super::super::tls::tests::decode(&mut peer, &frame.data);
                if index == 0 {
                    assert_eq!(&data[..2], &0x8003u16.to_be_bytes());
                    let response = parse::<p::SetupResponse>(&data[2..]).unwrap();
                    assert_eq!(response.media_status(), status);
                    assert_eq!(response.configs, if status == 2 { vec![0] } else { vec![] });
                }
            }
        }
    }
    #[test]
    fn video_configuration_before_start_is_forwarded_without_invalid_ack() {
        let mut s = connected_session();
        s.opened[VIDEO as usize] = true;
        let config = [0, 0, 0, 1, 0x67, 1, 2, 3];
        assert!(
            matches!(s.message(VIDEO,1,&config).unwrap().as_slice(),[Action::Video(v)] if v==&config)
        );
        assert!(s.outgoing.is_empty());
        assert!(s.message(VIDEO, 0, &[0; 16]).is_err());
    }
    #[test]
    fn discovery_serializes_requested_frame_rates() {
        for (fps, wire) in [(30, 2), (60, 1)] {
            let config = AndroidAutoConfig {
                fps,
                ..Default::default()
            };
            let bytes = discovery(&config, false).write_to_bytes().unwrap();
            let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
            let video = &decoded.channels[VIDEO as usize - 1]
                .av_channel
                .as_ref()
                .unwrap()
                .video_configs[0];
            assert_eq!(video.video_fps(), wire);
        }
    }
    #[test]
    fn location_is_offered_only_while_a_receiver_is_connected() {
        // Night, parking brake and driving status always; location with a receiver.
        for (gnss, sensors) in [(false, vec![10, 7, 13]), (true, vec![10, 7, 13, 1])] {
            let bytes = discovery(&AndroidAutoConfig::default(), gnss)
                .write_to_bytes()
                .unwrap();
            let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
            let offered = decoded.channels[SENSOR as usize - 1]
                .sensor_channel
                .sensors
                .iter()
                .map(|s| s.type_())
                .collect::<Vec<_>>();
            assert_eq!(offered, sensors);
        }
    }
    #[test]
    fn fixes_reach_the_phone_once_it_starts_the_location_sensor() {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.gnss = true;
        s.opened[SENSOR as usize] = true;
        let fix = Fix {
            latitude: 48.5,
            longitude: -2.25,
            accuracy: Some(3.5),
            altitude: Some(117.0),
            speed: Some(12.5),
            bearing: Some(90.0),
        };
        // Before the phone asks, the fix is only kept.
        s.location(fix.clone()).unwrap();
        assert!(s.outgoing.is_empty());
        let start = |kind: u32| {
            let mut r = p::SensorStart::new();
            r.set_sensor_type(kind);
            r.write_to_bytes().unwrap()
        };
        // Night and location both start; the kept fix answers at once.
        s.message(SENSOR, 0x8001, &start(10)).unwrap();
        s.message(SENSOR, 0x8001, &start(1)).unwrap();
        let mut still = fix.clone();
        still.bearing = None;
        s.location(still).unwrap();
        let messages = sent(&mut s, &mut peer);
        let ids = messages.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(ids, [0x8002, 0x8003, 0x8002, 0x8003, 0x8003]);
        let status = parse::<p::Status>(&messages[2].1).unwrap();
        assert_eq!(status.status(), 0);
        let first = parse::<p::SensorEvent>(&messages[3].1).unwrap();
        let l = &first.location[0];
        assert_eq!(
            (l.latitude(), l.longitude(), l.accuracy(), l.altitude()),
            (485_000_000, -22_500_000, 3_500, 11_700)
        );
        assert_eq!((l.speed(), l.bearing()), (12_500, 90_000_000));
        let second = parse::<p::SensorEvent>(&messages[4].1).unwrap();
        assert!(!second.location[0].has_bearing());

        // Without a receiver when the phone connected, location is refused.
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[SENSOR as usize] = true;
        s.message(SENSOR, 0x8001, &start(1)).unwrap();
        s.location(fix).unwrap();
        let messages = sent(&mut s, &mut peer);
        assert_eq!(messages.len(), 1);
        assert_eq!(parse::<p::Status>(&messages[0].1).unwrap().status(), 1);
    }
    #[test]
    fn the_parking_brake_lifts_the_driving_restrictions() {
        let start = |kind: u32| {
            let mut r = p::SensorStart::new();
            r.set_sensor_type(kind);
            r.write_to_bytes().unwrap()
        };
        let events = |s: &mut Session,
                      peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>|
         -> Vec<p::SensorEvent> {
            sent(s, peer)
                .into_iter()
                .filter(|(id, _)| *id == 0x8003)
                .map(|(_, body)| parse::<p::SensorEvent>(&body).unwrap())
                .collect()
        };
        let brake = |e: &p::SensorEvent| e.parking_brake.first().map(|b| b.parking_brake());
        let status = |e: &p::SensorEvent| e.driving_status.first().map(|d| d.status());

        // Until the brake is reported on, the car may be moving.
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[SENSOR as usize] = true;
        s.message(SENSOR, 0x8001, &start(7)).unwrap();
        s.message(SENSOR, 0x8001, &start(13)).unwrap();
        s.parking_brake(true).unwrap();
        s.parking_brake(false).unwrap();
        let sent = events(&mut s, &mut peer);
        assert_eq!(
            sent.iter().map(brake).collect::<Vec<_>>(),
            [Some(false), None, Some(true), None, Some(false), None]
        );
        assert_eq!(
            sent.iter().map(status).collect::<Vec<_>>(),
            [None, Some(31), None, Some(0), None, Some(31)]
        );

        // A session that starts with the brake on answers without restrictions.
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.tls = tls;
        s.opened[SENSOR as usize] = true;
        s.parked = true;
        s.message(SENSOR, 0x8001, &start(13)).unwrap();
        let sent = events(&mut s, &mut peer);
        assert_eq!(sent.iter().map(status).collect::<Vec<_>>(), [Some(0)]);
    }
    #[test]
    fn discovery_has_wired_channels_and_consistent_pcm_formats() {
        let d = discovery(&AndroidAutoConfig::default(), false);
        let bytes = d.write_to_bytes().unwrap();
        let parsed = parse::<p::Discovery>(&bytes).unwrap();
        assert_eq!(parsed.channels.len(), 7);
        assert!(parsed.channels.iter().all(|c| c.channel_id() <= 7));
        let v = &parsed.channels[2]
            .av_channel
            .as_ref()
            .unwrap()
            .video_configs[0];
        assert_eq!(v.video_resolution(), 1);
        assert_eq!(v.dpi(), 160);
        let input = parsed.channels[0].input_channel.as_ref().unwrap();
        let touch = input.touch_screen.as_ref().unwrap();
        assert_eq!((touch.width(), touch.height()), (800, 480));
        let keycodes = &input.supported_keycodes;
        // The phone only acts on keys the car said it has.
        for action in [
            PhoneAction::NextTrack,
            PhoneAction::PreviousTrack,
            PhoneAction::VoiceAssistant,
            PhoneAction::AnswerCall,
            PhoneAction::EndCall,
            PhoneAction::PhoneHome,
            PhoneAction::OpenMaps,
            PhoneAction::OpenMedia,
            PhoneAction::PlayPause,
        ] {
            assert!(keycodes.contains(&phone_key(action)), "{action:?}");
        }
        assert_eq!(phone_key(PhoneAction::PlayPause), 85);
    }
}
