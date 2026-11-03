//! Captured pictures to the UI's RGBA frames, through the scaler the
//! projection uses.

use std::path::Path;

use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::software::scaling::flag::Flags as ScalingFlags;
use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;

use super::format::{Lines, Packing};
use crate::services::frames::{configure_colors, publish_frame, Scaler};

/// Turns captured pictures into frames for the UI, keeping its scaler while
/// the camera's format and the UI's size stay the same.
#[derive(Default)]
pub struct Pictures {
    scaler: Option<Scaler>,
    sequence: u64,
}

impl Pictures {
    /// The picture in `data`, at `lines`, as RGBA at `size`: stretched to fill
    /// it, as reverse cameras are shown.
    pub fn convert(
        &mut self,
        packing: Packing,
        lines: Lines,
        (range, space): (Range, Space),
        data: &[u8],
        size: (u32, u32),
    ) -> Result<ffmpeg::frame::Video, String> {
        if lines.width == 0 || lines.height == 0 || !lines.width.is_multiple_of(2) {
            return Err(format!(
                "camera picture of {}x{} cannot be read",
                lines.width, lines.height
            ));
        }
        if data.len() < lines.needed() {
            return Err(format!(
                "camera buffer holds {} bytes, {} needed",
                data.len(),
                lines.needed()
            ));
        }
        let mut source = ffmpeg::frame::Video::new(packing.pixel(), lines.width, lines.height);
        let row = lines.width as usize * Packing::BYTES_PER_PIXEL;
        let stride = source.stride(0);
        let target = source.data_mut(0);
        for y in 0..lines.height as usize {
            let from = lines.offset + y * lines.stride;
            target[y * stride..][..row].copy_from_slice(&data[from..][..row]);
        }

        let spec = (
            packing.pixel(),
            lines.width,
            lines.height,
            range,
            space,
            size.0,
            size.1,
        );
        if self.scaler.as_ref().map(|s| s.spec != spec).unwrap_or(true) {
            let mut context = ScalingContext::get(
                packing.pixel(),
                lines.width,
                lines.height,
                Pixel::RGBA,
                size.0,
                size.1,
                ScalingFlags::AREA | ScalingFlags::FULL_CHR_H_INT,
            )
            .map_err(|e| format!("create camera scaler: {e}"))?;
            configure_colors(&mut context, range, space)?;
            self.scaler = Some(Scaler { spec, context });
        }
        let mut rgba = ffmpeg::frame::Video::new(Pixel::RGBA, size.0, size.1);
        self.scaler
            .as_mut()
            .expect("scaler was initialized")
            .context
            .run(&source, &mut rgba)
            .map_err(|e| format!("scale camera picture: {e}"))?;
        Ok(rgba)
    }

    /// Replaces the frame file at `path` with `rgba`, numbered after the last.
    pub fn publish(&mut self, path: &Path, rgba: &ffmpeg::frame::Video) -> std::io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        publish_frame(path, self.sequence, rgba)
    }
}
