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
use navbridge_common::{UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC};
use tracing::{debug, info, warn};

const RGBA_BYTES_PER_PIXEL: usize = 4;
const FRAME_BYTES: usize = UI_WIDTH as usize * UI_HEIGHT as usize * RGBA_BYTES_PER_PIXEL;
const VIDEO_QUEUE_DEPTH: usize = 12;
const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;
/// How often the frame counters are logged, at debug level, while video plays.
const COUNTER_LOG_INTERVAL: Duration = Duration::from_secs(5);

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

/// Software-only H.264 projection pipeline.
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
}

impl VideoPipeline {
    pub fn start(frame_path: impl AsRef<Path>) -> Result<Self, String> {
        let frame_path = frame_path.as_ref().to_path_buf();
        let counters = VideoCounters::default();
        let _ = fs::remove_file(&frame_path);

        // Fail early if the system FFmpeg libraries are unavailable or broken.
        ffmpeg::init().map_err(|e| format!("initialize FFmpeg: {e}"))?;
        if ffmpeg::decoder::find(ffmpeg::codec::Id::H264).is_none() {
            return Err("FFmpeg software H.264 decoder is unavailable".into());
        }

        let (tx, rx) = mpsc::sync_channel::<VideoCommand>(VIDEO_QUEUE_DEPTH);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let wait_for_idr = Arc::new(AtomicBool::new(false));

        let generation = Arc::new(Mutex::new(0));
        let worker_generation = Arc::clone(&generation);
        let worker_counters = counters.clone();
        let worker_frame_path = frame_path.clone();
        let worker = thread::Builder::new()
            .name("navbridge-h264-sw".into())
            .spawn(move || {
                let mut engine = match DecoderEngine::new(
                    worker_frame_path,
                    Arc::clone(&worker_counters.decoded),
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
                            warn!(%err, "failed to reset software H.264 decoder");
                            break;
                        }
                        engine.active_generation = generation;
                    }
                    if let Err(err) = engine.push(&segment) {
                        worker_counters.dropped.fetch_add(1, Ordering::Relaxed);
                        warn!(%err, "software H.264 decode error; resetting decoder");
                        if let Err(reset_err) = engine.reset() {
                            warn!(%reset_err, "failed to reset software H.264 decoder");
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
                return Err("software H.264 decoder did not initialize in time".into());
            }
        }

        info!(
            output = %frame_path.display(),
            decoder = "FFmpeg H.264 software",
            display = %format!("{}x{} RGBA", UI_WIDTH, UI_HEIGHT),
            "software projection decoder started"
        );

        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            counters,
            wait_for_idr,
            frame_path,
            generation,
        })
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
    // SPS/PPS waiting for the next picture; FFmpeg rejects a packet without one.
    codec_config: Vec<u8>,
    scaler: Option<Scaler>,
    decoded_count: Arc<AtomicU64>,
    frame_path: PathBuf,
    sequence: u64,
    generation: Arc<Mutex<u64>>,
    active_generation: u64,
}

impl DecoderEngine {
    fn new(frame_path: PathBuf, decoded_count: Arc<AtomicU64>) -> Result<Self, String> {
        Ok(Self {
            decoder: make_decoder()?,
            codec_config: Vec::new(),
            scaler: None,
            decoded_count,
            frame_path,
            sequence: 0,
            generation: Arc::new(Mutex::new(0)),
            active_generation: 0,
        })
    }

    fn reset(&mut self) -> Result<(), String> {
        self.decoder = make_decoder()?;
        self.codec_config.clear();
        self.scaler = None;
        let _ = fs::remove_file(&self.frame_path);
        debug!("software H.264 decoder reset");
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
            data.extend_from_slice(access_unit);
            ffmpeg::Packet::copy(&data)
        };
        self.decoder
            .send_packet(&packet)
            .map_err(|e| format!("send H.264 packet: {e}"))?;
        self.drain_frames()
    }

    fn drain_frames(&mut self) -> Result<(), String> {
        let mut decoded = ffmpeg::frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.width() == 0 || decoded.height() == 0 || decoded.format() == Pixel::None {
                continue;
            }

            normalize_full_range(&mut decoded);
            let spec = (
                decoded.format(),
                decoded.width(),
                decoded.height(),
                decoded.color_range(),
                decoded.color_space(),
            );
            let rebuild = self.scaler.as_ref().map(|s| s.spec != spec).unwrap_or(true);
            if rebuild {
                let mut context = ScalingContext::get(
                    decoded.format(),
                    decoded.width(),
                    decoded.height(),
                    Pixel::RGBA,
                    UI_WIDTH,
                    UI_HEIGHT,
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
                    "projection video format established"
                );
            }

            let mut rgba = ffmpeg::frame::Video::new(Pixel::RGBA, UI_WIDTH, UI_HEIGHT);
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
    spec: (Pixel, u32, u32, Range, Space),
    context: ScalingContext,
}

fn make_decoder() -> Result<ffmpeg::decoder::Video, String> {
    let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
        .ok_or_else(|| "FFmpeg H.264 decoder not found".to_string())?;
    ffmpeg::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .map_err(|e| format!("open FFmpeg H.264 software decoder: {e}"))
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
    let row_bytes = UI_WIDTH as usize * RGBA_BYTES_PER_PIXEL;
    let stride = frame.stride(0);
    let data = frame.data(0);
    if stride < row_bytes || data.len() < stride.saturating_mul(UI_HEIGHT as usize) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid RGBA stride or buffer",
        ));
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = File::create(&tmp)?;
    file.write_all(&VIDEO_FRAME_MAGIC)?;
    file.write_all(&UI_WIDTH.to_le_bytes())?;
    file.write_all(&UI_HEIGHT.to_le_bytes())?;
    file.write_all(&sequence.to_le_bytes())?;
    debug_assert_eq!(VIDEO_FRAME_HEADER_SIZE, 20);
    if stride == row_bytes {
        file.write_all(&data[..FRAME_BYTES])?;
    } else {
        for row in data.chunks_exact(stride).take(UI_HEIGHT as usize) {
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
            for (x, y, expected) in [
                (20, 20, 0_i16),
                (380, 20, 255),
                (20, 220, 255),
                (380, 220, 0),
            ] {
                let offset = (y * UI_WIDTH as usize + x) * 4;
                for channel in &pixels[offset..offset + 3] {
                    assert!((*channel as i16 - expected).abs() <= 2);
                }
                assert_eq!(pixels[offset + 3], 255);
            }
        }
    }

    #[test]
    fn every_access_unit_is_published_without_waiting_for_the_next() {
        let (config, first_picture) = codec_config_and_first_picture();
        let count = Arc::new(AtomicU64::new(0));
        let path = std::env::temp_dir().join(format!(
            "navbridge-access-unit-test-{}.rgba",
            std::process::id()
        ));
        let mut engine = DecoderEngine::new(path.clone(), Arc::clone(&count)).unwrap();
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
        let pipeline = VideoPipeline::start(&path).unwrap();
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
        let mut engine = DecoderEngine::new(path.clone(), count).unwrap();
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
        // The extra source column guarantees padding beyond the display width.
        let mut frame = ffmpeg::frame::Video::new(Pixel::RGBA, UI_WIDTH + 1, UI_HEIGHT);
        let stride = frame.stride(0);
        frame.data_mut(0).fill(0xee);
        for y in 0..UI_HEIGHT as usize {
            frame.data_mut(0)[y * stride..y * stride + UI_WIDTH as usize * 4].fill(y as u8);
        }
        let expected = tightly_pack_rgba(&frame).unwrap();
        let path =
            std::env::temp_dir().join(format!("navbridge-stride-test-{}.rgba", std::process::id()));
        publish_frame(&path, 17, &frame).unwrap();
        let published = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(&published[12..20], &17u64.to_le_bytes());
        assert_eq!(&published[VIDEO_FRAME_HEADER_SIZE..], expected.as_slice());
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
