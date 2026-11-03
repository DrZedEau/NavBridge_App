use std::cell::Cell;
use std::ffi::CStr;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::iter;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags as ScalingFlags};
use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;
use navbridge_common::{
    VideoCodec, MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE,
    VIDEO_FRAME_MAGIC,
};
use tracing::{debug, error, info, warn};

const RGBA_BYTES_PER_PIXEL: usize = 4;
#[cfg(test)]
const FRAME_BYTES: usize = UI_WIDTH as usize * UI_HEIGHT as usize * RGBA_BYTES_PER_PIXEL;
const VIDEO_QUEUE_DEPTH: usize = 12;
const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;
/// How often the frame counters are logged, at debug level, while video plays.
const COUNTER_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// Hardware decoders tried before FFmpeg's software one, by codec: the V4L2
/// H.264 decoder of the Pi 4 and CM4, and the HEVC decoder (rpi-hevc-dec) of
/// the Pi 4 and 5 and their CMs, which Raspberry Pi FFmpeg drives as a DRM
/// hwaccel. The Pi 5 and CM5 have none for H.264, so they decode it in software.
const HARDWARE_DECODERS: &[(VideoCodec, Hardware)] = &[
    (VideoCodec::H264, Hardware::Decoder("h264_v4l2m2m")),
    (VideoCodec::H265, Hardware::Drm),
];
/// A hardware decoder that takes this many pictures without returning a frame
/// is given up for the software one.
const HARDWARE_STALL_PICTURES: u32 = 30;

/// A hardware decoder: an FFmpeg decoder of its own, or FFmpeg's decoder for
/// the codec with a DRM device, through which a hwaccel decodes and hands
/// frames over as DRM buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hardware {
    Decoder(&'static str),
    Drm,
}

impl fmt::Display for Hardware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decoder(name) => f.write_str(name),
            Self::Drm => f.write_str("DRM hwaccel"),
        }
    }
}

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

struct DecoderEngine {
    decoder: ffmpeg::decoder::Video,
    /// What `decoder` decodes.
    codec: VideoCodec,
    /// Hardware decoders still worth trying, by codec: none once one has
    /// failed, or is missing, or when core.toml asks for software.
    hardware: Vec<(VideoCodec, Hardware)>,
    /// Which of them `decoder` is, if one.
    on_hardware: Option<Hardware>,
    /// Pictures sent since the last frame came back.
    pending_pictures: u32,
    info: DecoderInfo,
    // The codec configuration (SPS/PPS, and H.265's VPS) waiting for the next
    // picture; FFmpeg rejects a packet without one.
    codec_config: Vec<u8>,
    /// The stream's last configuration, for a decoder that takes over mid-stream.
    last_config: Vec<u8>,
    size: FrameSize,
    scaler: Option<Scaler>,
    /// The last picture decoded, until it is shown or a newer one replaces it.
    newest: Option<ffmpeg::frame::Video>,
    decoded_count: Arc<AtomicU64>,
    frame_path: PathBuf,
    sequence: u64,
    generation: Arc<Mutex<u64>>,
    active_generation: u64,
}

impl DecoderEngine {
    fn new(
        frame_path: PathBuf,
        decoded_count: Arc<AtomicU64>,
        codec: VideoCodec,
        mut hardware: Vec<(VideoCodec, Hardware)>,
        info: DecoderInfo,
        size: FrameSize,
    ) -> Result<Self, String> {
        let (decoder, on_hardware) = open_decoder(codec, &mut hardware, &info)?;
        Ok(Self {
            decoder,
            codec,
            hardware,
            on_hardware,
            pending_pictures: 0,
            info,
            codec_config: Vec::new(),
            last_config: Vec::new(),
            size,
            scaler: None,
            newest: None,
            decoded_count,
            frame_path,
            sequence: 0,
            generation: Arc::new(Mutex::new(0)),
            active_generation: 0,
        })
    }

    /// Opens a decoder afresh for `codec`.
    fn reset(&mut self) -> Result<(), String> {
        (self.decoder, self.on_hardware) =
            open_decoder(self.codec, &mut self.hardware, &self.info)?;
        self.pending_pictures = 0;
        self.codec_config.clear();
        self.last_config.clear();
        self.scaler = None;
        self.newest = None;
        let _ = fs::remove_file(&self.frame_path);
        debug!(codec = ?self.codec, "video decoder reset");
        Ok(())
    }

    /// Replaces a failing hardware decoder with the software one for the rest
    /// of the run, for this codec. It starts from the stream's configuration
    /// and the next keyframe.
    fn give_up_hardware(&mut self, reason: &str) -> Result<(), String> {
        warn!(
            %reason,
            codec = ?self.codec,
            "hardware video decoder failed; decoding in software from now on"
        );
        let codec = self.codec;
        self.hardware.retain(|&(decodes, _)| decodes != codec);
        let config = std::mem::take(&mut self.last_config);
        self.reset()?;
        self.codec_config.clone_from(&config);
        self.last_config = config;
        self.info.keyframe_wanted.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn push(&mut self, access_unit: &[u8]) -> Result<(), String> {
        // Android Auto sends one complete access unit per message, so decode it
        // now. An FFmpeg parser would hold each frame until the next one began.
        if !contains_picture(self.codec, access_unit) {
            if self.codec_config.len() + access_unit.len() > MAX_CODEC_CONFIG_BYTES {
                self.codec_config.clear();
            }
            self.codec_config.extend_from_slice(access_unit);
            return Ok(());
        }
        let packet = if self.codec_config.is_empty() {
            ffmpeg::Packet::copy(access_unit)
        } else {
            let mut data = std::mem::take(&mut self.codec_config);
            self.last_config.clone_from(&data);
            data.extend_from_slice(access_unit);
            ffmpeg::Packet::copy(&data)
        };
        self.pending_pictures += 1;
        self.decoder
            .send_packet(&packet)
            .map_err(|e| format!("send video packet: {e}"))?;
        self.drain_frames();
        if self.on_hardware.is_some() && self.pending_pictures >= HARDWARE_STALL_PICTURES {
            return Err(format!("no frame after {} pictures", self.pending_pictures));
        }
        Ok(())
    }

    /// FFmpeg decodes in software when the DRM hwaccel does not start, as on
    /// a board without its hardware: the decoder carries on, shown as software.
    fn no_drm_hwaccel(&mut self) {
        info!(codec = ?self.codec, "no hardware video decoder on this board");
        let codec = self.codec;
        self.hardware
            .retain(|&entry| entry != (codec, Hardware::Drm));
        self.on_hardware = None;
        let name = self.decoder.codec().map(|c| c.name().to_owned());
        show_decoder(&self.info, name.as_deref().unwrap_or_default(), false);
    }

    /// Takes the pictures the decoder has finished, keeping the newest.
    fn drain_frames(&mut self) {
        let mut decoded = ffmpeg::frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.width() == 0 || decoded.height() == 0 || decoded.format() == Pixel::None {
                continue;
            }
            if self.on_hardware == Some(Hardware::Drm) && decoded.format() != Pixel::DRM_PRIME {
                self.no_drm_hwaccel();
            }
            self.pending_pictures = 0;
            self.decoded_count.fetch_add(1, Ordering::Relaxed);
            self.newest = Some(std::mem::replace(
                &mut decoded,
                ffmpeg::frame::Video::empty(),
            ));
        }
    }

    /// Scales the newest decoded picture to the UI's size and publishes it,
    /// if there is one not shown yet.
    fn show_newest(&mut self) -> Result<(), String> {
        let Some(mut decoded) = self.newest.take() else {
            return Ok(());
        };
        if decoded.format() == Pixel::DRM_PRIME {
            decoded = from_hardware(&decoded)?;
        }
        normalize_full_range(&mut decoded);
        let (width, height) = self.size.get();
        let spec = (
            decoded.format(),
            decoded.width(),
            decoded.height(),
            decoded.color_range(),
            decoded.color_space(),
            width,
            height,
        );
        let rebuild = self.scaler.as_ref().map(|s| s.spec != spec).unwrap_or(true);
        if rebuild {
            let mut context = ScalingContext::get(
                decoded.format(),
                decoded.width(),
                decoded.height(),
                Pixel::RGBA,
                width,
                height,
                // 2:1 averaging for the panel, a plain conversion at 800x480.
                // Full chroma keeps a dark line off saturated colour edges.
                ScalingFlags::AREA | ScalingFlags::FULL_CHR_H_INT,
            )
            .map_err(|e| format!("create software scaler: {e}"))?;
            configure_colors(&mut context, decoded.color_range(), decoded.color_space())?;
            self.scaler = Some(Scaler { spec, context });
            info!(
                source_width = decoded.width(),
                source_height = decoded.height(),
                source_format = ?decoded.format(),
                source_fps = ?self.decoder.frame_rate(),
                source_range = ?decoded.color_range(),
                source_colorspace = ?decoded.color_space(),
                output = %format!("{width}x{height} RGBA"),
                "projection video format established"
            );
        }

        let mut rgba = ffmpeg::frame::Video::new(Pixel::RGBA, width, height);
        self.scaler
            .as_mut()
            .expect("scaler was initialized")
            .context
            .run(&decoded, &mut rgba)
            .map_err(|e| format!("scale projection frame: {e}"))?;

        self.sequence = self.sequence.wrapping_add(1);
        let generation = self.generation.lock().unwrap();
        if *generation != self.active_generation {
            return Ok(());
        }
        publish_frame(&self.frame_path, self.sequence, &rgba)
            .map_err(|e| format!("publish projection frame: {e}"))?;
        Ok(())
    }
}

// YUVJ has the same plane layout as YUV; retain its explicit full range.
fn normalize_full_range(frame: &mut ffmpeg::frame::Video) {
    let pixel = match frame.format() {
        Pixel::YUVJ420P => Pixel::YUV420P,
        Pixel::YUVJ422P => Pixel::YUV422P,
        Pixel::YUVJ444P => Pixel::YUV444P,
        Pixel::YUVJ440P => Pixel::YUV440P,
        Pixel::YUVJ411P => Pixel::YUV411P,
        _ => return,
    };
    frame.set_format(pixel);
    frame.set_color_range(Range::JPEG);
}

fn configure_colors(
    context: &mut ScalingContext,
    range: Range,
    space: Space,
) -> Result<(), String> {
    let matrix = match space {
        Space::BT709 => ffmpeg::ffi::SWS_CS_ITU709,
        Space::FCC => ffmpeg::ffi::SWS_CS_FCC,
        Space::SMPTE240M => ffmpeg::ffi::SWS_CS_SMPTE240M,
        Space::BT2020NCL | Space::BT2020CL => ffmpeg::ffi::SWS_CS_BT2020,
        _ => ffmpeg::ffi::SWS_CS_ITU601,
    };
    let result = unsafe {
        let coefficients = ffmpeg::ffi::sws_getCoefficients(matrix as i32);
        ffmpeg::ffi::sws_setColorspaceDetails(
            context.as_mut_ptr(),
            coefficients,
            i32::from(range == Range::JPEG),
            coefficients,
            1,
            0,
            1 << 16,
            1 << 16,
        )
    };
    if result < 0 {
        return Err(format!("configure scaler color range: {result}"));
    }
    Ok(())
}

struct Scaler {
    /// The source's format, size and colours, and the output size.
    spec: (Pixel, u32, u32, Range, Space, u32, u32),
    context: ScalingContext,
}

/// FFmpeg's version without its package suffix ("7.1.5-0+deb13u1+rpt2" is
/// 7.1.5), the decoder's name, and whether it runs on hardware.
fn decoder_text(version: &str, name: &str, hardware: bool) -> String {
    let version = version.split('-').next().unwrap_or_default();
    let kind = if hardware { "hardware" } else { "software" };
    format!("FFmpeg {version} {name} ({kind})")
}

/// FFmpeg's name for a codec.
fn codec_id(codec: VideoCodec) -> ffmpeg::codec::Id {
    match codec {
        VideoCodec::H264 => ffmpeg::codec::Id::H264,
        VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
    }
}

/// Whether FFmpeg has a software decoder for `codec`.
fn decodes(codec: VideoCodec) -> bool {
    ffmpeg::decoder::find(codec_id(codec)).is_some()
}

/// Opens the first of the `hardware` decoders for `codec` that works, else
/// FFmpeg's software one, and names it in `info`. Returns which hardware one
/// it is, if one. Boards do not grow a decoder: those that do not open are
/// taken off `hardware`, so they are tried only once.
fn open_decoder(
    codec: VideoCodec,
    hardware: &mut Vec<(VideoCodec, Hardware)>,
    info: &DecoderInfo,
) -> Result<(ffmpeg::decoder::Video, Option<Hardware>), String> {
    let mut opened = None;
    let mut missing = Vec::new();
    for &(decodes, kind) in hardware.iter() {
        if decodes != codec {
            continue;
        }
        let found = match kind {
            Hardware::Decoder(name) => ffmpeg::decoder::find_by_name(name),
            Hardware::Drm => ffmpeg::decoder::find(codec_id(codec)).filter(|&c| has_drm_hwaccel(c)),
        };
        let Some(found) = found else {
            info!(decoder = %kind, ?codec, "hardware video decoder not in this FFmpeg");
            missing.push((decodes, kind));
            continue;
        };
        // A missing device is expected on boards without one: say so once,
        // without FFmpeg's own error lines.
        let level = ffmpeg::util::log::get_level().unwrap_or(ffmpeg::util::log::Level::Info);
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
        let result = open_codec(found, kind == Hardware::Drm);
        ffmpeg::util::log::set_level(level);
        match result {
            Ok(decoder) => {
                opened = Some((decoder, found, Some(kind)));
                break;
            }
            Err(e) => {
                info!(decoder = %kind, ?codec, %e, "no hardware video decoder on this board");
                missing.push((decodes, kind));
            }
        }
    }
    hardware.retain(|entry| !missing.contains(entry));
    let (decoder, codec, on_hardware) = match opened {
        Some(opened) => opened,
        None => {
            let name = codec.name();
            let found = ffmpeg::decoder::find(codec_id(codec))
                .ok_or_else(|| format!("FFmpeg {name} decoder not found"))?;
            let decoder = open_codec(found, false)
                .map_err(|e| format!("open FFmpeg {name} software decoder: {e}"))?;
            (decoder, found, None)
        }
    };
    show_decoder(info, codec.name(), on_hardware.is_some());
    Ok((decoder, on_hardware))
}

/// Names the decoder in use for the System page.
fn show_decoder(info: &DecoderInfo, name: &str, hardware: bool) {
    // SAFETY: av_version_info returns a static string.
    let version = unsafe { CStr::from_ptr(ffmpeg::ffi::av_version_info()) }.to_string_lossy();
    let name = decoder_text(&version, name, hardware);
    let mut shown = info.name.lock().unwrap();
    if *shown != name {
        *shown = name;
        info.changed.store(true, Ordering::Relaxed);
    }
}

/// Whether FFmpeg's `codec` decoder has a hwaccel that decodes through a DRM
/// device, as Raspberry Pi FFmpeg's HEVC one does.
fn has_drm_hwaccel(codec: ffmpeg::Codec) -> bool {
    use ffmpeg::ffi::{avcodec_get_hw_config, AVHWDeviceType, AVPixelFormat};
    // SAFETY: the configurations are static, and the list ends with null.
    (0..)
        .map_while(|i| unsafe { avcodec_get_hw_config(codec.as_ptr(), i).as_ref() })
        .any(|config| {
            config.device_type == AVHWDeviceType::AV_HWDEVICE_TYPE_DRM
                && config.pix_fmt == AVPixelFormat::AV_PIX_FMT_DRM_PRIME
        })
}

/// Opens a decoder for Android Auto's 800x480 stream. Hardware decoders size
/// their buffers from it; the software one takes the size from the stream.
/// With `drm`, the decoder gets a DRM device, and its hwaccel starts when the
/// stream's configuration arrives; without the hardware, FFmpeg decodes in
/// software instead.
fn open_codec(codec: ffmpeg::Codec, drm: bool) -> Result<ffmpeg::decoder::Video, ffmpeg::Error> {
    let mut context = ffmpeg::codec::context::Context::new_with_codec(codec);
    // SAFETY: the context is not open yet, so these fields are the caller's;
    // it frees the device with itself.
    unsafe {
        let context = context.as_mut_ptr();
        (*context).width = MEDIA_WIDTH as i32;
        (*context).height = MEDIA_HEIGHT as i32;
        (*context).coded_width = MEDIA_WIDTH as i32;
        (*context).coded_height = MEDIA_HEIGHT as i32;
        if drm {
            let created = ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut (*context).hw_device_ctx,
                ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if created < 0 {
                return Err(ffmpeg::Error::from(created));
            }
        }
    }
    context.decoder().video()
}

/// Copies a frame the hardware decoded, held in a DRM buffer, into memory for
/// the scaler.
fn from_hardware(frame: &ffmpeg::frame::Video) -> Result<ffmpeg::frame::Video, String> {
    let mut copy = ffmpeg::frame::Video::empty();
    // SAFETY: both frames are valid, and the copy gets buffers of its own.
    let result = unsafe {
        match ffmpeg::ffi::av_hwframe_transfer_data(copy.as_mut_ptr(), frame.as_ptr(), 0) {
            0 => ffmpeg::ffi::av_frame_copy_props(copy.as_mut_ptr(), frame.as_ptr()),
            error => error,
        }
    };
    if result < 0 {
        return Err(format!(
            "copy the hardware frame: {}",
            ffmpeg::Error::from(result)
        ));
    }
    Ok(copy)
}

#[cfg(test)]
fn tightly_pack_rgba(frame: &ffmpeg::frame::Video) -> Result<Vec<u8>, String> {
    let row_bytes = UI_WIDTH as usize * RGBA_BYTES_PER_PIXEL;
    let stride = frame.stride(0);
    if stride < row_bytes {
        return Err(format!(
            "RGBA stride {stride} is smaller than row {row_bytes}"
        ));
    }

    let data = frame.data(0);
    let required = stride.saturating_mul(UI_HEIGHT as usize);
    if data.len() < required {
        return Err(format!(
            "RGBA frame buffer too short: {} < {}",
            data.len(),
            required
        ));
    }

    let mut out = vec![0_u8; FRAME_BYTES];
    for y in 0..UI_HEIGHT as usize {
        let src = y * stride;
        let dst = y * row_bytes;
        out[dst..dst + row_bytes].copy_from_slice(&data[src..src + row_bytes]);
    }
    Ok(out)
}

fn publish_frame(
    path: &Path,
    sequence: u64,
    frame: &ffmpeg::frame::Video,
) -> Result<(), std::io::Error> {
    let (width, height) = (frame.width(), frame.height());
    let row_bytes = width as usize * RGBA_BYTES_PER_PIXEL;
    let stride = frame.stride(0);
    let data = frame.data(0);
    if stride < row_bytes || data.len() < stride.saturating_mul(height as usize) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid RGBA stride or buffer",
        ));
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = File::create(&tmp)?;
    file.write_all(&VIDEO_FRAME_MAGIC)?;
    file.write_all(&width.to_le_bytes())?;
    file.write_all(&height.to_le_bytes())?;
    file.write_all(&sequence.to_le_bytes())?;
    debug_assert_eq!(VIDEO_FRAME_HEADER_SIZE, 20);
    if stride == row_bytes {
        file.write_all(&data[..row_bytes * height as usize])?;
    } else {
        for row in data.chunks_exact(stride).take(height as usize) {
            file.write_all(&row[..row_bytes])?;
        }
    }
    file.flush()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}

/// A NAL unit's type, from the first byte of its header.
fn nal_type(codec: VideoCodec, header: u8) -> u8 {
    match codec {
        VideoCodec::H264 => header & 0x1f,
        VideoCodec::H265 => (header >> 1) & 0x3f,
    }
}

/// Annex-B NAL unit types in stream order; covers 3- and 4-byte start codes.
fn nal_unit_types(codec: VideoCodec, data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(move |w| nal_type(codec, w[3]))
}

/// Detect a picture a decoder can start from, without parsing the whole
/// stream: an H.264 IDR (type 5), or an H.265 random access one (16-21).
fn contains_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    let keyframe = match codec {
        VideoCodec::H264 => 5..=5,
        VideoCodec::H265 => 16..=21,
    };
    nal_unit_types(codec, data).any(|nal| keyframe.contains(&nal))
}

/// Coded slices are H.264's NAL types 1-5 and H.265's 0-31; the codec
/// configuration has none.
fn contains_picture(codec: VideoCodec, data: &[u8]) -> bool {
    let picture = match codec {
        VideoCodec::H264 => 1..=5,
        VideoCodec::H265 => 0..=31,
    };
    nal_unit_types(codec, data).any(|nal| picture.contains(&nal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decoder_reads_as_ffmpeg_version_name_and_kind() {
        assert_eq!(
            decoder_text("7.1.5-0+deb13u1+rpt2", "h264", false),
            "FFmpeg 7.1.5 h264 (software)"
        );
        assert_eq!(
            decoder_text("8.1", "h264_v4l2m2m", true),
            "FFmpeg 8.1 h264_v4l2m2m (hardware)"
        );
    }

    /// A codec's fixture split into access units, one per Android Auto video
    /// message, at its access unit delimiters (H.264 type 9, H.265 type 35).
    fn units_of(codec: VideoCodec) -> Vec<&'static [u8]> {
        let (stream, delimiter): (&'static [u8], u8) = match codec {
            VideoCodec::H264 => (
                include_bytes!("../../tests/fixtures/parser-continuation.h264"),
                9,
            ),
            VideoCodec::H265 => (
                include_bytes!("../../tests/fixtures/parser-continuation.h265"),
                35,
            ),
        };
        let mut starts: Vec<usize> = stream
            .windows(5)
            .enumerate()
            .filter_map(|(i, w)| {
                (w[..4] == [0, 0, 0, 1] && nal_type(codec, w[4]) == delimiter).then_some(i)
            })
            .collect();
        assert_eq!(starts.len(), 24);
        starts.push(stream.len());
        starts
            .windows(2)
            .map(|pair| &stream[pair[0]..pair[1]])
            .collect()
    }

    /// The H.264 fixture's access units.
    fn access_units() -> Vec<&'static [u8]> {
        units_of(VideoCodec::H264)
    }

    /// Split the first unit like Android Auto: the codec configuration
    /// message, then the picture.
    fn config_and_first_picture_of(codec: VideoCodec) -> (&'static [u8], &'static [u8]) {
        let first = units_of(codec)[0];
        let slice = first
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && contains_picture(codec, w))
            .unwrap();
        first.split_at(slice)
    }

    fn codec_config_and_first_picture() -> (&'static [u8], &'static [u8]) {
        config_and_first_picture_of(VideoCodec::H264)
    }

    #[test]
    fn full_and_limited_range_fill_the_output_with_correct_black_and_white() {
        ffmpeg::init().unwrap();
        for (pixel, black, white) in [(Pixel::YUVJ420P, 0, 255), (Pixel::YUV420P, 16, 235)] {
            let mut input = ffmpeg::frame::Video::new(pixel, 800, 480);
            input.set_color_range(Range::MPEG);
            input.set_color_space(Space::BT709);
            let stride = input.stride(0);
            for y in 0..480 {
                for x in 0..800 {
                    input.data_mut(0)[y * stride + x] =
                        if (x < 400) == (y < 240) { black } else { white };
                }
            }
            input.data_mut(1).fill(128);
            input.data_mut(2).fill(128);
            normalize_full_range(&mut input);
            assert_eq!(input.format(), Pixel::YUV420P);
            assert_eq!(
                input.color_range(),
                if pixel == Pixel::YUVJ420P {
                    Range::JPEG
                } else {
                    Range::MPEG
                }
            );
            let mut scaler = ScalingContext::get(
                input.format(),
                800,
                480,
                Pixel::RGBA,
                UI_WIDTH,
                UI_HEIGHT,
                ScalingFlags::AREA | ScalingFlags::FULL_CHR_H_INT,
            )
            .unwrap();
            configure_colors(&mut scaler, input.color_range(), input.color_space()).unwrap();
            let mut output = ffmpeg::frame::Video::new(Pixel::RGBA, UI_WIDTH, UI_HEIGHT);
            scaler.run(&input, &mut output).unwrap();
            let pixels = tightly_pack_rgba(&output).unwrap();
            // A wrong range is off by 16 or more. FFmpeg 8 rounds full-range
            // white to [254, 252, 254], so 3 is allowed.
            for (x, y, expected) in [
                (20, 20, 0_i16),
                (380, 20, 255),
                (20, 220, 255),
                (380, 220, 0),
            ] {
                let offset = (y * UI_WIDTH as usize + x) * 4;
                for channel in &pixels[offset..offset + 3] {
                    assert!(
                        (*channel as i16 - expected).abs() <= 3,
                        "{pixel:?} at ({x}, {y}): {:?}, expected {expected}",
                        &pixels[offset..offset + 3]
                    );
                }
                assert_eq!(pixels[offset + 3], 255);
            }
        }
    }

    fn software_engine(path: PathBuf, count: Arc<AtomicU64>) -> DecoderEngine {
        software_engine_for(VideoCodec::H264, path, count)
    }

    fn software_engine_for(
        codec: VideoCodec,
        path: PathBuf,
        count: Arc<AtomicU64>,
    ) -> DecoderEngine {
        DecoderEngine::new(
            path,
            count,
            codec,
            Vec::new(),
            DecoderInfo::default(),
            FrameSize::default(),
        )
        .unwrap()
    }

    #[test]
    fn frames_follow_the_size_the_ui_asks_for() {
        let path = std::env::temp_dir().join(format!(
            "navbridge-frame-size-test-{}.rgba",
            std::process::id()
        ));
        let size = FrameSize::default();
        assert_eq!(size.get(), (400, 240));
        let mut engine = DecoderEngine::new(
            path.clone(),
            Arc::new(AtomicU64::new(0)),
            VideoCodec::H264,
            Vec::new(),
            DecoderInfo::default(),
            size.clone(),
        )
        .unwrap();
        let header = |path: &Path| {
            let frame = fs::read(path).unwrap();
            let number = |at: usize| u32::from_le_bytes(frame[at..at + 4].try_into().unwrap());
            (number(4), number(8), frame.len() - VIDEO_FRAME_HEADER_SIZE)
        };
        let mut units = access_units().into_iter();
        engine.push(units.next().unwrap()).unwrap();
        engine.show_newest().unwrap();
        assert_eq!(header(&path), (400, 240, 400 * 240 * 4));
        // Twice as large, mid-stream: Android Auto's own 800x480.
        assert!(size.set(800, 480));
        assert!(!size.set(800, 480), "unchanged");
        engine.push(units.next().unwrap()).unwrap();
        engine.show_newest().unwrap();
        assert_eq!(header(&path), (800, 480, 800 * 480 * 4));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_board_without_a_hardware_decoder_decodes_in_software() {
        let path = std::env::temp_dir().join(format!(
            "navbridge-no-hardware-test-{}.rgba",
            std::process::id()
        ));
        let count = Arc::new(AtomicU64::new(0));
        let info = DecoderInfo::default();
        let mut engine = DecoderEngine::new(
            path.clone(),
            Arc::clone(&count),
            VideoCodec::H264,
            vec![(VideoCodec::H264, Hardware::Decoder("no_such_decoder"))],
            info.clone(),
            FrameSize::default(),
        )
        .unwrap();
        assert!(engine.on_hardware.is_none());
        assert!(engine.hardware.is_empty(), "looked for again at each reset");
        assert!(info.name.lock().unwrap().ends_with(" h264 (software)"));
        assert!(info.changed.load(Ordering::Relaxed));
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
        let _ = fs::remove_file(&path);
        assert_eq!(count.load(Ordering::Relaxed), 24);
    }

    #[test]
    fn a_failing_hardware_decoder_gives_way_to_software_mid_stream() {
        let path = std::env::temp_dir().join(format!(
            "navbridge-failing-hardware-test-{}.rgba",
            std::process::id()
        ));
        let count = Arc::new(AtomicU64::new(0));
        let info = DecoderInfo::default();
        // FFmpeg's HEVC decoder stands in for a hardware one that opens but
        // cannot decode the stream.
        let mut engine = DecoderEngine::new(
            path.clone(),
            Arc::clone(&count),
            VideoCodec::H264,
            vec![(VideoCodec::H264, Hardware::Decoder("hevc"))],
            info.clone(),
            FrameSize::default(),
        )
        .unwrap();
        assert_eq!(engine.on_hardware, Some(Hardware::Decoder("hevc")));
        assert!(info.name.lock().unwrap().ends_with(" hevc (hardware)"));
        // As Android Auto sends it: the SPS/PPS alone, then the pictures. The
        // stand-in fails outright, or returns no frame for
        // HARDWARE_STALL_PICTURES.
        let (config, keyframe) = codec_config_and_first_picture();
        let stream = || {
            [config, keyframe]
                .into_iter()
                .chain(access_units().into_iter().skip(1))
        };
        let failure = (0..3)
            .flat_map(|_| stream())
            .find_map(|unit| engine.push(unit).err())
            .expect("the stand-in decoded H.264");
        assert_eq!(count.load(Ordering::Relaxed), 0);
        info.changed.store(false, Ordering::Relaxed);
        engine.give_up_hardware(&failure).unwrap();
        assert!(engine.on_hardware.is_none() && engine.hardware.is_empty());
        assert!(info.name.lock().unwrap().ends_with(" h264 (software)"));
        assert!(info.changed.load(Ordering::Relaxed));
        assert!(info.keyframe_wanted.load(Ordering::Relaxed));
        // The stream's SPS/PPS carry over, so its next keyframe decodes alone.
        engine.push(keyframe).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);
        // A reset for the next phone does not bring the hardware one back.
        engine.reset().unwrap();
        assert!(engine.on_hardware.is_none());
        let _ = fs::remove_file(&path);
    }

    /// Whether this board has the Raspberry Pi HEVC decoder.
    fn board_has_hevc_decoder() -> bool {
        fs::read_dir("/sys/class/video4linux")
            .into_iter()
            .flatten()
            .flatten()
            .any(|device| {
                fs::read_to_string(device.path().join("name"))
                    .is_ok_and(|name| name.trim() == "rpi-hevc-dec")
            })
    }

    #[test]
    fn h265_decodes_in_hardware_where_the_board_and_ffmpeg_can() {
        let hevc = ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC).unwrap();
        let hardware = has_drm_hwaccel(hevc) && board_has_hevc_decoder();
        // The last frame, from the DRM hwaccel where there is one, and from
        // the software decoder.
        let last_frame = |candidates: Vec<(VideoCodec, Hardware)>| {
            let path = std::env::temp_dir().join(format!(
                "navbridge-drm-test-{}-{}.rgba",
                candidates.len(),
                std::process::id()
            ));
            let count = Arc::new(AtomicU64::new(0));
            let info = DecoderInfo::default();
            let mut engine = DecoderEngine::new(
                path.clone(),
                Arc::clone(&count),
                VideoCodec::H265,
                candidates,
                info.clone(),
                FrameSize::default(),
            )
            .unwrap();
            // Each access unit's frame comes at once, from the hardware too.
            for (index, unit) in units_of(VideoCodec::H265).into_iter().enumerate() {
                engine.push(unit).unwrap();
                assert_eq!(count.load(Ordering::Relaxed), index as u64 + 1);
                engine.show_newest().unwrap();
            }
            let frame = fs::read(&path).unwrap();
            let _ = fs::remove_file(&path);
            let name = info.name.lock().unwrap().clone();
            (frame, engine.on_hardware, name)
        };
        let (drm, on_hardware, name) = last_frame(vec![(VideoCodec::H265, Hardware::Drm)]);
        assert_eq!(on_hardware.is_some(), hardware);
        let kind = if hardware { "hardware" } else { "software" };
        assert!(name.ends_with(&format!(" hevc ({kind})")), "{name}");
        // H.265 decodes to the same picture everywhere.
        let (software, ..) = last_frame(Vec::new());
        assert_eq!(drm.len(), software.len());
        let differs = drm
            .iter()
            .zip(&software)
            .filter(|(a, b)| a.abs_diff(**b) > 2);
        assert_eq!(differs.count(), 0);
    }

    #[test]
    fn a_drm_hwaccel_that_does_not_start_leaves_software_decoding() {
        let path =
            std::env::temp_dir().join(format!("navbridge-no-drm-test-{}.rgba", std::process::id()));
        let count = Arc::new(AtomicU64::new(0));
        let mut engine = software_engine(path.clone(), Arc::clone(&count));
        // As FFmpeg's HEVC decoder with a DRM device on a board without the
        // hardware: its frames come from software.
        engine.on_hardware = Some(Hardware::Drm);
        engine.hardware = vec![(VideoCodec::H264, Hardware::Drm)];
        show_decoder(&engine.info, "h264", true);
        engine.info.changed.store(false, Ordering::Relaxed);
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
        let _ = fs::remove_file(&path);
        assert_eq!(count.load(Ordering::Relaxed), 24);
        assert!(engine.on_hardware.is_none());
        assert!(engine.hardware.is_empty(), "not tried again");
        assert!(engine
            .info
            .name
            .lock()
            .unwrap()
            .ends_with(" h264 (software)"));
        assert!(engine.info.changed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_new_decoder_asks_the_phone_for_a_keyframe() {
        let (tx, queue) = mpsc::sync_channel(4);
        let decoder = DecoderInfo::default();
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: None,
            counters: VideoCounters::default(),
            wait_for_keyframe: Arc::new(AtomicBool::new(false)),
            frame_path: std::env::temp_dir().join(format!(
                "navbridge-keyframe-wanted-test-{}.rgba",
                std::process::id()
            )),
            generation: Arc::new(Mutex::new(0)),
            decoder: decoder.clone(),
            codec: Cell::new(VideoCodec::H264),
        };
        decoder.keyframe_wanted.store(true, Ordering::Relaxed);
        let picture = [0, 0, 0, 1, 0x41];
        assert_eq!(pipeline.push(&picture), PushResult::NeedKeyframe);
        assert_eq!(pipeline.push(&picture), PushResult::Dropped);
        assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Accepted);
        assert_eq!(queue.try_iter().count(), 1);
    }

    #[test]
    fn every_access_unit_is_published_without_waiting_for_the_next() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let (config, first_picture) = config_and_first_picture_of(codec);
            let count = Arc::new(AtomicU64::new(0));
            let path = std::env::temp_dir().join(format!(
                "navbridge-access-unit-test-{codec:?}-{}.rgba",
                std::process::id()
            ));
            let mut engine = software_engine_for(codec, path.clone(), Arc::clone(&count));
            engine.push(config).unwrap();
            assert_eq!(count.load(Ordering::Relaxed), 0);
            assert!(!path.exists());
            let pictures =
                std::iter::once(first_picture).chain(units_of(codec).into_iter().skip(1));
            for (index, unit) in pictures.enumerate() {
                engine.push(unit).unwrap();
                assert_eq!(
                    count.load(Ordering::Relaxed),
                    index as u64 + 1,
                    "{codec:?} access unit {index} was held back"
                );
                engine.show_newest().unwrap();
                assert_eq!(sequence(&path), index as u64 + 1);
            }
            let frame = fs::read(&path).unwrap();
            let _ = fs::remove_file(&path);
            assert_eq!(frame.len(), VIDEO_FRAME_HEADER_SIZE + FRAME_BYTES);
        }
    }

    /// The sequence number in a published frame's header.
    fn sequence(path: &Path) -> u64 {
        let frame = fs::read(path).unwrap();
        u64::from_le_bytes(frame[12..20].try_into().unwrap())
    }

    #[test]
    fn of_the_pictures_that_queued_up_only_the_newest_is_shown() {
        let count = Arc::new(AtomicU64::new(0));
        let path =
            std::env::temp_dir().join(format!("navbridge-newest-test-{}.rgba", std::process::id()));
        let mut engine = software_engine(path.clone(), Arc::clone(&count));
        // All of them decoded, as each builds on the one before...
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
        assert_eq!(count.load(Ordering::Relaxed), 24);
        assert!(!path.exists(), "nothing shown yet");
        // ...and one scaled and published.
        engine.show_newest().unwrap();
        assert_eq!(sequence(&path), 1);
        engine.show_newest().unwrap();
        assert_eq!(sequence(&path), 1, "shown once");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_full_queue_asks_once_for_a_keyframe_then_waits_for_one() {
        let (tx, queue) = mpsc::sync_channel(1);
        let counters = VideoCounters::default();
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: None,
            counters: counters.clone(),
            wait_for_keyframe: Arc::new(AtomicBool::new(false)),
            frame_path: std::env::temp_dir().join(format!(
                "navbridge-keyframe-test-{}.rgba",
                std::process::id()
            )),
            generation: Arc::new(Mutex::new(0)),
            decoder: DecoderInfo::default(),
            codec: Cell::new(VideoCodec::H264),
        };
        let (keyframe, frame): (&[u8], &[u8]) = (&[0, 0, 0, 1, 0x65], &[0, 0, 0, 1, 0x41]);
        assert_eq!(pipeline.push(frame), PushResult::Accepted);
        // The decoder is behind: the next frame is lost, so ask once.
        assert_eq!(pipeline.push(frame), PushResult::NeedKeyframe);
        assert_eq!(pipeline.push(frame), PushResult::Dropped);
        // Once the decoder catches up, only a keyframe can restart the picture.
        queue.recv().unwrap();
        assert_eq!(pipeline.push(frame), PushResult::Dropped);
        assert_eq!(pipeline.push(keyframe), PushResult::Accepted);
        assert_eq!(counters.dropped(), 3);
    }

    #[test]
    fn the_logged_counters_see_every_frame() {
        let path = std::env::temp_dir().join(format!(
            "navbridge-counters-test-{}.rgba",
            std::process::id()
        ));
        // Software, as this test expects each frame before the next unit.
        let pipeline = VideoPipeline::start(&path, false, FrameSize::default()).unwrap();
        let counters = pipeline.counters.clone();
        // One unit at a time, so the shallow queue never overflows.
        for (index, unit) in access_units().into_iter().enumerate() {
            assert_eq!(pipeline.push(unit), PushResult::Accepted);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while counters.decoded() <= index as u64 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "unit {index} not counted"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(pipeline.push(&[]), PushResult::Dropped);
        assert_eq!((counters.decoded(), counters.dropped()), (24, 1));
        assert!(!pipeline.stopped());
    }

    #[test]
    fn the_decoder_follows_the_codec_the_phone_chose() {
        let path =
            std::env::temp_dir().join(format!("navbridge-codec-test-{}.rgba", std::process::id()));
        let pipeline = VideoPipeline::start(&path, false, FrameSize::default()).unwrap();
        let counters = pipeline.counters.clone();
        assert!(pipeline.decoder().ends_with(" h264 (software)"));
        pipeline.decoder_changed();
        let mut decoded = 0;
        for codec in [VideoCodec::H265, VideoCodec::H264] {
            pipeline.set_codec(codec);
            for unit in units_of(codec) {
                assert_eq!(pipeline.push(unit), PushResult::Accepted);
                decoded += 1;
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while counters.decoded() < decoded {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{codec:?} unit {decoded} not decoded"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
            let name = if codec == VideoCodec::H265 {
                "hevc"
            } else {
                "h264"
            };
            assert!(pipeline.decoder().ends_with(&format!(" {name} (software)")));
            assert!(pipeline.decoder_changed());
        }
        assert_eq!(counters.dropped(), 0);
    }

    #[test]
    fn a_decoder_thread_that_gave_up_is_reported() {
        let path = std::env::temp_dir().join(format!(
            "navbridge-stopped-test-{}.rgba",
            std::process::id()
        ));
        // A decoder thread that has stopped, as after a failed reset.
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || drop(rx));
        while !worker.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: Some(worker),
            counters: VideoCounters::default(),
            wait_for_keyframe: Arc::new(AtomicBool::new(false)),
            frame_path: path,
            generation: Arc::new(Mutex::new(0)),
            decoder: DecoderInfo::default(),
            codec: Cell::new(VideoCodec::H264),
        };
        assert!(pipeline.stopped());
        assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Stopped);
    }

    #[test]
    fn reset_invalidates_full_queue_and_in_flight_decoder() {
        let path =
            std::env::temp_dir().join(format!("navbridge-reset-test-{}.rgba", std::process::id()));
        let (tx, rx) = mpsc::sync_channel(1);
        let generation = Arc::new(Mutex::new(0));
        let count = Arc::new(AtomicU64::new(0));
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: None,
            counters: VideoCounters::default(),
            wait_for_keyframe: Arc::new(AtomicBool::new(false)),
            frame_path: path.clone(),
            generation: generation.clone(),
            decoder: DecoderInfo::default(),
            codec: Cell::new(VideoCodec::H264),
        };
        assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Accepted);
        fs::write(&path, b"old frame").unwrap();
        pipeline.reset();
        assert!(!path.exists());
        let VideoCommand::Data {
            generation: queued, ..
        } = rx.recv().unwrap();
        assert_ne!(queued, *generation.lock().unwrap());
        let mut engine = software_engine(path.clone(), count);
        engine.generation = generation;
        // Old work already inside FFmpeg must also be unable to publish.
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
        engine.show_newest().unwrap();
        assert!(!path.exists());
        engine.reset().unwrap();
        engine.active_generation = 1;
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
        engine.show_newest().unwrap();
        assert!(path.exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn publication_writes_rgba_rows_without_stride_padding() {
        // An odd width guarantees stride padding after each row.
        let (width, height) = (UI_WIDTH as usize + 1, UI_HEIGHT as usize);
        let mut frame = ffmpeg::frame::Video::new(Pixel::RGBA, width as u32, height as u32);
        let stride = frame.stride(0);
        assert!(stride > width * 4);
        frame.data_mut(0).fill(0xee);
        for y in 0..height {
            frame.data_mut(0)[y * stride..y * stride + width * 4].fill(y as u8);
        }
        let path =
            std::env::temp_dir().join(format!("navbridge-stride-test-{}.rgba", std::process::id()));
        publish_frame(&path, 17, &frame).unwrap();
        let published = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(&published[4..8], &(width as u32).to_le_bytes());
        assert_eq!(&published[8..12], &(height as u32).to_le_bytes());
        assert_eq!(&published[12..20], &17u64.to_le_bytes());
        let pixels = &published[VIDEO_FRAME_HEADER_SIZE..];
        assert_eq!(pixels.len(), width * height * 4);
        for (y, row) in pixels.chunks_exact(width * 4).enumerate() {
            assert!(row.iter().all(|&b| b == y as u8), "row {y} has padding");
        }
    }

    #[test]
    fn finds_keyframes_annex_b_nal() {
        use VideoCodec::{H264, H265};
        // H.264: an IDR (5), not a plain slice (1).
        assert!(contains_keyframe(H264, &[0, 0, 0, 1, 0x65, 1, 2, 3]));
        assert!(contains_keyframe(H264, &[9, 9, 0, 0, 1, 0x25, 1]));
        assert!(!contains_keyframe(H264, &[0, 0, 0, 1, 0x41, 1, 2]));
        // H.265: IDR (19, 20) and CRA (21), not a trailing picture (1). The
        // H.264 IDR's byte is an H.265 slice.
        assert!(contains_keyframe(H265, &[0, 0, 0, 1, 0x26, 1, 2]));
        assert!(contains_keyframe(H265, &[0, 0, 1, 0x28, 1]));
        assert!(contains_keyframe(H265, &[0, 0, 1, 0x2a, 1]));
        assert!(!contains_keyframe(H265, &[0, 0, 0, 1, 0x02, 1, 2]));
        assert!(!contains_keyframe(H265, &[0, 0, 0, 1, 0x65, 1, 2]));
    }

    #[test]
    fn codec_configuration_is_not_a_picture() {
        use VideoCodec::{H264, H265};
        assert!(!contains_picture(
            H264,
            &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]
        ));
        assert!(!contains_picture(
            H264,
            &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x06, 5]
        ));
        assert!(contains_picture(
            H264,
            &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]
        ));
        assert!(contains_picture(H264, &[0, 0, 1, 0x41, 1]));
        // H.265: VPS, SPS and PPS (32-34), a delimiter (35) and SEI (39).
        assert!(!contains_picture(
            H265,
            &[0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 1, 0, 0, 0, 1, 0x44, 1]
        ));
        assert!(!contains_picture(
            H265,
            &[0, 0, 0, 1, 0x46, 1, 0, 0, 1, 0x4e, 1]
        ));
        assert!(contains_picture(
            H265,
            &[0, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x28, 1]
        ));
        assert!(contains_picture(H265, &[0, 0, 1, 0x02, 1]));
    }
}
