use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
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
    MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC,
};
use tracing::{debug, info, warn};

const RGBA_BYTES_PER_PIXEL: usize = 4;
#[cfg(test)]
const FRAME_BYTES: usize = UI_WIDTH as usize * UI_HEIGHT as usize * RGBA_BYTES_PER_PIXEL;
const VIDEO_QUEUE_DEPTH: usize = 12;
const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;
/// How often the frame counters are logged, at debug level, while video plays.
const COUNTER_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// H.264 decoders tried before FFmpeg's software one: the V4L2 hardware
/// decoder of the Pi 4 and CM4. The Pi 5 and CM5 have none, so they decode in
/// software.
const HARDWARE_DECODERS: &[&str] = &["h264_v4l2m2m"];
/// A hardware decoder that takes this many pictures without returning a frame
/// is given up for the software one.
const HARDWARE_STALL_PICTURES: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Accepted,
    Dropped,
    NeedKeyframe,
}

enum VideoCommand {
    Data { generation: u64, segment: Vec<u8> },
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

/// H.264 projection pipeline: a hardware decoder where the board has one,
/// FFmpeg's software decoder otherwise or once the hardware one fails.
///
/// Android Auto is negotiated at 800x480. Each reassembled video message is one
/// complete Annex-B access unit (or the SPS/PPS codec configuration), so it goes
/// to the normal software H.264 decoder as one packet and its frame is published
/// at once. Frames are scaled to the physical 400x240 NavBridge UI, then
/// atomically published as RGBA through tmpfs for the separate Slint process.
pub struct VideoPipeline {
    tx: Option<SyncSender<VideoCommand>>,
    worker: Option<JoinHandle<()>>,
    counters: VideoCounters,
    wait_for_idr: Arc<AtomicBool>,
    frame_path: PathBuf,
    generation: Arc<Mutex<u64>>,
    decoder: DecoderInfo,
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
        ffmpeg::init().map_err(|e| format!("initialize FFmpeg: {e}"))?;
        if ffmpeg::decoder::find(ffmpeg::codec::Id::H264).is_none() {
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
        let wait_for_idr = Arc::new(AtomicBool::new(false));

        let generation = Arc::new(Mutex::new(0));
        let worker_generation = Arc::clone(&generation);
        let worker_counters = counters.clone();
        let worker_frame_path = frame_path.clone();
        let worker = thread::Builder::new()
            .name("navbridge-h264".into())
            .spawn(move || {
                let mut engine = match DecoderEngine::new(
                    worker_frame_path,
                    Arc::clone(&worker_counters.decoded),
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
                let mut logged = Instant::now();
                while let Ok(VideoCommand::Data {
                    generation,
                    segment,
                }) = rx.recv()
                {
                    if generation != *engine.generation.lock().unwrap() {
                        continue;
                    }
                    if engine.active_generation != generation {
                        if let Err(err) = engine.reset() {
                            warn!(%err, "failed to reset the H.264 decoder");
                            break;
                        }
                        engine.active_generation = generation;
                    }
                    if let Err(err) = engine.push(&segment) {
                        worker_counters.dropped.fetch_add(1, Ordering::Relaxed);
                        let recovered = if engine.on_hardware {
                            engine.give_up_hardware(&err)
                        } else {
                            warn!(%err, "H.264 decode error; resetting decoder");
                            engine.reset()
                        };
                        if let Err(reset_err) = recovered {
                            warn!(%reset_err, "failed to reset the H.264 decoder");
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
            .map_err(|e| format!("start H.264 decoder thread: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = worker.join();
                return Err(err);
            }
            Err(_) => {
                return Err("H.264 decoder did not initialize in time".into());
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
            wait_for_idr,
            frame_path,
            generation,
            decoder,
        })
    }

    /// The H.264 decoder in use, such as "FFmpeg 7.1.5 h264 (software)".
    pub fn decoder(&self) -> String {
        self.decoder.name.lock().unwrap().clone()
    }

    /// Whether the decoder changed since this was last asked, as when a
    /// failing hardware decoder gave way to the software one.
    pub fn decoder_changed(&self) -> bool {
        self.decoder.changed.swap(false, Ordering::Relaxed)
    }

    /// Queue one Android Auto video message. If decoding falls behind,
    /// abandon dependent frames until an IDR arrives and ask the caller for a
    /// fresh keyframe once. Keeping this queue shallow is intentional for live
    /// projection latency.
    pub fn push_h264(&self, h264: &[u8]) -> PushResult {
        if h264.is_empty() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return PushResult::Dropped;
        }

        let is_idr = contains_idr(h264);
        // A new decoder starts from a keyframe: ask the phone for one.
        if self.decoder.keyframe_wanted.swap(false, Ordering::Relaxed) && !is_idr {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.wait_for_idr.store(true, Ordering::Relaxed);
            return PushResult::NeedKeyframe;
        }
        if self.wait_for_idr.load(Ordering::Relaxed) && !is_idr {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return PushResult::Dropped;
        }
        if is_idr {
            debug!("H.264 keyframe from the phone");
            if self.wait_for_idr.swap(false, Ordering::Relaxed) {
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
            segment: h264.to_vec(),
        }) {
            Ok(()) => PushResult::Accepted,
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                let was_waiting = self.wait_for_idr.swap(true, Ordering::Relaxed);
                if was_waiting {
                    PushResult::Dropped
                } else {
                    PushResult::NeedKeyframe
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                PushResult::Dropped
            }
        }
    }

    /// Flush decoder state between phone projection sessions.
    pub fn reset(&self) {
        self.wait_for_idr.store(false, Ordering::Relaxed);
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
    /// Hardware decoders still worth trying: none once one has failed, or is
    /// missing, or when core.toml asks for software.
    hardware: Vec<&'static str>,
    /// Whether `decoder` is one of them.
    on_hardware: bool,
    /// Pictures sent since the last frame came back.
    pending_pictures: u32,
    info: DecoderInfo,
    // SPS/PPS waiting for the next picture; FFmpeg rejects a packet without one.
    codec_config: Vec<u8>,
    /// The stream's last SPS/PPS, for a decoder that takes over mid-stream.
    last_config: Vec<u8>,
    size: FrameSize,
    scaler: Option<Scaler>,
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
        hardware: Vec<&'static str>,
        info: DecoderInfo,
        size: FrameSize,
    ) -> Result<Self, String> {
        let (decoder, on_hardware) = open_decoder(&hardware, &info)?;
        let mut engine = Self {
            decoder,
            hardware,
            on_hardware,
            pending_pictures: 0,
            info,
            codec_config: Vec::new(),
            last_config: Vec::new(),
            size,
            scaler: None,
            decoded_count,
            frame_path,
            sequence: 0,
            generation: Arc::new(Mutex::new(0)),
            active_generation: 0,
        };
        if !on_hardware {
            // Boards do not grow a decoder: look for one only once.
            engine.hardware.clear();
        }
        Ok(engine)
    }

    fn reset(&mut self) -> Result<(), String> {
        (self.decoder, self.on_hardware) = open_decoder(&self.hardware, &self.info)?;
        self.pending_pictures = 0;
        self.codec_config.clear();
        self.last_config.clear();
        self.scaler = None;
        let _ = fs::remove_file(&self.frame_path);
        debug!("H.264 decoder reset");
        Ok(())
    }

    /// Replaces a failing hardware decoder with the software one for the rest
    /// of the run. It starts from the stream's SPS/PPS and the next keyframe.
    fn give_up_hardware(&mut self, reason: &str) -> Result<(), String> {
        warn!(%reason, "hardware H.264 decoder failed; decoding in software from now on");
        self.hardware.clear();
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
        if !contains_picture(access_unit) {
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
            .map_err(|e| format!("send H.264 packet: {e}"))?;
        self.drain_frames()?;
        if self.on_hardware && self.pending_pictures >= HARDWARE_STALL_PICTURES {
            return Err(format!("no frame after {} pictures", self.pending_pictures));
        }
        Ok(())
    }

    fn drain_frames(&mut self) -> Result<(), String> {
        let mut decoded = ffmpeg::frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.width() == 0 || decoded.height() == 0 || decoded.format() == Pixel::None {
                continue;
            }
            self.pending_pictures = 0;

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
                    ScalingFlags::LANCZOS
                        | ScalingFlags::ACCURATE_RND
                        | ScalingFlags::FULL_CHR_H_INT,
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
                continue;
            }
            publish_frame(&self.frame_path, self.sequence, &rgba)
                .map_err(|e| format!("publish projection frame: {e}"))?;
            self.decoded_count.fetch_add(1, Ordering::Relaxed);
        }
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

/// Opens the first of the `hardware` decoders that works, else FFmpeg's
/// software one, and names it in `info`. Returns whether it is a hardware one.
fn open_decoder(
    hardware: &[&str],
    info: &DecoderInfo,
) -> Result<(ffmpeg::decoder::Video, bool), String> {
    let mut opened = None;
    for &name in hardware {
        let Some(codec) = ffmpeg::decoder::find_by_name(name) else {
            info!(decoder = name, "hardware H.264 decoder not in this FFmpeg");
            continue;
        };
        // A missing device is expected on boards without one: say so once,
        // without FFmpeg's own error lines.
        let level = ffmpeg::util::log::get_level().unwrap_or(ffmpeg::util::log::Level::Info);
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
        let result = open_codec(codec);
        ffmpeg::util::log::set_level(level);
        match result {
            Ok(decoder) => {
                opened = Some((decoder, codec, true));
                break;
            }
            Err(e) => info!(decoder = name, %e, "no hardware H.264 decoder on this board"),
        }
    }
    let (decoder, codec, on_hardware) = match opened {
        Some(opened) => opened,
        None => {
            let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
                .ok_or_else(|| "FFmpeg H.264 decoder not found".to_string())?;
            let decoder = open_codec(codec)
                .map_err(|e| format!("open FFmpeg H.264 software decoder: {e}"))?;
            (decoder, codec, false)
        }
    };
    // SAFETY: av_version_info returns a static string.
    let version = unsafe { CStr::from_ptr(ffmpeg::ffi::av_version_info()) }.to_string_lossy();
    let name = decoder_text(&version, codec.name(), on_hardware);
    let mut shown = info.name.lock().unwrap();
    if *shown != name {
        *shown = name;
        info.changed.store(true, Ordering::Relaxed);
    }
    Ok((decoder, on_hardware))
}

/// Opens a decoder for Android Auto's 800x480 stream. Hardware decoders size
/// their buffers from it; the software one takes the size from the stream.
fn open_codec(codec: ffmpeg::Codec) -> Result<ffmpeg::decoder::Video, ffmpeg::Error> {
    let mut context = ffmpeg::codec::context::Context::new_with_codec(codec);
    // SAFETY: the context is not open yet, so these fields are the caller's.
    unsafe {
        let context = context.as_mut_ptr();
        (*context).width = MEDIA_WIDTH as i32;
        (*context).height = MEDIA_HEIGHT as i32;
        (*context).coded_width = MEDIA_WIDTH as i32;
        (*context).coded_height = MEDIA_HEIGHT as i32;
    }
    context.decoder().video()
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

/// Annex-B NAL unit types in stream order; covers 3- and 4-byte start codes.
fn nal_unit_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(|w| w[3] & 0x1f)
}

/// Detect an Annex-B IDR NAL unit (type 5) without parsing the whole stream.
fn contains_idr(data: &[u8]) -> bool {
    nal_unit_types(data).any(|nal| nal == 5)
}

/// Coded slices are NAL types 1-5; the SPS/PPS codec configuration has none.
fn contains_picture(data: &[u8]) -> bool {
    nal_unit_types(data).any(|nal| (1..=5).contains(&nal))
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

    /// The fixture split into access units, one per Android Auto video message.
    fn access_units() -> Vec<&'static [u8]> {
        let stream: &'static [u8] = include_bytes!("../../tests/fixtures/parser-continuation.h264");
        let mut starts: Vec<usize> = stream
            .windows(5)
            .enumerate()
            .filter_map(|(i, w)| (w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 9).then_some(i))
            .collect();
        assert_eq!(starts.len(), 24);
        starts.push(stream.len());
        starts
            .windows(2)
            .map(|pair| &stream[pair[0]..pair[1]])
            .collect()
    }

    /// Split the first unit like Android Auto: SPS/PPS message, then the picture.
    fn codec_config_and_first_picture() -> (&'static [u8], &'static [u8]) {
        let first = access_units()[0];
        let slice = first
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && (1..=5).contains(&(w[3] & 0x1f)))
            .unwrap();
        first.split_at(slice)
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
                ScalingFlags::LANCZOS,
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
        DecoderEngine::new(
            path,
            count,
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
        assert_eq!(header(&path), (400, 240, 400 * 240 * 4));
        // Twice as large, mid-stream: Android Auto's own 800x480.
        assert!(size.set(800, 480));
        assert!(!size.set(800, 480), "unchanged");
        engine.push(units.next().unwrap()).unwrap();
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
            vec!["no_such_decoder"],
            info.clone(),
            FrameSize::default(),
        )
        .unwrap();
        assert!(!engine.on_hardware);
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
            vec!["hevc"],
            info.clone(),
            FrameSize::default(),
        )
        .unwrap();
        assert!(engine.on_hardware);
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
        assert!(!engine.on_hardware && engine.hardware.is_empty());
        assert!(info.name.lock().unwrap().ends_with(" h264 (software)"));
        assert!(info.changed.load(Ordering::Relaxed));
        assert!(info.keyframe_wanted.load(Ordering::Relaxed));
        // The stream's SPS/PPS carry over, so its next keyframe decodes alone.
        engine.push(keyframe).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);
        // A reset for the next phone does not bring the hardware one back.
        engine.reset().unwrap();
        assert!(!engine.on_hardware);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_new_decoder_asks_the_phone_for_a_keyframe() {
        let (tx, queue) = mpsc::sync_channel(4);
        let decoder = DecoderInfo::default();
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: None,
            counters: VideoCounters::default(),
            wait_for_idr: Arc::new(AtomicBool::new(false)),
            frame_path: std::env::temp_dir().join(format!(
                "navbridge-keyframe-wanted-test-{}.rgba",
                std::process::id()
            )),
            generation: Arc::new(Mutex::new(0)),
            decoder: decoder.clone(),
        };
        decoder.keyframe_wanted.store(true, Ordering::Relaxed);
        let picture = [0, 0, 0, 1, 0x41];
        assert_eq!(pipeline.push_h264(&picture), PushResult::NeedKeyframe);
        assert_eq!(pipeline.push_h264(&picture), PushResult::Dropped);
        assert_eq!(
            pipeline.push_h264(&[0, 0, 0, 1, 0x65]),
            PushResult::Accepted
        );
        assert_eq!(queue.try_iter().count(), 1);
    }

    #[test]
    fn every_access_unit_is_published_without_waiting_for_the_next() {
        let (config, first_picture) = codec_config_and_first_picture();
        let count = Arc::new(AtomicU64::new(0));
        let path = std::env::temp_dir().join(format!(
            "navbridge-access-unit-test-{}.rgba",
            std::process::id()
        ));
        let mut engine = software_engine(path.clone(), Arc::clone(&count));
        engine.push(config).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(!path.exists());
        let pictures = std::iter::once(first_picture).chain(access_units().into_iter().skip(1));
        for (index, unit) in pictures.enumerate() {
            engine.push(unit).unwrap();
            assert_eq!(
                count.load(Ordering::Relaxed),
                index as u64 + 1,
                "access unit {index} was held back"
            );
        }
        let frame = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(frame.len(), VIDEO_FRAME_HEADER_SIZE + FRAME_BYTES);
    }

    #[test]
    fn a_full_queue_asks_once_for_a_keyframe_then_waits_for_one() {
        let (tx, queue) = mpsc::sync_channel(1);
        let counters = VideoCounters::default();
        let pipeline = VideoPipeline {
            tx: Some(tx),
            worker: None,
            counters: counters.clone(),
            wait_for_idr: Arc::new(AtomicBool::new(false)),
            frame_path: std::env::temp_dir().join(format!(
                "navbridge-keyframe-test-{}.rgba",
                std::process::id()
            )),
            generation: Arc::new(Mutex::new(0)),
            decoder: DecoderInfo::default(),
        };
        let (keyframe, frame): (&[u8], &[u8]) = (&[0, 0, 0, 1, 0x65], &[0, 0, 0, 1, 0x41]);
        assert_eq!(pipeline.push_h264(frame), PushResult::Accepted);
        // The decoder is behind: the next frame is lost, so ask once.
        assert_eq!(pipeline.push_h264(frame), PushResult::NeedKeyframe);
        assert_eq!(pipeline.push_h264(frame), PushResult::Dropped);
        // Once the decoder catches up, only a keyframe can restart the picture.
        queue.recv().unwrap();
        assert_eq!(pipeline.push_h264(frame), PushResult::Dropped);
        assert_eq!(pipeline.push_h264(keyframe), PushResult::Accepted);
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
            assert_eq!(pipeline.push_h264(unit), PushResult::Accepted);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while counters.decoded() <= index as u64 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "unit {index} not counted"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(pipeline.push_h264(&[]), PushResult::Dropped);
        assert_eq!((counters.decoded(), counters.dropped()), (24, 1));
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
            wait_for_idr: Arc::new(AtomicBool::new(false)),
            frame_path: path.clone(),
            generation: generation.clone(),
            decoder: DecoderInfo::default(),
        };
        assert_eq!(
            pipeline.push_h264(&[0, 0, 0, 1, 0x65]),
            PushResult::Accepted
        );
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
        assert!(!path.exists());
        engine.reset().unwrap();
        engine.active_generation = 1;
        for unit in access_units() {
            engine.push(unit).unwrap();
        }
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
    fn finds_idr_annex_b_nal() {
        assert!(contains_idr(&[0, 0, 0, 1, 0x65, 1, 2, 3]));
        assert!(contains_idr(&[9, 9, 0, 0, 1, 0x25, 1]));
        assert!(!contains_idr(&[0, 0, 0, 1, 0x41, 1, 2]));
    }

    #[test]
    fn codec_configuration_is_not_a_picture() {
        assert!(!contains_picture(&[
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3
        ]));
        assert!(!contains_picture(&[
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x06, 5
        ]));
        assert!(contains_picture(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
        assert!(contains_picture(&[0, 0, 1, 0x41, 1]));
    }
}
