//! H.264 and H.265 access units: which NAL units they carry.

use navbridge_common::VideoCodec;

/// A NAL unit's type, from the first byte of its header.
pub(super) fn nal_type(codec: VideoCodec, header: u8) -> u8 {
    match codec {
        VideoCodec::H264 => header & 0x1f,
        VideoCodec::H265 => (header >> 1) & 0x3f,
    }
}

/// Annex-B NAL unit types in stream order; covers 3- and 4-byte start codes.
pub(super) fn nal_unit_types(codec: VideoCodec, data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(move |w| nal_type(codec, w[3]))
}

/// Detect a picture a decoder can start from, without parsing the whole
/// stream: an H.264 IDR (type 5), or an H.265 random access one (16-21).
pub(super) fn contains_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    let keyframe = match codec {
        VideoCodec::H264 => 5..=5,
        VideoCodec::H265 => 16..=21,
    };
    nal_unit_types(codec, data).any(|nal| keyframe.contains(&nal))
}

/// Coded slices are H.264's NAL types 1-5 and H.265's 0-31; the codec
/// configuration has none.
pub(super) fn contains_picture(codec: VideoCodec, data: &[u8]) -> bool {
    let picture = match codec {
        VideoCodec::H264 => 1..=5,
        VideoCodec::H265 => 0..=31,
    };
    nal_unit_types(codec, data).any(|nal| picture.contains(&nal))
}
