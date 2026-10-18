//! Transport-neutral signed 16-bit little-endian PCM workers. Both threads
//! sleep until there is work: playback in poll() on a doorbell and its ALSA
//! buffers, capture in blocking reads while the phone listens.
use crate::config::AudioConfig;
use alsa::{
    pcm::{Access, Format, HwParams, PCM},
    poll::pollfd,
    Direction, PollDescriptors, ValueOr,
};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{info, warn};
/// How long a stream may stay silent before its PCM is closed.
const IDLE_CLOSE: Duration = Duration::from_secs(3);
/// Wait before opening a PCM again after it failed.
const RETRY: Duration = Duration::from_secs(2);
const MIC: AudioFormat = AudioFormat {
    rate: 16000,
    channels: 1,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stream {
    Media,
    Speech,
    System,
}
impl Stream {
    pub fn format(self) -> AudioFormat {
        match self {
            Self::Media => AudioFormat {
                rate: 48000,
                channels: 2,
            },
            Self::Speech | Self::System => AudioFormat {
                rate: 16000,
                channels: 1,
            },
        }
    }
}
pub struct Capture {
    pub bytes: Vec<u8>,
    pub captured: Instant,
    pub generation: u64,
}
enum Playback {
    Data(u64, Stream, Vec<u8>),
    Stop(Stream),
}
#[derive(Default)]
struct State {
    generation: u64,
    mic_generation: u64,
    mic: bool,
    duck: bool,
}
pub struct AudioPipeline {
    tx: SyncSender<Playback>,
    rx: Receiver<Capture>,
    state: Arc<Mutex<State>>,
    /// Signalled when the microphone is switched on or off, and on stop.
    mic_changed: Arc<Condvar>,
    /// Wakes the playback thread for new samples, a reset or a stop.
    bell: UnixStream,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    enabled: bool,
}
/// Playback does not block, as its thread waits in poll(). Capture blocks, so
/// each read waits for the rest of its block.
pub fn open_pcm(device: &str, direction: Direction, fmt: AudioFormat) -> Result<PCM, alsa::Error> {
    let pcm = PCM::new(device, direction, direction == Direction::Playback)?;
    {
        let hw = HwParams::any(&pcm)?;
        hw.set_access(Access::RWInterleaved)?;
        hw.set_format(Format::s16())?;
        hw.set_channels(fmt.channels)?;
        hw.set_rate(fmt.rate, ValueOr::Nearest)?;
        hw.set_buffer_time_near(100000, ValueOr::Nearest)?;
        hw.set_period_time_near(20000, ValueOr::Nearest)?;
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
impl AudioPipeline {
    /// `wake` is signalled after each captured microphone block.
    pub fn start(config: &AudioConfig, wake: SyncSender<()>) -> io::Result<Self> {
        let (tx, rxplay) = mpsc::sync_channel(64);
        let (txcapture, rx) = mpsc::sync_channel(8);
        let (bell, doorbell) = UnixStream::pair()?;
        bell.set_nonblocking(true)?;
        doorbell.set_nonblocking(true)?;
        let state = Arc::new(Mutex::new(State::default()));
        let mic_changed = Arc::new(Condvar::new());
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        if config.enabled {
            let (device, s, end) = (config.playback_device.clone(), state.clone(), stop.clone());
            workers.push(
                thread::Builder::new()
                    .name("audio-playback".into())
                    .spawn(move || playback(&device, rxplay, doorbell, s, end))?,
            );
            let (device, s, end) = (config.capture_device.clone(), state.clone(), stop.clone());
            let changed = mic_changed.clone();
            workers.push(
                thread::Builder::new()
                    .name("audio-capture".into())
                    .spawn(move || capture(&device, txcapture, wake, s, changed, end))?,
            );
        }
        Ok(Self {
            tx,
            rx,
            state,
            mic_changed,
            bell,
            stop,
            workers,
            enabled: config.enabled,
        })
    }
    fn ring(&self) {
        // A full socket is already readable, so a failed write loses nothing.
        let _ = (&self.bell).write(&[1]);
    }
    pub fn receive_pcm(&self, stream: Stream, bytes: &[u8]) -> bool {
        let align = stream.format().channels as usize * 2;
        if !self.enabled
            || bytes.is_empty()
            || !bytes.len().is_multiple_of(align)
            || bytes.len() > 96000
        {
            return false;
        }
        let s = self.state.lock().unwrap();
        let queued = self
            .tx
            .try_send(Playback::Data(s.generation, stream, bytes.to_vec()))
            .is_ok();
        drop(s);
        if queued {
            self.ring();
        }
        queued
    }
    pub fn stop_stream(&self, stream: Stream) {
        if self.tx.send(Playback::Stop(stream)).is_ok() {
            self.ring();
        }
    }
    pub fn duck(&self, enabled: bool) {
        self.state.lock().unwrap().duck = enabled;
    }
    pub fn recording(&self, enabled: bool) -> Result<(), String> {
        if enabled && !self.enabled {
            return Err("audio disabled".into());
        }
        let mut s = self.state.lock().unwrap();
        s.mic_generation = s.mic_generation.wrapping_add(1);
        s.mic = enabled;
        self.mic_changed.notify_all();
        Ok(())
    }
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        s.generation = s.generation.wrapping_add(1);
        s.mic_generation = s.mic_generation.wrapping_add(1);
        s.mic = false;
        s.duck = false;
        self.mic_changed.notify_all();
        drop(s);
        // Retire queued samples now, not when their buffer next has room.
        self.ring();
    }
    pub fn outgoing(&self) -> Vec<Capture> {
        let s = self.state.lock().unwrap();
        self.rx
            .try_iter()
            .take(8)
            .filter(|p| {
                s.mic
                    && p.generation == s.mic_generation
                    && p.captured.elapsed() < Duration::from_millis(200)
            })
            .collect()
    }
}
impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Taking the lock orders the flag before the capture thread's next
        // check, so the notification cannot arrive before it waits.
        drop(self.state.lock());
        self.mic_changed.notify_all();
        self.ring();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
fn playback(
    device: &str,
    rx: Receiver<Playback>,
    doorbell: UnixStream,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
) {
    let mut players: HashMap<Stream, Player> = HashMap::new();
    let mut generation = 0;
    let mut retry = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let current = state.lock().unwrap().generation;
        if current != generation {
            players.clear();
            generation = current;
        }
        loop {
            let event = match rx.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                // The pipeline is gone.
                Err(TryRecvError::Disconnected) => return,
            };
            let (g, stream, bytes) = match event {
                Playback::Stop(stream) => {
                    players.remove(&stream);
                    continue;
                }
                Playback::Data(g, s, b) => (g, s, b),
            };
            if g != generation {
                continue;
            }
            let fmt = stream.format();
            if let std::collections::hash_map::Entry::Vacant(entry) = players.entry(stream) {
                if Instant::now() < retry {
                    continue;
                }
                match open_pcm(device, Direction::Playback, fmt) {
                    Ok(pcm) => {
                        info!(?stream, ?fmt, "PCM playback opened");
                        entry.insert(Player {
                            pcm,
                            samples: VecDeque::new(),
                            fmt,
                            last: Instant::now(),
                        });
                    }
                    Err(e) => {
                        warn!(%e,"PCM playback unavailable");
                        retry = Instant::now() + RETRY;
                        continue;
                    }
                }
            }
            let p = players.get_mut(&stream).unwrap();
            p.last = Instant::now();
            if p.samples.len() + bytes.len() / 2 > (fmt.rate * fmt.channels / 2) as usize {
                p.samples.clear();
                let _ = p.pcm.drop();
                let _ = p.pcm.prepare();
            }
            p.samples.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes([b[0], b[1]])),
            );
        }
        // Serialize reset with submitting samples so retired generations cannot play.
        let s = state.lock().unwrap();
        if s.generation != generation {
            drop(s);
            continue;
        }
        players.retain(|stream, p| {
            if !p.samples.is_empty() {
                let samples = p.samples.make_contiguous();
                let ducked: Vec<i16>;
                let data = if *stream == Stream::Media && s.duck {
                    ducked = samples.iter().map(|x| (*x as f32 * 0.25) as i16).collect();
                    &ducked[..]
                } else {
                    samples
                };
                match p.pcm.io_i16().and_then(|io| io.writei(data)) {
                    Ok(n) => {
                        p.samples.drain(..n * p.fmt.channels as usize);
                    }
                    Err(e) if e.errno() == 11 => (),
                    Err(e) => {
                        if p.pcm.try_recover(e, true).is_err() {
                            return false;
                        }
                    }
                }
            }
            p.last.elapsed() < IDLE_CLOSE || !p.samples.is_empty()
        });
        drop(s);
        wait_for_work(&doorbell, &players);
    }
}
/// Sleeps until the doorbell rings, a stream with queued samples has room in
/// its ALSA buffer, or an idle stream is due to close. Streams with nothing
/// queued are not watched, as their buffer would always report room.
fn wait_for_work(doorbell: &UnixStream, players: &HashMap<Stream, Player>) {
    let bell = pollfd {
        fd: doorbell.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut sources: Vec<&dyn PollDescriptors> = vec![&bell];
    let mut timeout: Option<Duration> = None;
    for p in players.values() {
        if p.samples.is_empty() {
            let close_in = IDLE_CLOSE.saturating_sub(p.last.elapsed());
            timeout = Some(timeout.map_or(close_in, |t| t.min(close_in)));
        } else {
            sources.push(&p.pcm);
        }
    }
    // Rounded up, so an idle stream is past its deadline on return.
    let millis = timeout.map_or(-1, |t| {
        t.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32
    });
    // poll_all also hands each PCM its results, which plugins such as dmix
    // need to clear their timer; otherwise poll() would return at once.
    if alsa::poll::poll_all(&sources, millis).is_err() {
        // Never spin, whatever went wrong.
        thread::sleep(Duration::from_millis(5));
    }
    // Drain the doorbell: the caller handles everything queued before it.
    let mut bytes = [0u8; 64];
    while matches!((&*doorbell).read(&mut bytes), Ok(n) if n > 0) {}
}
fn capture(
    device: &str,
    tx: SyncSender<Capture>,
    wake: SyncSender<()>,
    state: Arc<Mutex<State>>,
    mic_changed: Arc<Condvar>,
    stop: Arc<AtomicBool>,
) {
    // Whether the phone still listens to the microphone opened for `generation`.
    let listening = |s: &State, generation| {
        s.mic && s.mic_generation == generation && !stop.load(Ordering::Relaxed)
    };
    loop {
        // Sleep until the phone opens the microphone.
        let generation = {
            let mut s = state.lock().unwrap();
            while !s.mic && !stop.load(Ordering::Relaxed) {
                s = mic_changed.wait(s).unwrap();
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            s.mic_generation
        };
        let still_listening = || listening(&state.lock().unwrap(), generation);
        if let Err(e) = record(device, generation, still_listening, &tx, &wake) {
            warn!(%e,"PCM microphone unavailable");
            // Try again later, unless the phone stops listening first.
            let s = state.lock().unwrap();
            drop(mic_changed.wait_timeout_while(s, RETRY, |s| listening(s, generation)));
        }
    }
}
/// Sends the phone 20 ms microphone blocks while `listening` holds.
fn record(
    device: &str,
    generation: u64,
    listening: impl Fn() -> bool,
    tx: &SyncSender<Capture>,
    wake: &SyncSender<()>,
) -> Result<(), alsa::Error> {
    let pcm = open_pcm(device, Direction::Capture, MIC)?;
    pcm.start()?;
    info!(
        rate = MIC.rate,
        channels = MIC.channels,
        "PCM microphone opened"
    );
    let io = pcm.io_i16()?;
    let mut samples = [0i16; 320];
    let mut filled = 0;
    while listening() {
        // Blocks until the rest of the block has been captured.
        match io.readi(&mut samples[filled..]) {
            Ok(n) => {
                filled += n;
                if filled == samples.len() {
                    let bytes = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                    let queued = tx.try_send(Capture {
                        bytes,
                        captured: Instant::now(),
                        generation,
                    });
                    if queued.is_ok() {
                        let _ = wake.try_send(());
                    }
                    filled = 0;
                }
            }
            Err(e) => {
                filled = 0;
                pcm.try_recover(e, true)?;
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_pcm_and_old_capture_are_rejected() {
        let mut audio = AudioPipeline::start(
            &AudioConfig {
                enabled: false,
                ..Default::default()
            },
            mpsc::sync_channel(1).0,
        )
        .unwrap();
        assert!(!audio.receive_pcm(Stream::Media, &[1, 2, 3]));
        let (tx, rx) = mpsc::sync_channel(8);
        audio.rx = rx;
        audio.state.lock().unwrap().mic = true;
        tx.send(Capture {
            bytes: vec![0; 640],
            captured: Instant::now(),
            generation: 0,
        })
        .unwrap();
        audio.reset();
        assert!(audio.outgoing().is_empty());
        audio.state.lock().unwrap().mic = true;
        tx.send(Capture {
            bytes: vec![0; 640],
            captured: Instant::now(),
            generation: 0,
        })
        .unwrap();
        assert!(audio.outgoing().is_empty());
    }
    #[test]
    #[ignore = "requires PCM2912A"]
    fn pcm2912a_full_duplex() {
        let c = AudioConfig::default();
        let p = open_pcm(
            &c.playback_device,
            Direction::Playback,
            Stream::Media.format(),
        )
        .unwrap();
        assert!(p.io_i16().unwrap().writei(&[0; 1920]).unwrap() > 0);
        let mic = open_pcm(
            &c.capture_device,
            Direction::Capture,
            AudioFormat {
                rate: 16000,
                channels: 1,
            },
        )
        .unwrap();
        mic.start().unwrap();
        thread::sleep(Duration::from_millis(40));
        assert!(mic.io_i16().unwrap().readi(&mut [0; 320]).unwrap() > 0);
    }
}
