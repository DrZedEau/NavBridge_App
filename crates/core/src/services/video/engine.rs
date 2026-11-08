//! The decoder thread's engine: it feeds access units to the decoder, keeps the
//! newest picture, and gives way to software when a hardware decoder fails.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::decoders::{from_hardware, open_decoder, show_decoder, Hardware};
use super::nal::contains_picture;
use super::DecoderInfo;
use crate::services::frames::{
    configure_colors, normalize_full_range, publish_middle, FrameSize, Scaler,
};
use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::software::scaling::flag::Flags as ScalingFlags;
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;
use navbridge_common::{VideoCodec, MEDIA_HEIGHT, MEDIA_WIDTH};
use tracing::{debug, info, warn};

pub(super) const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;

/// A hardware decoder that takes this many pictures without returning a frame
/// is given up for the software one.
pub(super) const HARDWARE_STALL_PICTURES: u32 = 30;

pub(super) struct DecoderEngine {
    pub(super) decoder: ffmpeg::decoder::Video,
    /// What `decoder` decodes.
    pub(super) codec: VideoCodec,
    /// Hardware decoders still worth trying, by codec: none once one has
    /// failed, or is missing, or when core.toml asks for software.
    pub(super) hardware: Vec<(VideoCodec, Hardware)>,
    /// Which of them `decoder` is, if one.
    pub(super) on_hardware: Option<Hardware>,
    /// Pictures sent since the last frame came back.
    pub(super) pending_pictures: u32,
    pub(super) info: DecoderInfo,
    // The codec configuration (SPS/PPS, and H.265's VPS) waiting for the next
    // picture; FFmpeg rejects a packet without one.
    pub(super) codec_config: Vec<u8>,
    /// The stream's last configuration, for a decoder that takes over mid-stream.
    pub(super) last_config: Vec<u8>,
    pub(super) size: FrameSize,
    pub(super) scaler: Option<Scaler>,
    /// The last picture decoded, until it is shown or a newer one replaces it.
    pub(super) newest: Option<ffmpeg::frame::Video>,
    pub(super) decoded_count: Arc<AtomicU64>,
    pub(super) frame_path: PathBuf,
    pub(super) sequence: u64,
    pub(super) generation: Arc<Mutex<u64>>,
    pub(super) active_generation: u64,
}

impl DecoderEngine {
    pub(super) fn new(
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
    pub(super) fn reset(&mut self) -> Result<(), String> {
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
    pub(super) fn give_up_hardware(&mut self, reason: &str) -> Result<(), String> {
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

    pub(super) fn push(&mut self, access_unit: &[u8]) -> Result<(), String> {
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
    pub(super) fn no_drm_hwaccel(&mut self) {
        info!(codec = ?self.codec, "no hardware video decoder on this board");
        let codec = self.codec;
        self.hardware
            .retain(|&entry| entry != (codec, Hardware::Drm));
        self.on_hardware = None;
        let name = self.decoder.codec().map(|c| c.name().to_owned());
        show_decoder(&self.info, name.as_deref().unwrap_or_default(), false);
    }

    /// Takes the pictures the decoder has finished, keeping the newest.
    pub(super) fn drain_frames(&mut self) {
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
    pub(super) fn show_newest(&mut self) -> Result<(), String> {
        let Some(mut decoded) = self.newest.take() else {
            return Ok(());
        };
        if decoded.format() == Pixel::DRM_PRIME {
            decoded = from_hardware(&decoded)?;
        }
        normalize_full_range(&mut decoded);
        let (width, height) = self.size.get();
        // The whole picture, of which a 4:3 UI gets the middle: cropping it
        // here works whatever format the decoder gives.
        let scaled_width = height * MEDIA_WIDTH / MEDIA_HEIGHT;
        let spec = (
            decoded.format(),
            decoded.width(),
            decoded.height(),
            decoded.color_range(),
            decoded.color_space(),
            scaled_width,
            height,
        );
        let rebuild = self.scaler.as_ref().map(|s| s.spec != spec).unwrap_or(true);
        if rebuild {
            let mut context = ScalingContext::get(
                decoded.format(),
                decoded.width(),
                decoded.height(),
                Pixel::RGBA,
                scaled_width,
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

        let mut rgba = ffmpeg::frame::Video::new(Pixel::RGBA, scaled_width, height);
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
        publish_middle(&self.frame_path, self.sequence, &rgba, width)
            .map_err(|e| format!("publish projection frame: {e}"))?;
        Ok(())
    }
}
