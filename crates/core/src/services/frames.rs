//! Pictures for the UI, from the projection decoder or the reverse camera:
//! their colours, the scaler, and the frame files the UI reads.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;
use navbridge_common::{
    MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH, UI_WIDTH_4_3, VIDEO_FRAME_HEADER_SIZE,
    VIDEO_FRAME_MAGIC,
};

pub(crate) const RGBA_BYTES_PER_PIXEL: usize = 4;

/// The size frames are published at, which the UI chooses: the panel's
/// 400x240 or a 4:3 panel's 320x240, or twice either for a UI drawn twice as
/// large on a PC. It can change while video plays.
#[derive(Clone)]
pub struct FrameSize(Arc<AtomicU64>);

impl Default for FrameSize {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(Self::pack(UI_WIDTH, UI_HEIGHT))))
    }
}

impl FrameSize {
    /// The sizes a UI can ask for; any other is refused.
    pub const CHOICES: [(u32, u32); 4] = [
        (UI_WIDTH, UI_HEIGHT),
        (MEDIA_WIDTH, MEDIA_HEIGHT),
        (UI_WIDTH_4_3, UI_HEIGHT),
        (UI_WIDTH_4_3 * 2, UI_HEIGHT * 2),
    ];

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

    /// The columns of Android Auto's picture the phone is to leave black, half
    /// on each side, so that it draws in the UI's shape: 160 for a 4:3 UI.
    pub fn margin_width(&self) -> u32 {
        let (width, height) = self.get();
        MEDIA_WIDTH - width * MEDIA_HEIGHT / height
    }
}

// YUVJ has the same plane layout as YUV; retain its explicit full range.
pub(crate) fn normalize_full_range(frame: &mut ffmpeg::frame::Video) {
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

pub(crate) fn configure_colors(
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

pub(crate) struct Scaler {
    /// The source's format, size and colours, and the output size.
    pub(crate) spec: (Pixel, u32, u32, Range, Space, u32, u32),
    pub(crate) context: ScalingContext,
}

pub(crate) fn publish_frame(
    path: &Path,
    sequence: u64,
    frame: &ffmpeg::frame::Video,
) -> Result<(), std::io::Error> {
    publish_middle(path, sequence, frame, frame.width())
}

/// Publishes the middle `width` columns of `frame`, the part of the projection
/// between the margins a 4:3 UI asks the phone for.
pub(crate) fn publish_middle(
    path: &Path,
    sequence: u64,
    frame: &ffmpeg::frame::Video,
    width: u32,
) -> Result<(), std::io::Error> {
    let height = frame.height();
    let row_bytes = width as usize * RGBA_BYTES_PER_PIXEL;
    let whole_row = frame.width() as usize * RGBA_BYTES_PER_PIXEL;
    let stride = frame.stride(0);
    let data = frame.data(0);
    if width > frame.width()
        || stride < whole_row
        || data.len() < stride.saturating_mul(height as usize)
    {
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
        // One write for the file, not one for each row.
        let left = (whole_row - row_bytes) / 2;
        let mut rows = Vec::with_capacity(row_bytes * height as usize);
        for row in data.chunks_exact(stride).take(height as usize) {
            rows.extend_from_slice(&row[left..][..row_bytes]);
        }
        file.write_all(&rows)?;
    }
    file.flush()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}
