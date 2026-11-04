use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ffmpeg::util::color::{Range, Space};
use ffmpeg_next as ffmpeg;
use navbridge_common::{UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC};

use super::device::fourcc_text;
use super::format::*;
use super::picture::Pictures;
use super::v4l2::{self, PixFormat};
use super::*;

const UI: (u32, u32) = (UI_WIDTH, UI_HEIGHT);
const LIMITED_601: (Range, Space) = (Range::MPEG, Space::SMPTE170M);

/// A packed 4:2:2 buffer whose row `y` is all of the colour `colour(y)`, given
/// as (Y, Cb, Cr).
fn packed(
    packing: Packing,
    (width, height, stride): (u32, u32, usize),
    colour: impl Fn(u32) -> (u8, u8, u8),
) -> Vec<u8> {
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height {
        let (luma, cb, cr) = colour(y);
        let pair = match packing {
            Packing::Yuyv => [luma, cb, luma, cr],
            Packing::Uyvy => [cb, luma, cr, luma],
        };
        let row = &mut data[y as usize * stride..][..width as usize * 2];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pair);
        }
    }
    data
}

fn pixel(frame: &ffmpeg::frame::Video, x: usize, y: usize) -> [u8; 4] {
    let at = y * frame.stride(0) + x * 4;
    frame.data(0)[at..at + 4].try_into().unwrap()
}

fn assert_near(actual: [u8; 4], expected: [u8; 3]) {
    for (a, e) in actual.iter().zip(expected) {
        assert!(a.abs_diff(e) <= 3, "{actual:?} is not near {expected:?}");
    }
    assert_eq!(actual[3], 255, "opaque");
}

fn format(width: u32, height: u32, field: u32) -> PixFormat {
    PixFormat {
        width,
        height,
        pixelformat: v4l2::PIX_FMT_UYVY,
        field,
        bytesperline: width * 2,
        ..Default::default()
    }
}

#[test]
fn the_v4l2_layouts_match_the_kernels() {
    // The kernel's own ioctl numbers, which encode each structure's size, on
    // 64-bit Linux.
    assert_eq!(v4l2::VIDIOC_QUERYCAP, 0x8068_5600);
    assert_eq!(v4l2::VIDIOC_ENUM_FMT, 0xc040_5602);
    assert_eq!(v4l2::VIDIOC_G_FMT, 0xc0d0_5604);
    assert_eq!(v4l2::VIDIOC_S_FMT, 0xc0d0_5605);
    assert_eq!(v4l2::VIDIOC_REQBUFS, 0xc014_5608);
    assert_eq!(v4l2::VIDIOC_QUERYBUF, 0xc058_5609);
    assert_eq!(v4l2::VIDIOC_QBUF, 0xc058_560f);
    assert_eq!(v4l2::VIDIOC_DQBUF, 0xc058_5611);
    assert_eq!(v4l2::VIDIOC_STREAMON, 0x4004_5612);
    assert_eq!(v4l2::VIDIOC_STREAMOFF, 0x4004_5613);

    assert_eq!(offset_of!(v4l2::Capability, capabilities), 84);
    assert_eq!(offset_of!(v4l2::Capability, device_caps), 88);
    assert_eq!(offset_of!(v4l2::FmtDesc, pixelformat), 44);
    assert_eq!(offset_of!(v4l2::Format, fmt), 8);
    assert_eq!(size_of::<PixFormat>(), 48);
    assert_eq!(offset_of!(PixFormat, quantization), 40);
    assert_eq!(offset_of!(v4l2::Buffer, field), 16);
    assert_eq!(offset_of!(v4l2::Buffer, timestamp), 24);
    assert_eq!(offset_of!(v4l2::Buffer, sequence), 56);
    assert_eq!(offset_of!(v4l2::Buffer, offset), 64);
    assert_eq!(offset_of!(v4l2::Buffer, length), 72);
    assert_eq!(offset_of!(v4l2::Buffer, request_fd), 80);

    assert_eq!(v4l2::PIX_FMT_YUYV, 0x5659_5559);
    assert_eq!(v4l2::PIX_FMT_UYVY, 0x5956_5955);
    assert_eq!(fourcc_text(v4l2::PIX_FMT_MJPEG), "MJPG");
}

#[test]
fn the_first_packed_format_the_camera_offers_is_chosen() {
    let (yuyv, uyvy, mjpeg) = (v4l2::PIX_FMT_YUYV, v4l2::PIX_FMT_UYVY, v4l2::PIX_FMT_MJPEG);
    assert_eq!(choose_packing(&[mjpeg, yuyv]), Some(Packing::Yuyv));
    assert_eq!(choose_packing(&[uyvy, yuyv]), Some(Packing::Uyvy));
    assert_eq!(choose_packing(&[mjpeg]), None);
    assert_eq!(choose_packing(&[]), None);

    let asked = request(Packing::Yuyv);
    assert_eq!((asked.width, asked.height), REQUESTED_SIZE);
    assert_eq!(asked.pixelformat, yuyv);
    assert_eq!(asked.field, v4l2::FIELD_ANY);
}

#[test]
fn of_an_interlaced_picture_only_the_top_field_is_read() {
    let progressive = format(640, 480, v4l2::FIELD_NONE);
    assert_eq!(
        picture_lines(&progressive, v4l2::FIELD_NONE),
        Some(Lines {
            offset: 0,
            stride: 1280,
            width: 640,
            height: 480
        })
    );

    let woven = format(720, 480, v4l2::FIELD_INTERLACED);
    assert_eq!(
        picture_lines(&woven, v4l2::FIELD_INTERLACED),
        Some(Lines {
            offset: 0,
            stride: 2880,
            width: 720,
            height: 240
        })
    );

    let bottom_first = format(720, 576, v4l2::FIELD_SEQ_BT);
    assert_eq!(
        picture_lines(&bottom_first, v4l2::FIELD_SEQ_BT),
        Some(Lines {
            offset: 1440 * 288,
            stride: 1440,
            width: 720,
            height: 288
        })
    );

    // The ADV7280-M's default: one field per buffer, alternately.
    let alternate = format(720, 240, v4l2::FIELD_ALTERNATE);
    assert_eq!(
        picture_lines(&alternate, v4l2::FIELD_TOP).map(|l| l.height),
        Some(240)
    );
    assert_eq!(picture_lines(&alternate, v4l2::FIELD_BOTTOM), None);
}

#[test]
fn colours_follow_the_drivers_word_else_v4l2s_defaults() {
    let mut format = format(720, 240, v4l2::FIELD_ALTERNATE);
    assert_eq!(colors(&format), LIMITED_601);

    format.colorspace = v4l2::COLORSPACE_JPEG;
    assert_eq!(colors(&format), (Range::JPEG, Space::SMPTE170M));

    format.colorspace = v4l2::COLORSPACE_REC709;
    assert_eq!(colors(&format), (Range::MPEG, Space::BT709));

    format.quantization = v4l2::QUANTIZATION_FULL_RANGE;
    format.ycbcr_enc = v4l2::YCBCR_ENC_601;
    assert_eq!(colors(&format), (Range::JPEG, Space::SMPTE170M));

    format.colorspace = v4l2::COLORSPACE_SRGB;
    format.quantization = v4l2::QUANTIZATION_DEFAULT;
    format.ycbcr_enc = v4l2::YCBCR_ENC_DEFAULT;
    assert_eq!(colors(&format), LIMITED_601);
}

#[test]
fn packed_yuv_becomes_the_uis_rgba_in_either_byte_order() {
    // White, black and red rows, in BT.601 limited range.
    let rows = |y: u32| match y * 3 / 240 {
        0 => (235, 128, 128),
        1 => (16, 128, 128),
        _ => (81, 90, 240),
    };
    for packing in [Packing::Yuyv, Packing::Uyvy] {
        let data = packed(packing, (720, 240, 1440), rows);
        let lines = picture_lines(&format(720, 240, v4l2::FIELD_NONE), 0).unwrap();
        let mut pictures = Pictures::default();
        let rgba = pictures
            .convert(packing, lines, LIMITED_601, &data, UI)
            .unwrap();
        assert_eq!((rgba.width(), rgba.height()), UI);
        assert_near(pixel(&rgba, 200, 40), [255, 255, 255]);
        assert_near(pixel(&rgba, 200, 120), [0, 0, 0]);
        assert_near(pixel(&rgba, 200, 200), [255, 0, 0]);
    }
}

#[test]
fn full_range_cameras_keep_their_black_and_white() {
    let rows = |y: u32| {
        if y < 120 {
            (255, 128, 128)
        } else {
            (0, 128, 128)
        }
    };
    let data = packed(Packing::Yuyv, (640, 240, 1280), rows);
    let lines = picture_lines(&format(640, 240, v4l2::FIELD_NONE), 0).unwrap();
    let rgba = Pictures::default()
        .convert(
            Packing::Yuyv,
            lines,
            (Range::JPEG, Space::SMPTE170M),
            &data,
            UI,
        )
        .unwrap();
    assert_near(pixel(&rgba, 10, 10), [255, 255, 255]);
    assert_near(pixel(&rgba, 10, 230), [0, 0, 0]);
}

#[test]
fn a_woven_picture_shows_its_top_field_without_combing() {
    // The top field white, the bottom one black: only white may show.
    let format = format(720, 480, v4l2::FIELD_INTERLACED);
    let rows = |y: u32| {
        if y.is_multiple_of(2) {
            (235, 128, 128)
        } else {
            (16, 128, 128)
        }
    };
    let data = packed(Packing::Uyvy, (720, 480, 1440), rows);
    let lines = picture_lines(&format, v4l2::FIELD_INTERLACED).unwrap();
    let rgba = Pictures::default()
        .convert(Packing::Uyvy, lines, LIMITED_601, &data, UI)
        .unwrap();
    for y in [0, 1, 119, 120, 239] {
        assert_near(pixel(&rgba, 100, y), [255, 255, 255]);
    }
}

#[test]
fn a_buffer_too_short_for_its_format_is_refused() {
    let lines = picture_lines(&format(720, 240, v4l2::FIELD_NONE), 0).unwrap();
    assert_eq!(lines.needed(), 1440 * 240);
    let short = vec![0u8; lines.needed() - 1];
    let Err(error) = Pictures::default().convert(Packing::Uyvy, lines, LIMITED_601, &short, UI)
    else {
        panic!("a short buffer was converted");
    };
    assert!(error.contains("needed"), "{error}");
}

fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "navbridge-camera-{name}-{}.rgba",
        std::process::id()
    ))
}

fn header(path: &Path) -> (u32, u32, u64) {
    let file = fs::read(path).unwrap();
    assert_eq!(file[..4], VIDEO_FRAME_MAGIC);
    let word = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
    let sequence = u64::from_le_bytes(file[12..20].try_into().unwrap());
    assert_eq!(
        file.len(),
        VIDEO_FRAME_HEADER_SIZE + (word(4) * word(8) * 4) as usize
    );
    (word(4), word(8), sequence)
}

#[test]
fn camera_frames_are_published_in_the_projections_format() {
    let path = temporary("publish");
    let data = packed(Packing::Yuyv, (640, 480, 1280), |_| (128, 128, 128));
    let lines = picture_lines(&format(640, 480, v4l2::FIELD_NONE), 0).unwrap();
    let mut pictures = Pictures::default();
    for expected in 1..=2 {
        let rgba = pictures
            .convert(Packing::Yuyv, lines, LIMITED_601, &data, UI)
            .unwrap();
        pictures.publish(&path, &rgba).unwrap();
        assert_eq!(header(&path), (UI_WIDTH, UI_HEIGHT, expected));
    }
    fs::remove_file(path).unwrap();
}

fn wait_for(service: &CameraService, wanted: impl Fn(&CameraState) -> bool) -> CameraState {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = service.state();
        if wanted(&state) || Instant::now() > deadline {
            return state;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_missing_camera_is_reported_and_stops_promptly() {
    let config = CameraConfig {
        enabled: true,
        device: PathBuf::from("/dev/navbridge-no-such-camera"),
    };
    let service = CameraService::start(config, temporary("missing"), FrameSize::default());
    service.set_reverse(true);
    assert_eq!(
        wait_for(&service, |s| *s == CameraState::Missing),
        CameraState::Missing
    );
    let stopping = Instant::now();
    drop(service);
    assert!(stopping.elapsed() < Duration::from_millis(500));
}

#[test]
fn a_device_that_is_no_camera_is_reported_as_failed() {
    let config = CameraConfig {
        enabled: true,
        device: PathBuf::from("/dev/null"),
    };
    let service = CameraService::start(config, temporary("null"), FrameSize::default());
    let state = wait_for(&service, |s| matches!(s, CameraState::Failed(_)));
    assert!(matches!(state, CameraState::Failed(_)), "{state:?}");
}
