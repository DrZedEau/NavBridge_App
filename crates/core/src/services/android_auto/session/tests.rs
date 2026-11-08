use std::time::Instant;

use super::discovery::discovery;
use super::generated::aa as p;
use super::input::phone_key;
use super::protocol::{
    parse, BLUETOOTH, FULLY_RESTRICTED, INPUT, MEDIA, MIC, SENSOR, UNRESTRICTED, VIDEO,
};
use super::sensors::FRESH_FIX;
use super::*;
use crate::config::AndroidAutoConfig;
use crate::services::android_auto::framing::Parser;
use crate::services::gnss::Fix;
use navbridge_common::{NavigationKey, PhoneAction, TouchKind, VideoCodec};

fn connected_session() -> Session {
    let mut s = Session::new(AndroidAutoConfig::default(), false, false, false).unwrap();
    s.tls = super::super::tls::tests::pair().0;
    s.outgoing.clear();
    s.discovered = true;
    s
}
#[test]
fn shutdown_request_and_reply_both_end_the_session() {
    let mut s = connected_session();
    // The phone asks to stop: we reply, then end the session.
    assert!(matches!(
        s.message(0, 15, &[8, 1]).unwrap().as_slice(),
        [Action::Shutdown]
    ));
    assert_eq!(s.outgoing.len(), 1);
    // The phone acknowledges our own request to stop.
    assert!(matches!(
        s.message(0, 16, &[]).unwrap().as_slice(),
        [Action::Shutdown]
    ));
}
#[test]
fn focus_restore_preserves_media_session_and_controls() {
    let mut s = connected_session();
    s.opened[VIDEO as usize] = true;
    s.opened[INPUT as usize] = true;
    s.sessions[VIDEO as usize] = Some(42);
    let mut focus = p::VideoFocusRequest::new();
    focus.set_mode(2);
    let actions = s
        .message(VIDEO, 0x8007, &focus.write_to_bytes().unwrap())
        .unwrap();
    assert!(matches!(actions.as_slice(), [Action::HostUi]));
    s.resume().unwrap();
    s.key(19, true).unwrap();
    s.key(19, false).unwrap();
    assert_eq!(s.sessions[VIDEO as usize], Some(42));
    assert_eq!(s.outgoing.len(), 4);
    let fresh = Session::new(AndroidAutoConfig::default(), false, false, false).unwrap();
    assert!(!fresh.opened[VIDEO as usize]);
    assert!(fresh.sessions.iter().all(Option::is_none));
    assert!(!fresh.night_sensor);
    assert_eq!(fresh.mic_unacked, 0);
}
#[test]
fn microphone_window_ack_and_stop_are_generation_bound() {
    let mut s = connected_session();
    s.opened[MIC as usize] = true;
    s.sessions[MIC as usize] = Some(12);
    let mut open = p::MicOpen::new();
    open.set_open(true);
    open.set_max_unacked(1);
    s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
        .unwrap();
    // The first block fills the window; the next waits.
    let sent = s.outgoing.len();
    s.microphone(&[0; 640], 10).unwrap();
    s.microphone(&[0; 640], 20).unwrap();
    assert_eq!((s.outgoing.len() - sent, s.mic_waiting.len()), (1, 1));
    // Another session's acknowledgement frees nothing.
    let mut ack = p::Ack::new();
    ack.set_session(11);
    ack.set_value(1);
    s.message(MIC, 0x8004, &ack.write_to_bytes().unwrap())
        .unwrap();
    assert_eq!((s.mic_unacked, s.mic_waiting.len()), (1, 1));
    // Its own sends the waiting block in the room it makes.
    ack.set_session(12);
    s.message(MIC, 0x8004, &ack.write_to_bytes().unwrap())
        .unwrap();
    assert_eq!((s.outgoing.len() - sent, s.mic_unacked), (2, 1));
    assert!(s.mic_waiting.is_empty());
    // Closed, it sends and keeps nothing.
    s.microphone(&[0; 640], 30).unwrap();
    open.set_open(false);
    s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
        .unwrap();
    assert!(s.mic_waiting.is_empty());
    let sent = s.outgoing.len();
    s.microphone(&[0; 640], 40).unwrap();
    assert_eq!((s.outgoing.len(), s.mic_waiting.len()), (sent, 0));
}
#[test]
fn microphone_audio_waits_for_the_window_until_it_is_too_old() {
    let mut s = connected_session();
    s.opened[MIC as usize] = true;
    let mut open = p::MicOpen::new();
    open.set_open(true);
    open.set_max_unacked(2);
    s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
        .unwrap();
    // Fifteen 20 ms blocks: two go, and the rest wait, as long as they
    // span no more than 200 ms.
    for block in 0..15u64 {
        s.microphone(&[0; 640], block * 20_000).unwrap();
    }
    assert_eq!(s.mic_unacked, 2);
    assert_eq!(s.mic_waiting.len(), 11);
    assert_eq!(s.mic_waiting.front().unwrap().0, 80_000);
    assert_eq!(s.mic_dropped, 2);
    // An acknowledgement of more than is outstanding frees the whole
    // window, and the oldest waiting blocks go.
    let mut ack = p::Ack::new();
    ack.set_session(0);
    ack.set_value(5);
    s.message(MIC, 0x8004, &ack.write_to_bytes().unwrap())
        .unwrap();
    assert_eq!(s.mic_unacked, 2);
    assert_eq!(s.mic_waiting.front().unwrap().0, 120_000);
    // Without a window of its own, the phone gets one block at a time.
    open.set_max_unacked(0);
    s.message(MIC, 0x8005, &open.write_to_bytes().unwrap())
        .unwrap();
    assert_eq!((s.mic_window, s.mic_dropped), (1, 0));
    assert!(s.mic_waiting.is_empty());
}
#[test]
fn horizontal_navigation_uses_signed_rotary_and_vertical_uses_dpad() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[INPUT as usize] = true;
    for key in [NavigationKey::Left, NavigationKey::Right, NavigationKey::Up] {
        s.navigation_input(key, true).unwrap();
        s.navigation_input(key, false).unwrap();
    }
    assert_eq!(s.outgoing.len(), 4);
    let inputs = sent_inputs(&mut s, &mut peer);
    for (input, delta) in inputs[..2].iter().zip([-1, 1]) {
        let step = &input.relative.as_ref().unwrap().events[0];
        assert_eq!(step.scan_code(), 65536);
        assert_eq!(step.delta(), delta);
    }
    assert!(inputs[2].buttons.as_ref().unwrap().events[0].pressed());
    assert!(!inputs[3].buttons.as_ref().unwrap().events[0].pressed());
    assert_eq!(inputs[2].buttons.as_ref().unwrap().events[0].keycode(), 19);
    assert_eq!(phone_key(PhoneAction::VoiceAssistant), 84);
}
#[test]
fn select_and_back_send_the_whole_tap_on_press() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[INPUT as usize] = true;
    for (key, keycode) in [(NavigationKey::Select, 23), (NavigationKey::Back, 4)] {
        s.navigation_input(key, true).unwrap();
        let buttons = sent_inputs(&mut s, &mut peer)
            .iter()
            .map(|input| {
                let button = &input.buttons.as_ref().unwrap().events[0];
                (button.keycode(), button.pressed())
            })
            .collect::<Vec<_>>();
        assert_eq!(buttons, [(keycode, true), (keycode, false)]);
        // The release already went out with the press.
        s.navigation_input(key, false).unwrap();
        assert!(s.outgoing.is_empty());
    }
}
#[test]
fn touches_go_out_in_the_projections_pixels() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    // Nothing before the phone opens its input channel.
    s.touch(TouchKind::Down, 10, 10).unwrap();
    assert!(s.outgoing.is_empty());
    s.opened[INPUT as usize] = true;
    for (kind, x, y) in [
        (TouchKind::Down, 100, 200),
        (TouchKind::Move, 120, 210),
        (TouchKind::Up, 900, 600),
    ] {
        s.touch(kind, x, y).unwrap();
    }
    let touches = sent_inputs(&mut s, &mut peer)
        .iter()
        .map(|input| {
            let touch = input.touch.as_ref().unwrap();
            let at = &touch.locations[0];
            (touch.action(), at.x(), at.y(), at.pointer_id())
        })
        .collect::<Vec<_>>();
    // Android's down, move and up; a point past the edge stays on it.
    assert_eq!(
        touches,
        [(0, 100, 200, 0), (2, 120, 210, 0), (1, 799, 479, 0)]
    );
}
/// Decrypts the messages queued for the phone, as (message id, body).
fn sent(
    s: &mut Session,
    peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>,
) -> Vec<(u16, Vec<u8>)> {
    let mut parser = Parser::default();
    let frames = parser
        .feed(&s.outgoing.drain(..).flatten().collect::<Vec<_>>())
        .unwrap();
    frames
        .into_iter()
        .map(|f| {
            let data = super::super::tls::tests::decode(peer, &f.data);
            (u16::from_be_bytes([data[0], data[1]]), data[2..].to_vec())
        })
        .collect()
}
/// Decrypts the input reports queued for the phone.
fn sent_inputs(
    s: &mut Session,
    peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>,
) -> Vec<p::Input> {
    sent(s, peer)
        .into_iter()
        .map(|(id, body)| {
            assert_eq!(id, 0x8001);
            parse::<p::Input>(&body).unwrap()
        })
        .collect()
}
#[test]
fn hiding_resuming_and_keyframe_requests_move_video_focus() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[VIDEO as usize] = true;
    s.hide().unwrap();
    s.resume().unwrap();
    s.request_keyframe().unwrap();
    let focus = sent(&mut s, &mut peer)
        .into_iter()
        .map(|(id, body)| {
            assert_eq!(id, 0x8008);
            let f = parse::<p::VideoFocus>(&body).unwrap();
            (f.mode(), f.unrequested())
        })
        .collect::<Vec<_>>();
    // Hidden, shown, then taken and given back for a keyframe.
    assert_eq!(focus, [(2, true), (1, true), (2, true), (1, true)]);
}
#[test]
fn setup_negotiates_codec_not_legacy_config_index() {
    // Each setup's channel and codec, the status answered (2 ready, 1
    // failed) and the video codec passed on: H.264 always, H.265 only
    // when it was offered.
    let h264 = Some(VideoCodec::H264);
    let h265 = Some(VideoCodec::H265);
    for (offered, cases) in [
        (
            VideoCodec::H264,
            [
                (VIDEO, 3, 2, h264),
                (VIDEO, 7, 1, None),
                (MEDIA, 1, 2, None),
                (MEDIA, 3, 1, None),
            ],
        ),
        (
            VideoCodec::H265,
            [
                (VIDEO, 7, 2, h265),
                (VIDEO, 3, 2, h264),
                (MEDIA, 1, 2, None),
                (MEDIA, 7, 1, None),
            ],
        ),
    ] {
        let (tls, mut peer) = super::super::tls::tests::pair();
        let mut s = connected_session();
        s.config.codec = offered;
        s.tls = tls;
        s.opened[VIDEO as usize] = true;
        s.opened[MEDIA as usize] = true;
        let mut setup = p::SetupRequest::new();
        for (channel, codec, status, video) in cases {
            setup.set_media_codec_type(codec);
            let actions = s
                .message(channel, 0x8000, &setup.write_to_bytes().unwrap())
                .unwrap();
            let passed = actions.iter().find_map(|action| match action {
                Action::VideoCodec(codec) => Some(*codec),
                _ => None,
            });
            assert_eq!(passed, video, "{offered:?} offered, {codec} asked for");
            let mut parser = Parser::default();
            let frames = parser
                .feed(&s.outgoing.drain(..).flatten().collect::<Vec<_>>())
                .unwrap();
            for (index, frame) in frames.into_iter().enumerate() {
                let data = super::super::tls::tests::decode(&mut peer, &frame.data);
                if index == 0 {
                    assert_eq!(&data[..2], &0x8003u16.to_be_bytes());
                    let response = parse::<p::SetupResponse>(&data[2..]).unwrap();
                    assert_eq!(response.media_status(), status);
                    assert_eq!(response.configs, if status == 2 { vec![0] } else { vec![] });
                }
            }
        }
    }
}
#[test]
fn video_configuration_before_start_is_forwarded_without_invalid_ack() {
    let mut s = connected_session();
    s.opened[VIDEO as usize] = true;
    let config = [0, 0, 0, 1, 0x67, 1, 2, 3];
    assert!(
        matches!(s.message(VIDEO,1,&config).unwrap().as_slice(),[Action::Video(v)] if v==&config)
    );
    assert!(s.outgoing.is_empty());
    assert!(s.message(VIDEO, 0, &[0; 16]).is_err());
}
#[test]
fn discovery_asks_for_60_frames_a_second() {
    let config = AndroidAutoConfig::default();
    let bytes = discovery(&config, false, false, None)
        .write_to_bytes()
        .unwrap();
    let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
    let video = &decoded.channels[VIDEO as usize - 1]
        .av_channel
        .as_ref()
        .unwrap()
        .video_configs[0];
    assert_eq!(video.video_fps(), 1);
}
#[test]
fn a_4_3_ui_has_the_phone_draw_between_margins() {
    let decode = |config: &AndroidAutoConfig| {
        let bytes = discovery(config, false, false, None)
            .write_to_bytes()
            .unwrap();
        let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
        let video = decoded.channels[VIDEO as usize - 1]
            .av_channel
            .as_ref()
            .unwrap()
            .video_configs[0]
            .clone();
        let touch = decoded.channels[INPUT as usize - 1]
            .input_channel
            .as_ref()
            .unwrap()
            .touch_screen
            .as_ref()
            .unwrap()
            .clone();
        (video.margin_width(), touch.width(), touch.height())
    };
    let mut config = AndroidAutoConfig::default();
    assert_eq!(decode(&config), (0, 800, 480));
    config.margin_width = 160;
    assert_eq!(decode(&config), (160, 640, 480));
}
#[test]
fn discovery_names_the_codec_only_for_h265() {
    let codec = |codec| {
        let mut config = AndroidAutoConfig::default();
        config.codec = codec;
        let bytes = discovery(&config, false, false, None)
            .write_to_bytes()
            .unwrap();
        let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
        let channel = decoded.channels[VIDEO as usize - 1]
            .av_channel
            .clone()
            .unwrap();
        (
            channel.stream_type(),
            channel.video_configs[0].video_codec_type,
        )
    };
    assert_eq!(codec(VideoCodec::H264), (3, None));
    assert_eq!(codec(VideoCodec::H265), (3, Some(7)));
}
#[test]
fn the_cars_bluetooth_is_offered_only_with_an_address() {
    let bluetooth = |address| {
        let bytes = discovery(&AndroidAutoConfig::default(), false, false, address)
            .write_to_bytes()
            .unwrap();
        p::Discovery::parse_from_bytes(&bytes)
            .unwrap()
            .channels
            .into_iter()
            .find(|c| c.channel_id() == BLUETOOTH as u32)
    };
    assert!(bluetooth(None).is_none());
    let channel = bluetooth(Some("88:A2:9E:53:85:18")).unwrap();
    let offered = channel.bluetooth_channel.unwrap();
    assert_eq!(offered.adapter_address(), "88:A2:9E:53:85:18");
    assert_eq!(offered.supported_pairing_methods, [2]);
}
#[test]
fn the_phone_learns_whether_the_cars_bluetooth_knows_it() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    let mut open = p::Open::new();
    open.set_channel_id(BLUETOOTH as i32);
    let open = open.write_to_bytes().unwrap();
    // Not offered: neither opened nor taken.
    s.message(0, 7, &open).unwrap();
    assert!(!s.opened[BLUETOOTH as usize]);
    assert!(s.message(BLUETOOTH, 0x8001, &[]).is_err());
    // Offered, it opens, and the phone's request asks the car.
    s = s.with_bluetooth(Some("88:A2:9E:53:85:18".into()));
    s.message(0, 7, &open).unwrap();
    assert!(s.opened[BLUETOOTH as usize]);
    let mut request = p::BluetoothPairingRequest::new();
    request.set_phone_address("64:9D:38:E1:B6:24".into());
    request.set_pairing_method(2);
    let actions = s
        .message(BLUETOOTH, 0x8001, &request.write_to_bytes().unwrap())
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [Action::BluetoothPairing(phone)] if phone == "64:9D:38:E1:B6:24"
    ));
    sent(&mut s, &mut peer);
    s.bluetooth_paired(true).unwrap();
    let replies = sent(&mut s, &mut peer);
    let [(id, body)] = replies.as_slice() else {
        panic!("one reply expected, got {}", replies.len());
    };
    assert_eq!(*id, 0x8002);
    let reply = parse::<p::BluetoothPairingResponse>(body).unwrap();
    assert_eq!((reply.status(), reply.already_paired()), (0, true));
    // A channel number beyond a byte opens nothing.
    let mut beyond = p::Open::new();
    beyond.set_channel_id(256 + BLUETOOTH as i32);
    s.opened[BLUETOOTH as usize] = false;
    s.message(0, 7, &beyond.write_to_bytes().unwrap()).unwrap();
    assert!(!s.opened[BLUETOOTH as usize]);
}
#[test]
fn location_and_speed_are_offered_only_with_their_source() {
    // Night, parking brake and driving status always; location with a
    // receiver, speed with a receiver or while the I-Bus is read.
    for (gnss, speedometer, sensors) in [
        (false, false, vec![10, 7, 13]),
        (true, false, vec![10, 7, 13, 1, 3]),
        (false, true, vec![10, 7, 13, 3]),
        (true, true, vec![10, 7, 13, 1, 3]),
    ] {
        let bytes = discovery(&AndroidAutoConfig::default(), gnss, speedometer, None)
            .write_to_bytes()
            .unwrap();
        let decoded = p::Discovery::parse_from_bytes(&bytes).unwrap();
        let offered = decoded.channels[SENSOR as usize - 1]
            .sensor_channel
            .sensors
            .iter()
            .map(|s| s.type_())
            .collect::<Vec<_>>();
        assert_eq!(offered, sensors);
    }
}
#[test]
fn the_cars_speed_reaches_the_phone_and_decides_the_restrictions() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    // Moving at 88 km/h as the phone connects, with the I-Bus read.
    let mut s = connected_session().with_speedometer(true, Some(88));
    s.tls = tls;
    s.opened[SENSOR as usize] = true;
    let start = |kind: u32| {
        let mut r = p::SensorStart::new();
        r.set_sensor_type(kind);
        r.write_to_bytes().unwrap()
    };
    let events = |s: &mut Session, peer: &mut _| {
        sent(s, peer)
            .into_iter()
            .filter(|(id, _)| *id == 0x8003)
            .map(|(_, body)| parse::<p::SensorEvent>(&body).unwrap())
            .collect::<Vec<_>>()
    };
    let speed = |e: &p::SensorEvent| e.speed.first().map(|v| v.speed());
    let status = |e: &p::SensorEvent| e.driving_status.first().map(|v| v.status());
    // Both start; the speed answers at once, in mm/s, and moving restricts.
    s.message(SENSOR, 0x8001, &start(3)).unwrap();
    s.message(SENSOR, 0x8001, &start(13)).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(speed(&sent_now[0]), Some(24_444));
    assert_eq!(status(&sent_now[1]), Some(FULLY_RESTRICTED));
    // Each reading goes out; only a change of state sends the restrictions.
    s.vehicle_speed(Some(30)).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(sent_now.len(), 1);
    assert_eq!(speed(&sent_now[0]), Some(8_333));
    // 0 km/h is anything under 2: without the parking brake, still all.
    s.vehicle_speed(Some(0)).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(sent_now.len(), 1);
    assert_eq!(speed(&sent_now[0]), Some(0));
    // Standing with the parking brake on: none.
    s.parking_brake(true).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(status(&sent_now[0]), Some(UNRESTRICTED));
    // Moving off with the brake still on: all of them again.
    s.vehicle_speed(Some(2)).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(
        (speed(&sent_now[0]), status(&sent_now[1])),
        (Some(556), Some(FULLY_RESTRICTED))
    );
    // No speed any more, as with the ignition off: the brake alone decides.
    s.vehicle_speed(None).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(status(&sent_now[0]), Some(UNRESTRICTED));
    s.parking_brake(false).unwrap();
    let sent_now = events(&mut s, &mut peer);
    assert_eq!(status(&sent_now[0]), Some(FULLY_RESTRICTED));
    // With neither the I-Bus nor a receiver as the phone connected, a
    // start for speed is refused.
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut plain = connected_session();
    plain.tls = tls;
    plain.opened[SENSOR as usize] = true;
    plain.message(SENSOR, 0x8001, &start(3)).unwrap();
    let replies = sent(&mut plain, &mut peer);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, 0x8002);
    assert_ne!(parse::<p::Status>(&replies[0].1).unwrap().status(), 0);
}
#[test]
fn fixes_give_the_speed_and_the_i_bus_stands_in_without_them() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    // Moving at 88 km/h as the phone connects, with the I-Bus read and a
    // receiver connected.
    let mut s = connected_session().with_speedometer(true, Some(88));
    s.tls = tls;
    s.gnss = true;
    s.opened[SENSOR as usize] = true;
    let mut start = p::SensorStart::new();
    start.set_sensor_type(3);
    let speeds = |s: &mut Session, peer: &mut _| {
        sent(s, peer)
            .into_iter()
            .filter(|(id, _)| *id == 0x8003)
            .filter_map(|(_, body)| {
                let e = parse::<p::SensorEvent>(&body).unwrap();
                e.speed.first().map(|v| v.speed())
            })
            .collect::<Vec<_>>()
    };
    let fix = |speed| Fix {
        latitude: 48.5,
        longitude: -2.25,
        accuracy: None,
        altitude: None,
        speed,
        bearing: None,
    };
    // Before any fix, the I-Bus's.
    s.message(SENSOR, 0x8001, &start.write_to_bytes().unwrap())
        .unwrap();
    assert_eq!(speeds(&mut s, &mut peer), [24_444]);
    // A fix's speed goes out, and the I-Bus's waits while it is fresh.
    s.location(fix(Some(24.0))).unwrap();
    s.vehicle_speed(Some(86)).unwrap();
    assert_eq!(speeds(&mut s, &mut peer), [24_000]);
    // A fix without one changes nothing.
    s.location(fix(None)).unwrap();
    assert!(speeds(&mut s, &mut peer).is_empty());
    // Once fixes stop, the I-Bus's again.
    let stale = Instant::now().checked_sub(FRESH_FIX).unwrap();
    s.gnss_speed = Some((24.0, stale));
    s.vehicle_speed(Some(84)).unwrap();
    assert_eq!(speeds(&mut s, &mut peer), [23_333]);

    // With a receiver alone, the speed sensor starts and takes fixes.
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.gnss = true;
    s.opened[SENSOR as usize] = true;
    s.message(SENSOR, 0x8001, &start.write_to_bytes().unwrap())
        .unwrap();
    let replies = sent(&mut s, &mut peer);
    assert_eq!(replies.len(), 1);
    assert_eq!(parse::<p::Status>(&replies[0].1).unwrap().status(), 0);
    s.location(fix(Some(0.4))).unwrap();
    assert_eq!(speeds(&mut s, &mut peer), [400]);
}
#[test]
fn fixes_reach_the_phone_once_it_starts_the_location_sensor() {
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.gnss = true;
    s.opened[SENSOR as usize] = true;
    let fix = Fix {
        latitude: 48.5,
        longitude: -2.25,
        accuracy: Some(3.5),
        altitude: Some(117.0),
        speed: Some(12.5),
        bearing: Some(90.0),
    };
    // Before the phone asks, the fix is only kept.
    s.location(fix.clone()).unwrap();
    assert!(s.outgoing.is_empty());
    let start = |kind: u32| {
        let mut r = p::SensorStart::new();
        r.set_sensor_type(kind);
        r.write_to_bytes().unwrap()
    };
    // Night and location both start; the kept fix answers at once.
    s.message(SENSOR, 0x8001, &start(10)).unwrap();
    s.message(SENSOR, 0x8001, &start(1)).unwrap();
    let mut still = fix.clone();
    still.bearing = None;
    s.location(still).unwrap();
    let messages = sent(&mut s, &mut peer);
    let ids = messages.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    assert_eq!(ids, [0x8002, 0x8003, 0x8002, 0x8003, 0x8003]);
    let status = parse::<p::Status>(&messages[2].1).unwrap();
    assert_eq!(status.status(), 0);
    let first = parse::<p::SensorEvent>(&messages[3].1).unwrap();
    let l = &first.location[0];
    assert_eq!(
        (l.latitude(), l.longitude(), l.accuracy(), l.altitude()),
        (485_000_000, -22_500_000, 3_500, 11_700)
    );
    assert_eq!((l.speed(), l.bearing()), (12_500, 90_000_000));
    let second = parse::<p::SensorEvent>(&messages[4].1).unwrap();
    assert!(!second.location[0].has_bearing());

    // Without a receiver when the phone connected, location is refused.
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[SENSOR as usize] = true;
    s.message(SENSOR, 0x8001, &start(1)).unwrap();
    s.location(fix).unwrap();
    let messages = sent(&mut s, &mut peer);
    assert_eq!(messages.len(), 1);
    assert_eq!(parse::<p::Status>(&messages[0].1).unwrap().status(), 1);
}
#[test]
fn the_parking_brake_lifts_the_driving_restrictions() {
    let start = |kind: u32| {
        let mut r = p::SensorStart::new();
        r.set_sensor_type(kind);
        r.write_to_bytes().unwrap()
    };
    let events = |s: &mut Session,
                  peer: &mut openssl::ssl::SslStream<super::super::tls::Memory>|
     -> Vec<p::SensorEvent> {
        sent(s, peer)
            .into_iter()
            .filter(|(id, _)| *id == 0x8003)
            .map(|(_, body)| parse::<p::SensorEvent>(&body).unwrap())
            .collect()
    };
    let brake = |e: &p::SensorEvent| e.parking_brake.first().map(|b| b.parking_brake());
    let status = |e: &p::SensorEvent| e.driving_status.first().map(|d| d.status());

    // Until the brake is reported on, the car may be moving.
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[SENSOR as usize] = true;
    s.message(SENSOR, 0x8001, &start(7)).unwrap();
    s.message(SENSOR, 0x8001, &start(13)).unwrap();
    s.parking_brake(true).unwrap();
    s.parking_brake(false).unwrap();
    let sent = events(&mut s, &mut peer);
    assert_eq!(
        sent.iter().map(brake).collect::<Vec<_>>(),
        [Some(false), None, Some(true), None, Some(false), None]
    );
    assert_eq!(
        sent.iter().map(status).collect::<Vec<_>>(),
        [None, Some(31), None, Some(0), None, Some(31)]
    );

    // A session that starts with the brake on answers without restrictions.
    let (tls, mut peer) = super::super::tls::tests::pair();
    let mut s = connected_session();
    s.tls = tls;
    s.opened[SENSOR as usize] = true;
    s.parked = true;
    s.message(SENSOR, 0x8001, &start(13)).unwrap();
    let sent = events(&mut s, &mut peer);
    assert_eq!(sent.iter().map(status).collect::<Vec<_>>(), [Some(0)]);
}
#[test]
fn discovery_has_wired_channels_and_consistent_pcm_formats() {
    let d = discovery(&AndroidAutoConfig::default(), false, false, None);
    let bytes = d.write_to_bytes().unwrap();
    let parsed = parse::<p::Discovery>(&bytes).unwrap();
    assert_eq!(parsed.channels.len(), 7);
    assert!(parsed.channels.iter().all(|c| c.channel_id() <= 7));
    let v = &parsed.channels[2]
        .av_channel
        .as_ref()
        .unwrap()
        .video_configs[0];
    assert_eq!(v.video_resolution(), 1);
    assert_eq!(v.dpi(), 160);
    let input = parsed.channels[0].input_channel.as_ref().unwrap();
    let touch = input.touch_screen.as_ref().unwrap();
    assert_eq!((touch.width(), touch.height()), (800, 480));
    let keycodes = &input.supported_keycodes;
    // The phone only acts on keys the car said it has.
    for action in [
        PhoneAction::NextTrack,
        PhoneAction::PreviousTrack,
        PhoneAction::VoiceAssistant,
        PhoneAction::AnswerCall,
        PhoneAction::EndCall,
        PhoneAction::PhoneHome,
        PhoneAction::OpenMaps,
        PhoneAction::OpenMedia,
        PhoneAction::PlayPause,
    ] {
        assert!(keycodes.contains(&phone_key(action)), "{action:?}");
    }
    assert_eq!(phone_key(PhoneAction::PlayPause), 85);
    // The further keys, by the codes of Android and of Android Auto.
    for (action, code) in [
        (PhoneAction::PhoneMenu, 82),
        (PhoneAction::PhoneNotification, 83),
        (PhoneAction::PhoneSettings, 176),
        (PhoneAction::PhoneAppSwitch, 187),
        (PhoneAction::PhoneContacts, 207),
        (PhoneAction::PhoneCalendar, 208),
        (PhoneAction::PhoneMusic, 209),
        (PhoneAction::PhoneRadio, 65539),
        (PhoneAction::PhoneTel, 65540),
    ] {
        assert_eq!(phone_key(action), code, "{action:?}");
        assert!(keycodes.contains(&code), "{action:?}");
    }
}
