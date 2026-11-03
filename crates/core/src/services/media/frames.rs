//! Decoded pictures for the UI: their colours, the scaler, and the frame file
//! the UI reads.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;
use navbridge_common::{VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC};

pub(super) const RGBA_BYTES_PER_PIXEL: usize = 4;

// YUVJ has the same plane layout as YUV; retain its explicit full range.
pub(super) fn normalize_full_range(frame: &mut ffmpeg::frame::Video) {
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

pub(super) fn configure_colors(
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

pub(super) struct Scaler {
    /// The source's format, size and colours, and the output size.
    pub(super) spec: (Pixel, u32, u32, Range, Space, u32, u32),
    pub(super) context: ScalingContext,
}

pub(super) fn publish_frame(
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
