use crate::{render::*, state::*};
use navbridge_common::{AppSnapshot, NavigationKey, StreamFormat};

#[test]
fn reconnect_requires_a_fresh_snapshot_before_showing_projection() {
    let mut state = UiState {
        screen: Screen::Phone,
        core_connected: true,
        ..Default::default()
    };
    state.snapshot.projection.streaming = true;
    state.video_sequence.set(Some(42));
    state.held_navigation = Some(NavigationKey::Up);
    state.video = Some(VideoSettings {
        width: 800,
        height: 480,
    });
    assert!(state.show_projection());
    state.disconnect_projection();
    assert!(!state.show_projection());
    assert!(state.held_navigation.is_none());
    assert_eq!(state.video_sequence.get(), None);
    assert_eq!(state.video, None);
    state.core_connected = true;
    assert!(!state.show_projection());
    state.snapshot.projection.streaming = true;
    assert!(state.show_projection());
}

#[test]
fn system_page_shows_core_settings_only_while_connected() {
    let video = VideoSettings {
        width: 800,
        height: 480,
    };
    assert_eq!(
        projection_resolution(Some(video)),
        "AA 800x480 · CarPlay n/a"
    );
    assert_eq!(projection_resolution(None), "Unavailable");
    let stereo = Some(StreamFormat {
        channels: 2,
        rate: 44100,
    });
    assert_eq!(
        stream_text(stereo, true, true),
        ("Stereo".into(), "44.1 kHz".into())
    );
    let mono = Some(StreamFormat {
        channels: 1,
        rate: 16000,
    });
    assert_eq!(
        stream_text(mono, true, true),
        ("Mono".into(), "16 kHz".into())
    );
    assert_eq!(stream_text(None, true, true).1, "Idle");
    assert_eq!(stream_text(None, false, true).0, "n/a");
    assert_eq!(stream_text(stereo, true, false).0, "Unavailable");
    assert_eq!(core_value(true, "Speaker 100%"), "Speaker 100%");
    assert_eq!(core_value(true, " "), "n/a");
    assert_eq!(core_value(false, "Speaker 100%"), "Unavailable");
}

#[test]
fn usb_devices_fill_four_rows_by_name() {
    let mut snapshot = AppSnapshot::default();
    assert_eq!(usb_rows(&snapshot, true), ["Empty"; 4]);
    assert_eq!(usb_rows(&snapshot, false), ["Unavailable"; 4]);
    snapshot.system.usb_devices = vec!["USB audio CODEC".into(), "Arduino Micro".into()];
    assert_eq!(
        usb_rows(&snapshot, true),
        ["USB audio CODEC", "Arduino Micro", "Empty", "Empty"]
    );
    // More than the rows: the first ones show.
    snapshot.system.usb_devices = (1..=5).map(|n| format!("Device {n}")).collect();
    assert_eq!(usb_rows(&snapshot, true).len(), 4);
    assert_eq!(usb_rows(&snapshot, true)[3], "Device 4");
}
