//! Projection video: the phone's H.264 or H.265, decoded on the board's
//! hardware where it can be, else in software, scaled to the UI's size and
//! published for the UI.

mod decoders;
mod engine;
mod frames;
mod nal;
#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fs, iter};

use decoders::{decodes, HARDWARE_DECODERS};
use engine::DecoderEngine;
use ffmpeg_next as ffmpeg;
use nal::contains_keyframe;
use navbridge_common::{VideoCodec, MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH};
use tracing::{debug, error, info, warn};

const VIDEO_QUEUE_DEPTH: usize = 12;

/// How often the frame counters are logged, at debug level, while video plays.
const COUNTER_LOG_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Accepted,
    Dropped,
    NeedKeyframe,
    /// The decoder thread has stopped: no frame can be decoded any more.
    Stopped,
}

enum VideoCommand {
    Data {
        generation: u64,
        codec: VideoCodec,
        segment: Vec<u8>,
    },
}

/// Frames decoded and dropped since the core started. Clones share the counts,
/// so the decoder thread logs drops counted on the session thread too.
#[derive(Clone, Default)]
struct VideoCounters {
    decoded: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl VideoCounters {
    fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Relaxed)
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Projection pipeline for H.264 or H.265, as the phone chose: a hardware
/// decoder where the board has one, FFmpeg's software decoder otherwise or once
/// the hardware one fails.
///
/// Android Auto is negotiated at 800x480. Each reassembled video message is one
/// complete Annex-B access unit (or the codec configuration: H.264's SPS/PPS,
/// H.265's VPS/SPS/PPS), so it goes to the decoder as one packet and its frame
/// is published at once. Frames are scaled to the physical 400x240 NavBridge UI, then
/// atomically published as RGBA through tmpfs for the separate Slint process.
pub struct VideoPipeline {
    tx: Option<SyncSender<VideoCommand>>,
    worker: Option<JoinHandle<()>>,
    counters: VideoCounters,
    wait_for_keyframe: Arc<AtomicBool>,
    frame_path: PathBuf,
    generation: Arc<Mutex<u64>>,
    decoder: DecoderInfo,
    /// The codec the phone's video comes in.
    codec: Cell<VideoCodec>,
}

/// The size frames are published at, which the UI chooses: the panel's
/// 400x240, or Android Auto's own 800x480 for a UI drawn twice as large on a
/// PC. It can change while video plays.
#[derive(Clone)]
pub struct FrameSize(Arc<AtomicU64>);

impl Default for FrameSize {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(Self::pack(UI_WIDTH, UI_HEIGHT))))
    }
}

impl FrameSize {
    /// The sizes a UI can ask for; any other is refused.
    pub const CHOICES: [(u32, u32); 2] = [(UI_WIDTH, UI_HEIGHT), (MEDIA_WIDTH, MEDIA_HEIGHT)];

    fn pack(width: u32, height: u32) -> u64 {
        u64::from(width) << 32 | u64::from(height)
    }

    /// Returns whether the size changed.
    pub fn set(&self, width: u32, height: u32) -> bool {
        self.0.swap(Self::pack(width, height), Ordering::Relaxed) != Self::pack(width, height)
    }

    pub fn get(&self) -> (u32, u32) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }
}

/// The decoder in use, as the System page shows it, shared with the decoder
/// thread, which can change it.
#[derive(Clone, Default)]
struct DecoderInfo {
    name: Arc<Mutex<String>>,
    changed: Arc<AtomicBool>,
    /// The new decoder needs a keyframe to start from.
    keyframe_wanted: Arc<AtomicBool>,
}

impl VideoPipeline {
    /// `hardware`: try a hardware decoder first; core.toml can turn it off.
    /// Frames are published at `size`.
    pub fn start(
        frame_path: impl AsRef<Path>,
        hardware: bool,
        size: FrameSize,
    ) -> Result<Self, String> {
        let frame_path = frame_path.as_ref().to_path_buf();
        let counters = VideoCounters::default();
        let _ = fs::remove_file(&frame_path);

        // Fail early if the system FFmpeg libraries are unavailable or broken.
        // H.264 is the codec every phone can send.
        ffmpeg::init().map_err(|e| format!("initialize FFmpeg: {e}"))?;
        if !decodes(VideoCodec::H264) {
            return Err("FFmpeg software H.264 decoder is unavailable".into());
        }
        let candidates = if hardware {
            HARDWARE_DECODERS.to_vec()
        } else {
            Vec::new()
        };
        let decoder = DecoderInfo::default();
        let worker_decoder = decoder.clone();

        let (tx, rx) = mpsc::sync_channel::<VideoCommand>(VIDEO_QUEUE_DEPTH);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let wait_for_keyframe = Arc::new(AtomicBool::new(false));

        let generation = Arc::new(Mutex::new(0));
        let worker_generation = Arc::clone(&generation);
        let worker_counters = counters.clone();
        let worker_frame_path = frame_path.clone();
        let worker = thread::Builder::new()
            .name("navbridge-video".into())
            .spawn(move || {
                let mut engine = match DecoderEngine::new(
                    worker_frame_path,
                    Arc::clone(&worker_counters.decoded),
                    VideoCodec::H264,
                    candidates,
                    worker_decoder,
                    size,
                ) {
                    Ok(engine) => {
                        let _ = ready_tx.send(Ok(()));
                        engine
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };

                engine.generation = worker_generation;
                // A failed frame is dropped and the decoder starts afresh, in
                // software if the hardware one failed; false if even that fails.
                let recovered = |engine: &mut DecoderEngine, err: String| {
                    worker_counters.dropped.fetch_add(1, Ordering::Relaxed);
                    let reset = if engine.on_hardware.is_some() {
                        engine.give_up_hardware(&err)
                    } else {
                        warn!(%err, "video decode error; resetting decoder");
                        engine.reset()
                    };
                    reset
                        .inspect_err(|e| error!(%e, "failed to reset the video decoder; it stops"))
                        .is_ok()
                };
                let mut logged = Instant::now();
                'decode: while let Ok(next) = rx.recv() {
                    // Each picture builds on the one before, so all that wait
                    // are decoded; only the newest is scaled and shown.
                    let waiting = iter::once(next).chain(rx.try_iter().take(VIDEO_QUEUE_DEPTH));
                    for VideoCommand::Data {
                        generation,
                        codec,
                        segment,
                    } in waiting
                    {
                        if generation != *engine.generation.lock().unwrap() {
                            continue;
                        }
                        // A new phone, or the phone's video in another codec.
                        if engine.active_generation != generation || engine.codec != codec {
                            engine.codec = codec;
                            if let Err(err) = engine.reset() {
                                error!(%err, "failed to reset the video decoder; it stops");
                                break 'decode;
                            }
                            engine.active_generation = generation;
                        }
                        if let Err(err) = engine.push(&segment) {
                            if !recovered(&mut engine, err) {
                                break 'decode;
                            }
                        }
                    }
                    if let Err(err) = engine.show_newest() {
                        if !recovered(&mut engine, err) {
                            break;
                        }
                    }
                    // Only while video arrives, so a paused phone adds no lines.
                    if logged.elapsed() >= COUNTER_LOG_INTERVAL {
                        logged = Instant::now();
                        debug!(
                            decoded = worker_counters.decoded(),
                            dropped = worker_counters.dropped(),
                            "projection video frames since the core started"
                        );
                    }
                }
            })
            .map_err(|e| format!("start video decoder thread: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = worker.join();
                return Err(err);
            }
            Err(_) => {
                return Err("video decoder did not initialize in time".into());
            }
        }

        info!(
            output = %frame_path.display(),
            decoder = %decoder.name.lock().unwrap(),
            "projection decoder started"
        );

        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            counters,
            wait_for_keyframe,
            frame_path,
            generation,
            decoder,
            codec: Cell::new(VideoCodec::H264),
        })
    }

    /// The decoder thread has stopped, as when its decoder could not be
    /// reset: video stays black until a new pipeline starts.
    pub fn stopped(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// The video decoder in use, such as "FFmpeg 7.1.5 h264 (software)".
    pub fn decoder(&self) -> String {
        self.decoder.name.lock().unwrap().clone()
    }

    /// Whether the decoder changed since this was last asked, as when a
    /// failing hardware decoder gave way to the software one.
    pub fn decoder_changed(&self) -> bool {
        self.decoder.changed.swap(false, Ordering::Relaxed)
    }

    /// Whether FFmpeg can decode `codec`, in software at least.
    pub fn decodes(&self, codec: VideoCodec) -> bool {
        decodes(codec)
    }

    /// The codec the phone's video comes in, as its setup chose: the decoder
    /// follows from the next message on.
    pub fn set_codec(&self, codec: VideoCodec) {
        self.codec.set(codec);
    }

    /// Queue one Android Auto video message. If decoding falls behind,
    /// abandon dependent frames until a keyframe arrives and ask the caller
    /// for a fresh one once. Keeping this queue shallow is intentional for live
    /// projection latency.
    pub fn push(&self, video: &[u8]) -> PushResult {
        if video.is_empty() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return PushResult::Dropped;
        }

        let codec = self.codec.get();
        let is_keyframe = contains_keyframe(codec, video);
        // A new decoder starts from a keyframe: ask the phone for one.
        if self.decoder.keyframe_wanted.swap(false, Ordering::Relaxed) && !is_keyframe {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.wait_for_keyframe.store(true, Ordering::Relaxed);
            return PushResult::NeedKeyframe;
        }
        if self.wait_for_keyframe.load(Ordering::Relaxed) && !is_keyframe {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return PushResult::Dropped;
        }
        if is_keyframe {
            debug!("keyframe from the phone");
            if self.wait_for_keyframe.swap(false, Ordering::Relaxed) {
                info!("keyframe received; video decoding resumes");
            }
        }

        let Some(tx) = self.tx.as_ref() else {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return PushResult::Dropped;
        };

        let generation = *self.generation.lock().unwrap();
        match tx.try_send(VideoCommand::Data {
            generation,
            codec,
            segment: video.to_vec(),
        }) {
            Ok(()) => PushResult::Accepted,
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                let was_waiting = self.wait_for_keyframe.swap(true, Ordering::Relaxed);
                if was_waiting {
                    PushResult::Dropped
                } else {
                    PushResult::NeedKeyframe
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                PushResult::Stopped
            }
        }
    }

    /// Flush decoder state between phone projection sessions.
    pub fn reset(&self) {
        self.wait_for_keyframe.store(false, Ordering::Relaxed);
        self.decoder.keyframe_wanted.store(false, Ordering::Relaxed);
        // Also serialize with publication: after reset returns, an old worker
        // cannot put an obsolete frame back, even if it was already decoding.
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        let _ = fs::remove_file(&self.frame_path);
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.frame_path);
    }
}
