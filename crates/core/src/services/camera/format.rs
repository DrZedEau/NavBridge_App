//! What the camera service asks a device for, and how it reads what the device
//! gives: the pixel packing, which lines of a buffer make the picture, and the
//! picture's colours.

use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;

use super::v4l2::{self, PixFormat};

/// The size asked of a USB camera: one every UVC camera gives in packed
/// YUV at 30 frames a second, well within USB 2.0. A video decoder such as the
/// ADV7280-M answers with its standard's size instead (720x480 for NTSC,
/// 720x576 for PAL, or a field of either).
pub const REQUESTED_SIZE: (u32, u32) = (640, 480);

/// The packed 4:2:2 layouts the scaler reads as they are: YUYV from USB
/// cameras, UYVY from the ADV7280-M.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Yuyv,
    Uyvy,
}

impl Packing {
    pub fn from_fourcc(code: u32) -> Option<Self> {
        match code {
            v4l2::PIX_FMT_YUYV => Some(Self::Yuyv),
            v4l2::PIX_FMT_UYVY => Some(Self::Uyvy),
            _ => None,
        }
    }

    pub fn fourcc(self) -> u32 {
        match self {
            Self::Yuyv => v4l2::PIX_FMT_YUYV,
            Self::Uyvy => v4l2::PIX_FMT_UYVY,
        }
    }

    pub fn pixel(self) -> Pixel {
        match self {
            Self::Yuyv => Pixel::YUYV422,
            Self::Uyvy => Pixel::UYVY422,
        }
    }

    pub const BYTES_PER_PIXEL: usize = 2;
}

/// The first of the device's formats, in its own order, that the service
/// reads. Cameras offering only MJPEG or H.264 are not read yet.
pub fn choose_packing(offered: &[u32]) -> Option<Packing> {
    offered.iter().find_map(|&code| Packing::from_fourcc(code))
}

/// The format asked of the device: `packing` at the requested size, whatever
/// field order it gives.
pub fn request(packing: Packing) -> PixFormat {
    PixFormat {
        width: REQUESTED_SIZE.0,
        height: REQUESTED_SIZE.1,
        pixelformat: packing.fourcc(),
        field: v4l2::FIELD_ANY,
        ..Default::default()
    }
}

/// Where a captured buffer's picture lies: from `offset`, `height` lines of
/// `width` pixels, `stride` bytes apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lines {
    pub offset: usize,
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}

impl Lines {
    /// The bytes the buffer must hold for these lines.
    pub fn needed(&self) -> usize {
        let row = self.width as usize * Packing::BYTES_PER_PIXEL;
        match self.height as usize {
            0 => 0,
            height => self.offset + self.stride * (height - 1) + row,
        }
    }
}

/// The lines of a buffer, of the negotiated `format`, that make the picture
/// shown; `None` to skip the buffer.
///
/// The UI is 240 lines high, so one field of an interlaced picture (240 lines
/// for NTSC, 288 for PAL) holds all it can show: the top field is shown and
/// the bottom one dropped, which needs no deinterlacer and shows no combing.
/// `buffer_field` is the field the driver reports for this buffer, which only
/// alternating fields need.
pub fn picture_lines(format: &PixFormat, buffer_field: u32) -> Option<Lines> {
    let stride = format.bytesperline as usize;
    let whole = Lines {
        offset: 0,
        stride,
        width: format.width,
        height: format.height,
    };
    let half = format.height / 2;
    match format.field {
        // Both fields woven together: the top field is the even lines.
        v4l2::FIELD_INTERLACED | v4l2::FIELD_INTERLACED_TB | v4l2::FIELD_INTERLACED_BT => {
            Some(Lines {
                stride: stride * 2,
                height: half,
                ..whole
            })
        }
        // One field after the other.
        v4l2::FIELD_SEQ_TB => Some(Lines {
            height: half,
            ..whole
        }),
        v4l2::FIELD_SEQ_BT => Some(Lines {
            offset: stride * half as usize,
            height: half,
            ..whole
        }),
        // One field per buffer; the format's height is the field's.
        v4l2::FIELD_ALTERNATE => (buffer_field == v4l2::FIELD_TOP).then_some(whole),
        // Progressive, or a single field.
        _ => Some(whole),
    }
}

/// The range and matrix of the device's Y'CbCr, following V4L2's defaults
/// when the driver leaves them unsaid: limited range except for the JPEG
/// colorspace, and the BT.709 matrix only for the Rec. 709 colorspace (analog
/// video and most USB cameras use BT.601).
pub fn colors(format: &PixFormat) -> (Range, Space) {
    let range = match format.quantization {
        v4l2::QUANTIZATION_FULL_RANGE => Range::JPEG,
        v4l2::QUANTIZATION_LIM_RANGE => Range::MPEG,
        _ if format.colorspace == v4l2::COLORSPACE_JPEG => Range::JPEG,
        _ => Range::MPEG,
    };
    let space = match format.ycbcr_enc {
        v4l2::YCBCR_ENC_709 => Space::BT709,
        v4l2::YCBCR_ENC_601 => Space::SMPTE170M,
        _ if format.colorspace == v4l2::COLORSPACE_REC709 => Space::BT709,
        _ => Space::SMPTE170M,
    };
    (range, space)
}

/// A field order's name, for the log and the System page.
pub fn field_name(field: u32) -> &'static str {
    match field {
        v4l2::FIELD_NONE => "progressive",
        v4l2::FIELD_TOP => "top field",
        v4l2::FIELD_BOTTOM => "bottom field",
        v4l2::FIELD_INTERLACED | v4l2::FIELD_INTERLACED_TB | v4l2::FIELD_INTERLACED_BT => {
            "interlaced"
        }
        v4l2::FIELD_SEQ_TB | v4l2::FIELD_SEQ_BT => "sequential fields",
        v4l2::FIELD_ALTERNATE => "alternating fields",
        _ => "unknown field order",
    }
}
