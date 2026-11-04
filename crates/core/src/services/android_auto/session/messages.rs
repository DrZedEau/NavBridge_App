//! The phone's messages on each channel, and the car's answers.

use super::discovery::discovery;
use super::generated::aa as p;
use super::protocol::{
    audio_stream, codec, msg, parse, sensor_type, BLUETOOTH, FOCUS_NATIVE, INPUT, KEYS, MEDIA, MIC,
    SENSOR, SPEECH, SYSTEM, VIDEO,
};
use super::sensors::FRESH_FIX;
use super::{Action, Session};

impl Session {
    pub(super) fn message(
        &mut self,
        channel: u8,
        id: u16,
        body: &[u8],
    ) -> Result<Vec<Action>, String> {
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
                        &discovery(
                            &self.config,
                            self.gnss,
                            self.speedometer,
                            self.bluetooth.as_deref(),
                        ),
                    )?;
                    self.discovered = true;
                    actions.push(Action::Connected(request.device_name().into()));
                }
                msg::CHANNEL_OPEN_REQUEST => {
                    let r = parse::<p::Open>(body)?;
                    let target = u8::try_from(r.channel_id())
                        .ok()
                        .filter(|&channel| self.discovered && self.offers(channel));
                    self.status(
                        target.unwrap_or(0),
                        msg::CHANNEL_OPEN_RESPONSE,
                        target.is_some(),
                    )?;
                    if let Some(channel) = target {
                        self.opened[channel as usize] = true;
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
        if !self.offers(channel) {
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
                self.mic_off(&mut actions);
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
                        sensor_type::SPEED if self.gnss || self.speedometer => {
                            self.speed_sensor = true;
                            self.status(SENSOR, msg::sensor::START_RESPONSE, true)?;
                            tracing::info!(
                                gnss = self.gnss,
                                ibus = self.speedometer,
                                "Android Auto uses the car's speed"
                            );
                            self.send_speed(None)?;
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
                    let supported = if channel == VIDEO {
                        let video = self.video_codec(r.media_codec_type());
                        if let Some(video) = video {
                            tracing::info!(codec = ?video, "Android Auto video codec");
                            actions.push(Action::VideoCodec(video));
                        } else {
                            tracing::warn!(
                                codec = r.media_codec_type(),
                                "Android Auto asks for a video codec it was not offered"
                            );
                        }
                        video.is_some()
                    } else {
                        r.media_codec_type() == codec::PCM
                    };
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
                        self.mic_off(&mut actions);
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
                    if m.open() {
                        // Without a window of its own, the phone gets the
                        // smallest.
                        self.mic_window = m.max_unacked().clamp(1, 10) as u32;
                        tracing::info!(
                            window = self.mic_window,
                            asked = m.max_unacked(),
                            "the phone opens the microphone"
                        );
                        self.mic = true;
                        self.mic_unacked = 0;
                        self.mic_waiting.clear();
                        self.mic_dropped = 0;
                        actions.push(Action::Microphone(true));
                    } else {
                        self.mic_off(&mut actions);
                    }
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
                        // More than was outstanding, as after a reopen,
                        // frees the whole window rather than none of it.
                        self.mic_unacked = self.mic_unacked.saturating_sub(n);
                        self.send_microphone()?;
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
            BLUETOOTH => match id {
                msg::bluetooth::PAIRING_REQUEST => {
                    let r = parse::<p::BluetoothPairingRequest>(body)?;
                    actions.push(Action::BluetoothPairing(r.phone_address().into()));
                }
                // Such as the code a phone sends for a car that compares it
                // itself: NavBridge shows it on screen instead.
                _ => tracing::debug!(id, "ignored AA Bluetooth message"),
            },
            _ => (),
        }
        Ok(actions)
    }

    /// Answers the phone's pairing request: whether the car has paired with
    /// it already; if not, the phone pairs over Bluetooth.
    pub fn bluetooth_paired(&mut self, paired: bool) -> Result<(), String> {
        let mut r = p::BluetoothPairingResponse::new();
        r.set_status(0);
        r.set_already_paired(paired);
        self.send(BLUETOOTH, msg::bluetooth::PAIRING_RESPONSE, &r)
    }
}
