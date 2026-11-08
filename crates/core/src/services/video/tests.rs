use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use std::{fs, thread};

use super::decoders::{decoder_text, has_drm_hwaccel, show_decoder, Hardware};
use super::engine::DecoderEngine;
use super::nal::{contains_keyframe, contains_picture, nal_type};
use super::*;
use crate::services::frames::{
    configure_colors, normalize_full_range, publish_frame, publish_middle, RGBA_BYTES_PER_PIXEL,
};
use ffmpeg::software::scaling::context::Context as ScalingContext;
use ffmpeg::software::scaling::flag::Flags as ScalingFlags;
use ffmpeg::util::color::{Range, Space};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;
use navbridge_common::{VideoCodec, UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE};

const FRAME_BYTES: usize = UI_WIDTH as usize * UI_HEIGHT as usize * RGBA_BYTES_PER_PIXEL;

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

/// A codec's fixture split into access units, one per Android Auto video
/// message, at its access unit delimiters (H.264 type 9, H.265 type 35).
fn units_of(codec: VideoCodec) -> Vec<&'static [u8]> {
    let (stream, delimiter): (&'static [u8], u8) = match codec {
        VideoCodec::H264 => (
            include_bytes!("../../../tests/fixtures/parser-continuation.h264"),
            9,
        ),
        VideoCodec::H265 => (
            include_bytes!("../../../tests/fixtures/parser-continuation.h265"),
            35,
        ),
    };
    let mut starts: Vec<usize> = stream
        .windows(5)
        .enumerate()
        .filter_map(|(i, w)| {
            (w[..4] == [0, 0, 0, 1] && nal_type(codec, w[4]) == delimiter).then_some(i)
        })
        .collect();
    assert_eq!(starts.len(), 24);
    starts.push(stream.len());
    starts
        .windows(2)
        .map(|pair| &stream[pair[0]..pair[1]])
        .collect()
}

/// The H.264 fixture's access units.
fn access_units() -> Vec<&'static [u8]> {
    units_of(VideoCodec::H264)
}

/// Split the first unit like Android Auto: the codec configuration
/// message, then the picture.
fn config_and_first_picture_of(codec: VideoCodec) -> (&'static [u8], &'static [u8]) {
    let first = units_of(codec)[0];
    let slice = first
        .windows(4)
        .position(|w| w[..3] == [0, 0, 1] && contains_picture(codec, w))
        .unwrap();
    first.split_at(slice)
}

fn codec_config_and_first_picture() -> (&'static [u8], &'static [u8]) {
    config_and_first_picture_of(VideoCodec::H264)
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
            ScalingFlags::AREA | ScalingFlags::FULL_CHR_H_INT,
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
    software_engine_for(VideoCodec::H264, path, count)
}

fn software_engine_for(codec: VideoCodec, path: PathBuf, count: Arc<AtomicU64>) -> DecoderEngine {
    DecoderEngine::new(
        path,
        count,
        codec,
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
        VideoCodec::H264,
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
    engine.show_newest().unwrap();
    assert_eq!(header(&path), (400, 240, 400 * 240 * 4));
    // Twice as large, mid-stream: Android Auto's own 800x480.
    assert!(size.set(800, 480));
    assert!(!size.set(800, 480), "unchanged");
    engine.push(units.next().unwrap()).unwrap();
    engine.show_newest().unwrap();
    assert_eq!(header(&path), (800, 480, 800 * 480 * 4));
    // A 4:3 UI: the phone's margins are left out, at either scale.
    for (width, height) in [(320, 240), (640, 480)] {
        assert!(size.set(width, height));
        assert_eq!(size.margin_width(), 160);
        engine.push(units.next().unwrap()).unwrap();
        engine.show_newest().unwrap();
        assert_eq!(
            header(&path),
            (width, height, (width * height * 4) as usize)
        );
    }
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
        VideoCodec::H264,
        vec![(VideoCodec::H264, Hardware::Decoder("no_such_decoder"))],
        info.clone(),
        FrameSize::default(),
    )
    .unwrap();
    assert!(engine.on_hardware.is_none());
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
        VideoCodec::H264,
        vec![(VideoCodec::H264, Hardware::Decoder("hevc"))],
        info.clone(),
        FrameSize::default(),
    )
    .unwrap();
    assert_eq!(engine.on_hardware, Some(Hardware::Decoder("hevc")));
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
    assert!(engine.on_hardware.is_none() && engine.hardware.is_empty());
    assert!(info.name.lock().unwrap().ends_with(" h264 (software)"));
    assert!(info.changed.load(Ordering::Relaxed));
    assert!(info.keyframe_wanted.load(Ordering::Relaxed));
    // The stream's SPS/PPS carry over, so its next keyframe decodes alone.
    engine.push(keyframe).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    // A reset for the next phone does not bring the hardware one back.
    engine.reset().unwrap();
    assert!(engine.on_hardware.is_none());
    let _ = fs::remove_file(&path);
}

/// Whether this board has the Raspberry Pi HEVC decoder.
fn board_has_hevc_decoder() -> bool {
    fs::read_dir("/sys/class/video4linux")
        .into_iter()
        .flatten()
        .flatten()
        .any(|device| {
            fs::read_to_string(device.path().join("name"))
                .is_ok_and(|name| name.trim() == "rpi-hevc-dec")
        })
}

#[test]
fn h265_decodes_in_hardware_where_the_board_and_ffmpeg_can() {
    let hevc = ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC).unwrap();
    let hardware = has_drm_hwaccel(hevc) && board_has_hevc_decoder();
    // The last frame, from the DRM hwaccel where there is one, and from
    // the software decoder.
    let last_frame = |candidates: Vec<(VideoCodec, Hardware)>| {
        let path = std::env::temp_dir().join(format!(
            "navbridge-drm-test-{}-{}.rgba",
            candidates.len(),
            std::process::id()
        ));
        let count = Arc::new(AtomicU64::new(0));
        let info = DecoderInfo::default();
        let mut engine = DecoderEngine::new(
            path.clone(),
            Arc::clone(&count),
            VideoCodec::H265,
            candidates,
            info.clone(),
            FrameSize::default(),
        )
        .unwrap();
        // Each access unit's frame comes at once, from the hardware too.
        for (index, unit) in units_of(VideoCodec::H265).into_iter().enumerate() {
            engine.push(unit).unwrap();
            assert_eq!(count.load(Ordering::Relaxed), index as u64 + 1);
            engine.show_newest().unwrap();
        }
        let frame = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        let name = info.name.lock().unwrap().clone();
        (frame, engine.on_hardware, name)
    };
    let (drm, on_hardware, name) = last_frame(vec![(VideoCodec::H265, Hardware::Drm)]);
    assert_eq!(on_hardware.is_some(), hardware);
    let kind = if hardware { "hardware" } else { "software" };
    assert!(name.ends_with(&format!(" hevc ({kind})")), "{name}");
    // H.265 decodes to the same picture everywhere.
    let (software, ..) = last_frame(Vec::new());
    assert_eq!(drm.len(), software.len());
    let differs = drm
        .iter()
        .zip(&software)
        .filter(|(a, b)| a.abs_diff(**b) > 2);
    assert_eq!(differs.count(), 0);
}

#[test]
fn a_drm_hwaccel_that_does_not_start_leaves_software_decoding() {
    let path =
        std::env::temp_dir().join(format!("navbridge-no-drm-test-{}.rgba", std::process::id()));
    let count = Arc::new(AtomicU64::new(0));
    let mut engine = software_engine(path.clone(), Arc::clone(&count));
    // As FFmpeg's HEVC decoder with a DRM device on a board without the
    // hardware: its frames come from software.
    engine.on_hardware = Some(Hardware::Drm);
    engine.hardware = vec![(VideoCodec::H264, Hardware::Drm)];
    show_decoder(&engine.info, "h264", true);
    engine.info.changed.store(false, Ordering::Relaxed);
    for unit in access_units() {
        engine.push(unit).unwrap();
    }
    let _ = fs::remove_file(&path);
    assert_eq!(count.load(Ordering::Relaxed), 24);
    assert!(engine.on_hardware.is_none());
    assert!(engine.hardware.is_empty(), "not tried again");
    assert!(engine
        .info
        .name
        .lock()
        .unwrap()
        .ends_with(" h264 (software)"));
    assert!(engine.info.changed.load(Ordering::Relaxed));
}

#[test]
fn a_new_decoder_asks_the_phone_for_a_keyframe() {
    let (tx, queue) = mpsc::sync_channel(4);
    let decoder = DecoderInfo::default();
    let pipeline = VideoPipeline {
        tx: Some(tx),
        worker: None,
        counters: VideoCounters::default(),
        wait_for_keyframe: Arc::new(AtomicBool::new(false)),
        frame_path: std::env::temp_dir().join(format!(
            "navbridge-keyframe-wanted-test-{}.rgba",
            std::process::id()
        )),
        generation: Arc::new(Mutex::new(0)),
        decoder: decoder.clone(),
        codec: Cell::new(VideoCodec::H264),
    };
    decoder.keyframe_wanted.store(true, Ordering::Relaxed);
    let picture = [0, 0, 0, 1, 0x41];
    assert_eq!(pipeline.push(&picture), PushResult::NeedKeyframe);
    assert_eq!(pipeline.push(&picture), PushResult::Dropped);
    assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Accepted);
    assert_eq!(queue.try_iter().count(), 1);
}

#[test]
fn every_access_unit_is_published_without_waiting_for_the_next() {
    for codec in [VideoCodec::H264, VideoCodec::H265] {
        let (config, first_picture) = config_and_first_picture_of(codec);
        let count = Arc::new(AtomicU64::new(0));
        let path = std::env::temp_dir().join(format!(
            "navbridge-access-unit-test-{codec:?}-{}.rgba",
            std::process::id()
        ));
        let mut engine = software_engine_for(codec, path.clone(), Arc::clone(&count));
        engine.push(config).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(!path.exists());
        let pictures = std::iter::once(first_picture).chain(units_of(codec).into_iter().skip(1));
        for (index, unit) in pictures.enumerate() {
            engine.push(unit).unwrap();
            assert_eq!(
                count.load(Ordering::Relaxed),
                index as u64 + 1,
                "{codec:?} access unit {index} was held back"
            );
            engine.show_newest().unwrap();
            assert_eq!(sequence(&path), index as u64 + 1);
        }
        let frame = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(frame.len(), VIDEO_FRAME_HEADER_SIZE + FRAME_BYTES);
    }
}

/// The sequence number in a published frame's header.
fn sequence(path: &Path) -> u64 {
    let frame = fs::read(path).unwrap();
    u64::from_le_bytes(frame[12..20].try_into().unwrap())
}

#[test]
fn of_the_pictures_that_queued_up_only_the_newest_is_shown() {
    let count = Arc::new(AtomicU64::new(0));
    let path =
        std::env::temp_dir().join(format!("navbridge-newest-test-{}.rgba", std::process::id()));
    let mut engine = software_engine(path.clone(), Arc::clone(&count));
    // All of them decoded, as each builds on the one before...
    for unit in access_units() {
        engine.push(unit).unwrap();
    }
    assert_eq!(count.load(Ordering::Relaxed), 24);
    assert!(!path.exists(), "nothing shown yet");
    // ...and one scaled and published.
    engine.show_newest().unwrap();
    assert_eq!(sequence(&path), 1);
    engine.show_newest().unwrap();
    assert_eq!(sequence(&path), 1, "shown once");
    let _ = fs::remove_file(&path);
}

#[test]
fn a_full_queue_asks_once_for_a_keyframe_then_waits_for_one() {
    let (tx, queue) = mpsc::sync_channel(1);
    let counters = VideoCounters::default();
    let pipeline = VideoPipeline {
        tx: Some(tx),
        worker: None,
        counters: counters.clone(),
        wait_for_keyframe: Arc::new(AtomicBool::new(false)),
        frame_path: std::env::temp_dir().join(format!(
            "navbridge-keyframe-test-{}.rgba",
            std::process::id()
        )),
        generation: Arc::new(Mutex::new(0)),
        decoder: DecoderInfo::default(),
        codec: Cell::new(VideoCodec::H264),
    };
    let (keyframe, frame): (&[u8], &[u8]) = (&[0, 0, 0, 1, 0x65], &[0, 0, 0, 1, 0x41]);
    assert_eq!(pipeline.push(frame), PushResult::Accepted);
    // The decoder is behind: the next frame is lost, so ask once.
    assert_eq!(pipeline.push(frame), PushResult::NeedKeyframe);
    assert_eq!(pipeline.push(frame), PushResult::Dropped);
    // Once the decoder catches up, only a keyframe can restart the picture.
    queue.recv().unwrap();
    assert_eq!(pipeline.push(frame), PushResult::Dropped);
    assert_eq!(pipeline.push(keyframe), PushResult::Accepted);
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
        assert_eq!(pipeline.push(unit), PushResult::Accepted);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while counters.decoded() <= index as u64 {
            assert!(
                std::time::Instant::now() < deadline,
                "unit {index} not counted"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(pipeline.push(&[]), PushResult::Dropped);
    assert_eq!((counters.decoded(), counters.dropped()), (24, 1));
    assert!(!pipeline.stopped());
}

#[test]
fn the_decoder_follows_the_codec_the_phone_chose() {
    let path =
        std::env::temp_dir().join(format!("navbridge-codec-test-{}.rgba", std::process::id()));
    let pipeline = VideoPipeline::start(&path, false, FrameSize::default()).unwrap();
    let counters = pipeline.counters.clone();
    assert!(pipeline.decoder().ends_with(" h264 (software)"));
    pipeline.decoder_changed();
    let mut decoded = 0;
    for codec in [VideoCodec::H265, VideoCodec::H264] {
        pipeline.set_codec(codec);
        for unit in units_of(codec) {
            assert_eq!(pipeline.push(unit), PushResult::Accepted);
            decoded += 1;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while counters.decoded() < decoded {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{codec:?} unit {decoded} not decoded"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        let name = if codec == VideoCodec::H265 {
            "hevc"
        } else {
            "h264"
        };
        assert!(pipeline.decoder().ends_with(&format!(" {name} (software)")));
        assert!(pipeline.decoder_changed());
    }
    assert_eq!(counters.dropped(), 0);
}

#[test]
fn a_decoder_thread_that_gave_up_is_reported() {
    let path = std::env::temp_dir().join(format!(
        "navbridge-stopped-test-{}.rgba",
        std::process::id()
    ));
    // A decoder thread that has stopped, as after a failed reset.
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || drop(rx));
    while !worker.is_finished() {
        thread::sleep(Duration::from_millis(1));
    }
    let pipeline = VideoPipeline {
        tx: Some(tx),
        worker: Some(worker),
        counters: VideoCounters::default(),
        wait_for_keyframe: Arc::new(AtomicBool::new(false)),
        frame_path: path,
        generation: Arc::new(Mutex::new(0)),
        decoder: DecoderInfo::default(),
        codec: Cell::new(VideoCodec::H264),
    };
    assert!(pipeline.stopped());
    assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Stopped);
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
        wait_for_keyframe: Arc::new(AtomicBool::new(false)),
        frame_path: path.clone(),
        generation: generation.clone(),
        decoder: DecoderInfo::default(),
        codec: Cell::new(VideoCodec::H264),
    };
    assert_eq!(pipeline.push(&[0, 0, 0, 1, 0x65]), PushResult::Accepted);
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
    engine.show_newest().unwrap();
    assert!(!path.exists());
    engine.reset().unwrap();
    engine.active_generation = 1;
    for unit in access_units() {
        engine.push(unit).unwrap();
    }
    engine.show_newest().unwrap();
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
fn the_middle_of_a_picture_is_published_without_its_margins() {
    // Each column holds its own number.
    let (width, height) = (400usize, 240usize);
    let mut frame = ffmpeg::frame::Video::new(Pixel::RGBA, width as u32, height as u32);
    let stride = frame.stride(0);
    for y in 0..height {
        for x in 0..width {
            frame.data_mut(0)[y * stride + x * 4..][..4].fill(x as u8);
        }
    }
    let path =
        std::env::temp_dir().join(format!("navbridge-middle-test-{}.rgba", std::process::id()));
    publish_middle(&path, 3, &frame, 320).unwrap();
    let published = fs::read(&path).unwrap();
    assert_eq!(&published[4..8], &320u32.to_le_bytes());
    assert_eq!(&published[8..12], &240u32.to_le_bytes());
    let pixels = &published[VIDEO_FRAME_HEADER_SIZE..];
    assert_eq!(pixels.len(), 320 * 240 * 4);
    for row in pixels.chunks_exact(320 * 4) {
        assert_eq!(row[0], 40, "the first column shown");
        assert_eq!(row[319 * 4], (359 % 256) as u8, "the last column shown");
    }
    // Wider than the picture, there is no middle.
    assert!(publish_middle(&path, 4, &frame, 401).is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn finds_keyframes_annex_b_nal() {
    use VideoCodec::{H264, H265};
    // H.264: an IDR (5), not a plain slice (1).
    assert!(contains_keyframe(H264, &[0, 0, 0, 1, 0x65, 1, 2, 3]));
    assert!(contains_keyframe(H264, &[9, 9, 0, 0, 1, 0x25, 1]));
    assert!(!contains_keyframe(H264, &[0, 0, 0, 1, 0x41, 1, 2]));
    // H.265: IDR (19, 20) and CRA (21), not a trailing picture (1). The
    // H.264 IDR's byte is an H.265 slice.
    assert!(contains_keyframe(H265, &[0, 0, 0, 1, 0x26, 1, 2]));
    assert!(contains_keyframe(H265, &[0, 0, 1, 0x28, 1]));
    assert!(contains_keyframe(H265, &[0, 0, 1, 0x2a, 1]));
    assert!(!contains_keyframe(H265, &[0, 0, 0, 1, 0x02, 1, 2]));
    assert!(!contains_keyframe(H265, &[0, 0, 0, 1, 0x65, 1, 2]));
}

#[test]
fn codec_configuration_is_not_a_picture() {
    use VideoCodec::{H264, H265};
    assert!(!contains_picture(
        H264,
        &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]
    ));
    assert!(!contains_picture(
        H264,
        &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x06, 5]
    ));
    assert!(contains_picture(
        H264,
        &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]
    ));
    assert!(contains_picture(H264, &[0, 0, 1, 0x41, 1]));
    // H.265: VPS, SPS and PPS (32-34), a delimiter (35) and SEI (39).
    assert!(!contains_picture(
        H265,
        &[0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 1, 0, 0, 0, 1, 0x44, 1]
    ));
    assert!(!contains_picture(
        H265,
        &[0, 0, 0, 1, 0x46, 1, 0, 0, 1, 0x4e, 1]
    ));
    assert!(contains_picture(
        H265,
        &[0, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x28, 1]
    ));
    assert!(contains_picture(H265, &[0, 0, 1, 0x02, 1]));
}
