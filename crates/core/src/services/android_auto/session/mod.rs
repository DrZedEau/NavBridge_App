//! One Android Auto session over any transport: the version exchange, TLS,
//! and the messages of each channel, grouped below by what they are about.

mod discovery;
mod input;
mod messages;
mod microphone;
mod protocol;
mod sensors;
#[cfg(test)]
mod tests;
mod video;

pub use input::phone_key;

pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/aa/mod.rs"));
}

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::AndroidAutoConfig;
use crate::services::android_auto::framing::{self, Assembler, Parser};
use crate::services::android_auto::tls::Tls;
use crate::services::audio::Stream;
use crate::services::gnss::Fix;
use generated::aa as p;
use navbridge_common::VideoCodec;
use protobuf::Message;
use protocol::msg;
use sensors::SpeedSource;

pub enum Action {
    Connected(String),
    /// The codec the phone's video comes in, before any of it.
    VideoCodec(VideoCodec),
    Video(Vec<u8>),
    Audio(Stream, Vec<u8>),
    StopAudio(Stream),
    Microphone(bool),
    Duck(bool),
    HostUi,
    /// The phone, at this Bluetooth address, asks whether the car has paired
    /// with it: `bluetooth_paired` answers.
    BluetoothPairing(String),
    /// The phone ended the session, or acknowledged our request to end it.
    Shutdown,
}

pub struct Session {
    config: AndroidAutoConfig,
    parser: Parser,
    assembler: Assembler,
    tls: Tls,
    pub outgoing: Vec<Vec<u8>>,
    pub opened: [bool; 9],
    sessions: [Option<i32>; 9],
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
    /// The I-Bus was being read when the session started. With it or a GNSS
    /// receiver, the phone is offered the car's speed.
    speedometer: bool,
    speed_sensor: bool,
    /// The car's Bluetooth address, offered for calls.
    bluetooth: Option<String>,
    /// The car's speedometer, in km/h, while the I-Bus reports it.
    speed: Option<u16>,
    /// The latest fix's speed in m/s, and when it came.
    gnss_speed: Option<(f64, Instant)>,
    /// Where the speed the phone last got came from, to log each change.
    speed_source: Option<SpeedSource>,
    last_fix: Option<(Fix, Instant)>,
    mic: bool,
    mic_window: u32,
    mic_unacked: u32,
    /// Microphone blocks that wait for room in the window, oldest first, with
    /// their timestamps; acknowledgements send them.
    mic_waiting: VecDeque<(u64, Vec<u8>)>,
    /// Blocks dropped since the microphone opened, for waiting too long.
    mic_dropped: u32,
    started: Instant,
    last_ping: Instant,
    pong: Option<(i64, Instant)>,
    clock: Instant,
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
            opened: [false; 9],
            sessions: [None; 9],
            discovered: false,
            night,
            night_sensor: false,
            parked,
            parking_brake_sensor: false,
            driving_status_sensor: false,
            gnss,
            location_sensor: false,
            speedometer: false,
            speed_sensor: false,
            speed: None,
            gnss_speed: None,
            speed_source: None,
            bluetooth: None,
            last_fix: None,
            mic: false,
            mic_window: 1,
            mic_unacked: 0,
            mic_waiting: VecDeque::new(),
            mic_dropped: 0,
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
