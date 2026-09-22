//! Carlinkit signed 16-bit LE PCM: u32 format, f32 volume, u32 stream,
//! then samples (or one control byte / four-byte volume ramp duration).
//! Wire reference: rhysmorgan134/node-carplay src/modules/messages/{readable,sendable}.ts.
use super::protocol::{ProtocolMessage, CMD_AUDIO, CMD_CONTROL, CMD_PHASE, CMD_UNPLUGGED};
use crate::config::AudioConfig;
use alsa::{
    pcm::{Access, Format, HwParams, PCM},
    Direction, ValueOr,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioFormat {
    rate: u32,
    channels: u32,
}
fn format(code: u32) -> Option<AudioFormat> {
    let (rate, channels) = match code {
        1 | 2 => (44100, 2),
        3 => (8000, 1),
        4 => (48000, 2),
        5 => (16000, 1),
        6 => (24000, 1),
        7 => (16000, 2),
        _ => return None,
    };
    Some(AudioFormat { rate, channels })
}
#[derive(Debug, PartialEq)]
enum Body<'a> {
    Control(u8),
    Volume(f32),
    Samples(&'a [u8]),
}
#[derive(Debug)]
struct Packet<'a> {
    code: u32,
    stream: u32,
    volume: f32,
    body: Body<'a>,
}
fn parse(data: &[u8]) -> Option<Packet<'_>> {
    if data.len() < 13 {
        return None;
    }
    let code = u32::from_le_bytes(data[..4].try_into().ok()?);
    let volume = f32::from_le_bytes(data[4..8].try_into().ok()?);
    let stream = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let body = match data.len() - 12 {
        1 => Body::Control(data[12]),
        4 => Body::Volume(f32::from_le_bytes(data[12..16].try_into().ok()?)),
        n if n % (format(code)?.channels as usize * 2) == 0 => Body::Samples(&data[12..]),
        _ => return None,
    };
    Some(Packet {
        code,
        stream,
        volume,
        body,
    })
}
fn microphone_message(code: u32, samples: &[i16]) -> ProtocolMessage {
    let mut data = Vec::with_capacity(12 + samples.len() * 2);
    data.extend(code.to_le_bytes());
    data.extend(0_f32.to_le_bytes());
    data.extend(3_u32.to_le_bytes());
    for sample in samples {
        data.extend(sample.to_le_bytes());
    }
    ProtocolMessage::new(CMD_AUDIO, data)
}
#[derive(Default)]
struct MicState {
    generation: u64,
    active: bool,
    code: u32,
}
struct MicPacket {
    generation: u64,
    message: ProtocolMessage,
    captured: Instant,
}
enum Playback {
    Data(Vec<u8>),
    Reset,
}

pub struct AudioPipeline {
    tx: SyncSender<Playback>,
    mic: Arc<Mutex<MicState>>,
    rx: Receiver<MicPacket>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}
impl AudioPipeline {
    pub fn start(config: &AudioConfig) -> Self {
        let (tx, playback_rx) = mpsc::sync_channel(64);
        let (mic_tx, rx) = mpsc::sync_channel(8);
        let mic = Arc::new(Mutex::new(MicState {
            code: 5,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        if config.enabled {
            let device = config.playback_device.clone();
            let end = stop.clone();
            workers.push(thread::spawn(move || {
                playback_worker(&device, playback_rx, &end)
            }));
            let device = config.capture_device.clone();
            let end = stop.clone();
            let state = mic.clone();
            workers.push(thread::spawn(move || {
                capture_worker(&device, mic_tx, state, &end)
            }));
            info!(playback = %config.playback_device, capture = %config.capture_device, "USB audio enabled");
        }
        Self {
            tx,
            mic,
            rx,
            stop,
            workers,
        }
    }
    pub fn active(&self) -> bool {
        self.mic.lock().unwrap().active
    }
    fn recording(&self, active: bool, code: Option<u32>) {
        let mut state = self.mic.lock().unwrap();
        let code = code
            .filter(|c| format(*c).is_some_and(|f| f.channels == 1))
            .unwrap_or(state.code);
        if state.active != active || state.code != code {
            state.generation += 1;
            state.active = active;
            state.code = code;
            info!(active, format = code, "phone microphone request");
        }
    }
    pub fn reset(&self) {
        self.recording(false, Some(5));
        // Reset must not be lost behind a full data queue. The worker never
        // blocks on ALSA, so this is bounded by its 5 ms service interval.
        let _ = self.tx.send(Playback::Reset);
    }
    pub fn receive(&self, message: &ProtocolMessage) {
        if message.encrypted || self.workers.is_empty() {
            return;
        }
        if message.kind == CMD_UNPLUGGED
            || (message.kind == CMD_PHASE && message.payload == 0_u32.to_le_bytes())
        {
            self.reset();
            return;
        }
        if message.kind == CMD_CONTROL {
            if message.payload == 1_u32.to_le_bytes() {
                self.recording(true, None);
            }
            if message.payload == 2_u32.to_le_bytes() {
                self.recording(false, None);
            }
        }
        if message.kind != CMD_AUDIO {
            return;
        }
        let Some(packet) = parse(&message.payload) else {
            warn!(
                bytes = message.payload.len(),
                "invalid/unsupported audio packet"
            );
            return;
        };
        if let Body::Control(command) = packet.body {
            info!(
                command,
                format = packet.code,
                stream = packet.stream,
                "phone audio control"
            );
            match command {
                3 => self.recording(self.active(), Some(packet.code)),
                4 | 8 => self.recording(true, Some(packet.code)),
                5 | 9 => self.recording(false, None),
                _ => (),
            }
        }
        if self
            .tx
            .try_send(Playback::Data(message.payload.clone()))
            .is_err()
        {
            debug!("audio queue full; dropping packet to bound latency");
        }
    }
    pub fn outgoing(&self) -> Vec<ProtocolMessage> {
        let state = self.mic.lock().unwrap();
        self.rx
            .try_iter()
            .take(8)
            .filter(|p| {
                state.active
                    && p.generation == state.generation
                    && p.captured.elapsed() < Duration::from_millis(200)
            })
            .map(|p| p.message)
            .collect()
    }
}
impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
fn open_pcm(device: &str, direction: Direction, fmt: AudioFormat) -> Result<PCM, alsa::Error> {
    let pcm = PCM::new(device, direction, true)?;
    {
        let hw = HwParams::any(&pcm)?;
        hw.set_access(Access::RWInterleaved)?;
        hw.set_format(Format::s16())?;
        hw.set_channels(fmt.channels)?;
        hw.set_rate(fmt.rate, ValueOr::Nearest)?;
        hw.set_buffer_time_near(100_000, ValueOr::Nearest)?;
        hw.set_period_time_near(20_000, ValueOr::Nearest)?;
        pcm.hw_params(&hw)?;
        let sw = pcm.sw_params_current()?;
        sw.set_start_threshold(if direction == Direction::Playback {
            (fmt.rate / 50) as i64
        } else {
            1
        })?;
        pcm.sw_params(&sw)?;
    }
    pcm.prepare()?;
    Ok(pcm)
}
struct Player {
    pcm: PCM,
    samples: VecDeque<i16>,
    fmt: AudioFormat,
    last: Instant,
}
fn playback_worker(device: &str, rx: Receiver<Playback>, stop: &AtomicBool) {
    let mut players: HashMap<(u32, u32), Player> = HashMap::new();
    let mut gains: HashMap<(u32, u32), f32> = HashMap::new();
    let mut retry = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        for event in rx.try_iter().take(64) {
            let data = match event {
                Playback::Reset => {
                    players.clear();
                    gains.clear();
                    continue;
                }
                Playback::Data(data) => data,
            };
            let Some(packet) = parse(&data) else {
                continue;
            };
            let key = (packet.code, packet.stream);
            match packet.body {
                Body::Control(2 | 5 | 7 | 9 | 11 | 13) => {
                    players.remove(&key);
                }
                Body::Volume(_) => {
                    if packet.volume.is_finite() {
                        gains.insert(key, packet.volume.clamp(0.0, 1.0));
                    }
                }
                Body::Samples(bytes) => {
                    let Some(fmt) = format(packet.code) else {
                        continue;
                    };
                    if !players.contains_key(&key) {
                        if Instant::now() < retry || players.len() >= 16 {
                            continue;
                        }
                        match open_pcm(device, Direction::Playback, fmt) {
                            Ok(pcm) => {
                                info!(format = packet.code, stream = packet.stream, rate = fmt.rate, channels = fmt.channels, %device, "audio playback opened");
                                players.insert(
                                    key,
                                    Player {
                                        pcm,
                                        samples: VecDeque::new(),
                                        fmt,
                                        last: Instant::now(),
                                    },
                                );
                            }
                            Err(err) => {
                                warn!(%err, %device, "audio playback unavailable; retrying in 2 seconds");
                                retry = Instant::now() + Duration::from_secs(2);
                                continue;
                            }
                        }
                    }
                    let player = players.get_mut(&key).unwrap();
                    player.last = Instant::now();
                    let gain = gains.get(&key).copied().unwrap_or(1.0);
                    if player.samples.len() + bytes.len() / 2
                        > (fmt.rate * fmt.channels / 2) as usize
                    {
                        player.samples.clear();
                        let _ = player.pcm.drop();
                        let _ = player.pcm.prepare();
                        warn!("discarded stale playback backlog");
                    }
                    player.samples.extend(
                        bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|s| (i16::from_le_bytes([s[0], s[1]]) as f32 * gain) as i16),
                    );
                }
                _ => (),
            }
        }
        players.retain(|_, p| {
            if !p.samples.is_empty() {
                let result = p
                    .pcm
                    .io_i16()
                    .and_then(|io| io.writei(p.samples.make_contiguous()));
                match result {
                    Ok(frames) => {
                        p.samples.drain(..frames * p.fmt.channels as usize);
                    }
                    Err(e) if e.errno() == 11 => (), // nonblocking EAGAIN
                    Err(e) => {
                        if p.pcm.try_recover(e, true).is_err() {
                            return false;
                        }
                    }
                }
            }
            p.last.elapsed() < Duration::from_secs(3) || !p.samples.is_empty()
        });
        thread::sleep(Duration::from_millis(5));
    }
}
fn capture_worker(
    device: &str,
    tx: SyncSender<MicPacket>,
    state: Arc<Mutex<MicState>>,
    stop: &AtomicBool,
) {
    let mut pcm: Option<PCM> = None;
    let mut generation = u64::MAX;
    let mut samples = Vec::new();
    let mut filled = 0;
    let mut retry = Instant::now();
    let mut packets = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        let (active, current, code) = {
            let s = state.lock().unwrap();
            (s.active, s.generation, s.code)
        };
        if generation != current {
            pcm = None;
            filled = 0;
            generation = current;
            retry = Instant::now();
            if !active {
                info!(packets, "microphone capture stopped");
            }
        }
        if !active {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if pcm.is_none() && Instant::now() >= retry {
            let fmt = format(code).unwrap();
            match open_pcm(device, Direction::Capture, fmt).and_then(|p| {
                p.start()?;
                Ok(p)
            }) {
                Ok(p) => {
                    samples.resize((fmt.rate / 50) as usize, 0);
                    pcm = Some(p);
                    info!(%device, rate = fmt.rate, "microphone capture started");
                }
                Err(err) => {
                    warn!(%err, %device, "microphone unavailable; retrying in 2 seconds");
                    retry = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(p) = pcm.as_ref() {
            match p.io_i16().and_then(|io| io.readi(&mut samples[filled..])) {
                Ok(count) => {
                    filled += count;
                    if filled == samples.len() {
                        let _ = tx.try_send(MicPacket {
                            generation,
                            message: microphone_message(code, &samples),
                            captured: Instant::now(),
                        });
                        packets += 1;
                        filled = 0;
                        if packets == 1 || packets.is_multiple_of(250) {
                            debug!(packets, "microphone packets captured");
                        }
                    }
                }
                Err(e) if e.errno() == 11 => (),
                Err(e) => {
                    filled = 0;
                    if p.try_recover(e, true).is_err() {
                        pcm = None;
                        retry = Instant::now() + Duration::from_secs(2);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the attached PCM2912A; briefly captures and discards microphone samples"]
    fn pcm2912a_full_duplex() {
        let config = AudioConfig::default();
        let stereo = open_pcm(
            &config.playback_device,
            Direction::Playback,
            format(4).unwrap(),
        )
        .unwrap();
        let voice = open_pcm(
            &config.playback_device,
            Direction::Playback,
            format(5).unwrap(),
        )
        .unwrap();
        assert!(stereo.io_i16().unwrap().writei(&[0; 1920]).unwrap() > 0);
        assert!(voice.io_i16().unwrap().writei(&[0; 640]).unwrap() > 0);
        let capture = open_pcm(
            &config.capture_device,
            Direction::Capture,
            format(5).unwrap(),
        )
        .unwrap();
        capture.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut samples = [0_i16; 320];
        loop {
            match capture.io_i16().unwrap().readi(&mut samples) {
                Ok(n) if n > 0 => break,
                Ok(_) => (),
                Err(e) if e.errno() == 11 => (),
                Err(e) => panic!("capture failed: {e}"),
            }
            assert!(Instant::now() < deadline, "no microphone samples received");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn validates_audio_and_distinguishes_commands_from_samples() {
        let pcm = microphone_message(5, &[i16::MIN, -1, 0, i16::MAX]);
        let p = parse(&pcm.payload).unwrap();
        assert_eq!((p.code, p.stream), (5, 3));
        assert_eq!(p.body, Body::Samples(&pcm.payload[12..]));
        let mut control = pcm.payload[..12].to_vec();
        control.push(8);
        assert_eq!(parse(&control).unwrap().body, Body::Control(8));
        assert!(parse(&control[..11]).is_none());
        control.extend([0, 0]);
        assert!(parse(&control).is_none());
    }
    #[test]
    fn microphone_header_and_little_endian_samples_match_protocol() {
        let m = microphone_message(5, &[0x1234, -2]);
        assert_eq!(m.kind, 7);
        assert_eq!(
            m.payload,
            [5, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0x34, 0x12, 0xfe, 0xff]
        );
        assert_eq!(
            format(4),
            Some(AudioFormat {
                rate: 48000,
                channels: 2
            })
        );
        assert_eq!(
            format(3),
            Some(AudioFormat {
                rate: 8000,
                channels: 1
            })
        );
        assert!(format(99).is_none());
    }
    #[test]
    fn stopped_or_old_microphone_packets_cannot_escape() {
        let config = AudioConfig {
            enabled: false,
            ..Default::default()
        };
        let mut audio = AudioPipeline::start(&config);
        let (tx, rx) = mpsc::sync_channel(8);
        audio.rx = rx;
        audio.recording(true, None);
        tx.send(MicPacket {
            generation: 1,
            message: microphone_message(5, &[0; 320]),
            captured: Instant::now(),
        })
        .unwrap();
        audio.recording(false, None);
        assert!(audio.outgoing().is_empty());
        audio.recording(true, None);
        tx.send(MicPacket {
            generation: 1,
            message: microphone_message(5, &[0; 320]),
            captured: Instant::now(),
        })
        .unwrap();
        assert!(audio.outgoing().is_empty());
    }
}
