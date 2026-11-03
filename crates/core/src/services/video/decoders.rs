//! Choosing and opening a decoder: the board's hardware ones, FFmpeg's DRM
//! hwaccel, or FFmpeg's software one, and how the System page names it.

use std::ffi::CStr;
use std::sync::atomic::Ordering;
use std::{fmt, ptr};

use super::DecoderInfo;
use ffmpeg_next as ffmpeg;
use navbridge_common::{VideoCodec, MEDIA_HEIGHT, MEDIA_WIDTH};
use tracing::info;

/// Hardware decoders tried before FFmpeg's software one, by codec: the V4L2
/// H.264 decoder of the Pi 4 and CM4, and the HEVC decoder (rpi-hevc-dec) of
/// the Pi 4 and 5 and their CMs, which Raspberry Pi FFmpeg drives as a DRM
/// hwaccel. The Pi 5 and CM5 have none for H.264, so they decode it in software.
pub(super) const HARDWARE_DECODERS: &[(VideoCodec, Hardware)] = &[
    (VideoCodec::H264, Hardware::Decoder("h264_v4l2m2m")),
    (VideoCodec::H265, Hardware::Drm),
];

/// A hardware decoder: an FFmpeg decoder of its own, or FFmpeg's decoder for
/// the codec with a DRM device, through which a hwaccel decodes and hands
/// frames over as DRM buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hardware {
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

/// FFmpeg's version without its package suffix ("7.1.5-0+deb13u1+rpt2" is
/// 7.1.5), the decoder's name, and whether it runs on hardware.
pub(super) fn decoder_text(version: &str, name: &str, hardware: bool) -> String {
    let version = version.split('-').next().unwrap_or_default();
    let kind = if hardware { "hardware" } else { "software" };
    format!("FFmpeg {version} {name} ({kind})")
}

/// FFmpeg's name for a codec.
pub(super) fn codec_id(codec: VideoCodec) -> ffmpeg::codec::Id {
    match codec {
        VideoCodec::H264 => ffmpeg::codec::Id::H264,
        VideoCodec::H265 => ffmpeg::codec::Id::HEVC,
    }
}

/// Whether FFmpeg has a software decoder for `codec`.
pub(super) fn decodes(codec: VideoCodec) -> bool {
    ffmpeg::decoder::find(codec_id(codec)).is_some()
}

/// Opens the first of the `hardware` decoders for `codec` that works, else
/// FFmpeg's software one, and names it in `info`. Returns which hardware one
/// it is, if one. Boards do not grow a decoder: those that do not open are
/// taken off `hardware`, so they are tried only once.
pub(super) fn open_decoder(
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
pub(super) fn show_decoder(info: &DecoderInfo, name: &str, hardware: bool) {
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
pub(super) fn has_drm_hwaccel(codec: ffmpeg::Codec) -> bool {
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
pub(super) fn open_codec(
    codec: ffmpeg::Codec,
    drm: bool,
) -> Result<ffmpeg::decoder::Video, ffmpeg::Error> {
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
pub(super) fn from_hardware(frame: &ffmpeg::frame::Video) -> Result<ffmpeg::frame::Video, String> {
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
