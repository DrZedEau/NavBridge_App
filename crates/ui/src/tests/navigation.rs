use std::sync::mpsc;
use std::time::Instant;

use crate::config::KeyboardConfig;
use crate::input::{KeyAction, KeyMap};
use crate::{actions::*, projection::*, render::*, settings::*, state::*, LedState};
use navbridge_common::{AppSnapshot, ClientCommand, NavigationKey, TouchKind, VideoCodec};

fn connected_projection() -> UiState {
    let mut state = UiState {
        screen: Screen::Phone,
        core_connected: true,
        ..UiState::default()
    };
    state.snapshot.projection.phone_connected = true;
    state
}

#[test]
fn page_keys_set_day_and_night_once_per_press_on_every_screen() {
    let config: KeyboardConfig =
        toml::from_str(include_str!("../../../../config/keyboard.toml")).unwrap();
    let map = KeyMap::new(&config).unwrap();
    for (key, night) in [
        (slint::platform::Key::PageUp, false),
        (slint::platform::Key::PageDown, true),
    ] {
        let text: slint::SharedString = key.into();
        let action = map.action_for(text.as_str()).unwrap();
        assert_eq!(action, KeyAction::NightMode(night));
        for screen in [
            Screen::Menu,
            Screen::Phone,
            Screen::System,
            Screen::Settings,
        ] {
            let mut state = UiState {
                screen,
                ..UiState::default()
            };
            let (tx, rx) = mpsc::channel();
            handle_key_event(&mut state, action, true, false, &tx);
            handle_key_event(&mut state, action, true, true, &tx);
            handle_key_event(&mut state, action, false, false, &tx);
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientCommand::SetNightMode { enabled }) if enabled == night
            ));
            assert!(rx.try_recv().is_err());
            assert_eq!(state.screen, screen);
        }
    }
}

#[test]
fn control_and_alt_report_the_parking_brake_once_per_press_on_every_screen() {
    let config: KeyboardConfig =
        toml::from_str(include_str!("../../../../config/keyboard.toml")).unwrap();
    let map = KeyMap::new(&config).unwrap();
    for (key, engaged) in [
        (slint::platform::Key::Control, true),
        (slint::platform::Key::ControlR, true),
        (slint::platform::Key::Alt, false),
    ] {
        let text: slint::SharedString = key.into();
        let action = map.action_for(text.as_str()).unwrap();
        assert_eq!(action, KeyAction::ParkingBrake(engaged));
        for screen in [
            Screen::Menu,
            Screen::Phone,
            Screen::System,
            Screen::Settings,
        ] {
            let mut state = UiState {
                screen,
                ..UiState::default()
            };
            let (tx, rx) = mpsc::channel();
            handle_key_event(&mut state, action, true, false, &tx);
            handle_key_event(&mut state, action, true, true, &tx);
            handle_key_event(&mut state, action, false, false, &tx);
            assert!(matches!(
                rx.try_recv(),
                Ok(ClientCommand::SetParkingBrake { engaged: sent }) if sent == engaged
            ));
            assert!(rx.try_recv().is_err());
            assert_eq!(state.screen, screen);
        }
    }
}

#[test]
fn older_keyboard_files_get_the_mode_and_play_pause_keys() {
    let config: KeyboardConfig = toml::from_str(
        r#"
        [bindings]
        up = ["Up"]
        down = ["Down"]
        left = ["Left"]
        right = ["Right"]
        select = ["Return"]
        back = ["Backspace"]
        home = ["Escape"]
        quit = ["Q"]
        toggle_night_mode = ["Tab"]
        pause_music = ["P"]
        resume_music = ["S"]
        "#,
    )
    .unwrap();
    let map = KeyMap::new(&config).unwrap();
    let page_up: slint::SharedString = slint::platform::Key::PageUp.into();
    let tab: slint::SharedString = slint::platform::Key::Tab.into();
    assert_eq!(
        map.action_for(page_up.as_str()),
        Some(KeyAction::NightMode(false))
    );
    assert_eq!(
        map.action_for("p"),
        Some(KeyAction::Phone(navbridge_common::PhoneAction::PlayPause))
    );
    assert_eq!(map.action_for(tab.as_str()), None);
    assert_eq!(map.action_for("s"), None);
    let (control, alt): (slint::SharedString, slint::SharedString) = (
        slint::platform::Key::Control.into(),
        slint::platform::Key::Alt.into(),
    );
    assert_eq!(
        map.action_for(control.as_str()),
        Some(KeyAction::ParkingBrake(true))
    );
    assert_eq!(
        map.action_for(alt.as_str()),
        Some(KeyAction::ParkingBrake(false))
    );
}

#[test]
fn phone_shortcuts_send_once_and_only_in_connected_phone_view() {
    let config: KeyboardConfig =
        toml::from_str(include_str!("../../../../config/keyboard.toml")).unwrap();
    let map = KeyMap::new(&config).unwrap();
    for (letter, expected) in [
        ("n", navbridge_common::PhoneAction::NextTrack),
        ("b", navbridge_common::PhoneAction::PreviousTrack),
        ("v", navbridge_common::PhoneAction::VoiceAssistant),
        ("a", navbridge_common::PhoneAction::AnswerCall),
        ("e", navbridge_common::PhoneAction::EndCall),
        ("h", navbridge_common::PhoneAction::PhoneHome),
        ("m", navbridge_common::PhoneAction::OpenMaps),
        ("s", navbridge_common::PhoneAction::OpenMedia),
        ("p", navbridge_common::PhoneAction::PlayPause),
    ] {
        let action = map.action_for(letter).unwrap();
        assert_eq!(action, KeyAction::Phone(expected));
        assert_eq!(map.action_for(&letter.to_uppercase()), Some(action));
        let mut state = UiState::default();
        state.screen = Screen::Phone;
        state.core_connected = true;
        state.snapshot.projection.phone_connected = true;
        let (tx, rx) = std::sync::mpsc::channel();
        handle_key_event(&mut state, action, true, false, &tx);
        handle_key_event(&mut state, action, true, true, &tx);
        handle_key_event(&mut state, action, false, false, &tx);
        assert!(
            matches!(rx.try_recv(), Ok(ClientCommand::PhoneAction { action }) if action == expected)
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(state.screen, Screen::Phone);
        state.snapshot.projection.phone_connected = false;
        handle_key_event(&mut state, action, true, false, &tx);
        state.snapshot.projection.phone_connected = true;
        state.screen = Screen::System;
        handle_key_event(&mut state, action, true, false, &tx);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn projection_keys_forward_press_release_without_leaving_projection() {
    for (action, expected) in [
        (KeyAction::Up, NavigationKey::Up),
        (KeyAction::Down, NavigationKey::Down),
        (KeyAction::Left, NavigationKey::Left),
        (KeyAction::Right, NavigationKey::Right),
        (KeyAction::Select, NavigationKey::Select),
        (KeyAction::Back, NavigationKey::Back),
    ] {
        let (tx, rx) = mpsc::channel();
        let mut state = connected_projection();
        handle_key_event(&mut state, action, true, false, &tx);
        handle_key_event(&mut state, action, true, true, &tx);
        handle_key_event(&mut state, action, false, false, &tx);
        let commands: Vec<_> = rx.try_iter().collect();
        assert_eq!(commands.len(), 2);
        assert!(
            matches!(commands[0], ClientCommand::NavigationInput { key, pressed: true } if key == expected)
        );
        assert!(
            matches!(commands[1], ClientCommand::NavigationInput { key, pressed: false } if key == expected)
        );
        assert_eq!(state.screen, Screen::Phone);
    }
}

#[test]
fn home_releases_phone_key_hides_projection_and_returns_to_menu() {
    let (tx, rx) = mpsc::channel();
    let mut state = connected_projection();
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    handle_key_event(&mut state, KeyAction::Home, true, false, &tx);
    handle_key_event(&mut state, KeyAction::Select, false, false, &tx);
    assert_eq!(state.screen, Screen::Menu);
    assert!(state.projection_hidden_by_phone);
    let commands: Vec<_> = rx.try_iter().collect();
    assert_eq!(commands.len(), 3);
    assert!(matches!(
        commands[1],
        ClientCommand::NavigationInput { pressed: false, .. }
    ));
    assert!(matches!(commands[2], ClientCommand::HideProjection));
    // Already in the menu: nothing more to hide.
    handle_key_event(&mut state, KeyAction::Home, true, false, &tx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn menu_entry_release_does_not_click_phone_and_waiting_view_sends_nothing() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState::default();
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert_eq!(state.screen, Screen::Phone);
    state.core_connected = true;
    state.snapshot.projection.phone_connected = true;
    handle_key_event(&mut state, KeyAction::Select, false, false, &tx);
    assert!(
        rx.try_recv().is_err(),
        "the menu's Select release reached the phone"
    );
    // Until a phone connects, the projection view has nothing to act on.
    state.snapshot.projection.phone_connected = false;
    state.snapshot.usb.selected = Some(navbridge_common::UsbDeviceInfo::default());
    for action in [KeyAction::Select, KeyAction::Right, KeyAction::Home] {
        handle_key_event(&mut state, action, true, false, &tx);
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn system_round_trip_preserves_phone_session_and_keyboard_routing() {
    let (tx, rx) = mpsc::channel();
    let mut state = connected_projection();
    // Menu, left to System (the last tile), back, right to Phone.
    for action in [
        KeyAction::Home,
        KeyAction::Left,
        KeyAction::Select,
        KeyAction::Back,
        KeyAction::Right,
        KeyAction::Select,
    ] {
        handle_key_event(&mut state, action, true, false, &tx);
        handle_key_event(&mut state, action, false, false, &tx);
    }
    assert_eq!(state.screen, Screen::Phone);
    assert!(!state.projection_hidden_by_phone);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::HideProjection)));
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::ResumeProjection)));
    assert!(
        rx.try_recv().is_err(),
        "leaving and reopening a connected session only moves its video focus"
    );
    handle_key_event(&mut state, KeyAction::Right, true, false, &tx);
    handle_key_event(&mut state, KeyAction::Right, false, false, &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::NavigationInput {
            key: NavigationKey::Right,
            pressed: true
        })
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::NavigationInput {
            key: NavigationKey::Right,
            pressed: false
        })
    ));
}

#[test]
fn reopening_streaming_session_does_not_retry_connection() {
    let (tx, rx) = mpsc::channel();
    let mut state = connected_projection();
    state.screen = Screen::Menu;
    state.snapshot.projection.phone_connected = false;
    state.snapshot.projection.streaming = true;
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert_eq!(state.screen, Screen::Phone);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::ResumeProjection)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn menu_tiles_wrap_and_open_their_screens() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState::default();
    // From Phone, left wraps round to System.
    handle_key_event(&mut state, KeyAction::Left, true, false, &tx);
    assert_eq!(state.menu_index, 2);
    handle_key_event(&mut state, KeyAction::Right, true, false, &tx);
    assert_eq!(state.menu_index, 0);
    for (index, screen) in [
        (0, Screen::Phone),
        (1, Screen::Settings),
        (2, Screen::System),
    ] {
        state.screen = Screen::Menu;
        state.menu_index = index;
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        assert_eq!(state.screen, screen);
        // Back returns to the menu, here even from the phone's waiting view.
        handle_key_event(&mut state, KeyAction::Back, true, false, &tx);
        assert_eq!(state.screen, Screen::Menu);
    }
    assert!(rx.try_recv().is_err(), "no phone, nothing for the core");
    // System's four pages, the I-Bus last, wrap both ways.
    state.screen = Screen::System;
    state.system_page = 0;
    handle_key_event(&mut state, KeyAction::Left, true, false, &tx);
    assert_eq!(state.system_page, 3);
    handle_key_event(&mut state, KeyAction::Right, true, false, &tx);
    assert_eq!(state.system_page, 0);
}

#[test]
fn settings_rows_are_chosen_with_left_right_and_enter_opens_or_switches_them() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    let press = |state: &mut UiState, action, repeat| {
        handle_key_event(state, action, true, repeat, &tx);
    };
    // Left/Right only move between the rows, and wrap.
    press(&mut state, KeyAction::Right, false);
    assert_eq!(selected_setting(&state), Setting::DefaultPhone);
    press(&mut state, KeyAction::Left, false);
    press(&mut state, KeyAction::Left, false);
    assert_eq!(selected_setting(&state), Setting::Theme);
    press(&mut state, KeyAction::Right, false);
    assert_eq!(selected_setting(&state), Setting::Hotspot);
    assert!(rx.try_recv().is_err(), "choosing a row changed it");
    // Enter flips the hotspot's switch without opening its row, once
    // per press.
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Select, true);
    assert!(!state.settings_editing);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetWireless { enabled: true })
    ));
    assert!(rx.try_recv().is_err(), "a held key switched it twice");
    // Left/Right still move on to the next row.
    press(&mut state, KeyAction::Right, false);
    assert_eq!(selected_setting(&state), Setting::DefaultPhone);
    assert!(rx.try_recv().is_err());
    press(&mut state, KeyAction::Left, false);
    // A phone on USB does not stop it; a missing core does.
    state.snapshot.usb.selected = Some(navbridge_common::UsbDeviceInfo::default());
    press(&mut state, KeyAction::Select, false);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetWireless { enabled: false })
    ));
    state.core_connected = false;
    press(&mut state, KeyAction::Select, false);
    assert!(rx.try_recv().is_err());
    state.core_connected = true;
    // Enter opens the other rows and closes them again, then Back
    // returns to the menu.
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Select, false);
    assert!(state.settings_editing);
    press(&mut state, KeyAction::Select, false);
    assert!(!state.settings_editing);
    press(&mut state, KeyAction::Back, false);
    assert_eq!(state.screen, Screen::Menu);
    // Without the core, a row does not open.
    state.screen = Screen::Settings;
    state.core_connected = false;
    press(&mut state, KeyAction::Select, false);
    assert!(!state.settings_editing);
    assert!(rx.try_recv().is_err());
}

#[test]
fn open_rows_change_the_video_and_the_codec_levels() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    state.snapshot.projection.dpi = 160;
    state.snapshot.system.audio.volume_percent = Some(14);
    state.snapshot.system.audio.call_volume_db = Some(0);
    state.snapshot.system.audio.microphone_percent = Some(48);
    let press = |state: &mut UiState, action, repeat| {
        handle_key_event(state, action, true, repeat, &tx);
    };
    // The DPI switches between 160 and 140.
    state.settings_index = place(Setting::Dpi);
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Left, false);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetDpi { dpi: 140 })
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetDpi { dpi: 160 })
    ));
    // Back closes it too.
    press(&mut state, KeyAction::Back, false);
    assert_eq!(state.screen, Screen::Settings);
    // The codec, below the DPI, switches between H.264 and H.265; a held
    // key does not repeat.
    assert_eq!(
        setting_text(&state, Setting::Codec),
        ("Codec", "H.264".into())
    );
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetVideoCodec {
            codec: VideoCodec::H265
        })
    ));
    assert_eq!(
        setting_text(&state, Setting::Codec),
        ("Codec", "H.265".into())
    );
    assert!(!repeats(&state, KeyAction::Left));
    press(&mut state, KeyAction::Left, false);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetVideoCodec {
            codec: VideoCodec::H264
        })
    ));
    press(&mut state, KeyAction::Back, false);
    // Levels move in 1% steps; a held key repeats.
    state.settings_index = place(Setting::Volume);
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Right, true);
    press(&mut state, KeyAction::Left, false);
    for expected in [15, 16, 15] {
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientCommand::SetVolume { percent }) if percent == expected
        ));
    }
    // The call volume moves in 1 dB steps.
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Right, true);
    press(&mut state, KeyAction::Left, false);
    for expected in [1, 2, 1] {
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientCommand::SetCallVolume { db }) if db == expected
        ));
    }
    assert_eq!(setting_text(&state, Setting::CallVolume).1, "+1 dB");
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Right, false);
    press(&mut state, KeyAction::Select, false);
    press(&mut state, KeyAction::Down, false);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetMicrophone { percent: 47 })
    ));
    assert_eq!(
        (step(100, true), step(0, false), step(13, false)),
        (100, 0, 12)
    );
    // Nothing is sent past either end, or without the codec.
    state.snapshot.system.audio.microphone_percent = Some(0);
    press(&mut state, KeyAction::Down, false);
    press(&mut state, KeyAction::Down, true);
    assert!(rx.try_recv().is_err());
    assert_eq!(moved(Some(100), true), None);
    assert_eq!(
        (louder(Some(18), true), louder(Some(-12), false)),
        (None, None)
    );
    assert_eq!((decibels(0), decibels(-3)), ("0 dB".into(), "-3 dB".into()));
    state.snapshot.system.audio.microphone_percent = None;
    press(&mut state, KeyAction::Down, false);
    assert!(rx.try_recv().is_err());
    assert_eq!(setting_text(&state, Setting::Microphone).1, "n/a");
    // Nor while calls do not play in the car.
    state.snapshot.system.audio.call_volume_db = None;
    assert_eq!(louder(None, true), None);
    assert_eq!(setting_text(&state, Setting::CallVolume).1, "n/a");
    assert_eq!(setting_text(&state, Setting::Dpi).1, "160");
}

#[test]
fn the_phone_page_pairs_phones_in_wireless_mode() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Phone,
        core_connected: true,
        ..UiState::default()
    };
    // Wireless mode off: nothing to pair with.
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert!(rx.try_recv().is_err());
    // Enter switches pairing on, then off again before its time is up.
    state.snapshot.wireless.enabled = true;
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::StartPairing)));
    assert!(state.snapshot.bluetooth.pairing);
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::StopPairing)));
    assert!(!state.snapshot.bluetooth.pairing);
    // A held key switches it only once.
    handle_key_event(&mut state, KeyAction::Select, true, true, &tx);
    assert!(rx.try_recv().is_err());
    assert_eq!(state.screen, Screen::Phone);
    // What the page says meanwhile.
    assert_eq!(phone_message(&state), "Waiting for paired phone");
    state.snapshot.bluetooth.pairing = true;
    state.snapshot.bluetooth.name = "NavBridge-984B".into();
    assert_eq!(
        phone_message(&state),
        "On your phone, pair with NavBridge-984B"
    );
    state.snapshot.bluetooth.pairing_seconds = 95;
    assert_eq!(
        phone_message(&state),
        "On your phone, pair with NavBridge-984B\n1:35 left"
    );
    state.snapshot.wireless.enabled = false;
    assert_eq!(phone_message(&state), "Waiting for phone");

    // The phone's code is answered from any screen.
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    let request = navbridge_common::PairingRequest {
        device: "Pixel 7 Pro".into(),
        code: "042137".into(),
    };
    state.snapshot.bluetooth.confirm = Some(request.clone());
    handle_key_event(&mut state, KeyAction::Back, true, false, &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::ConfirmPairing { accept: false })
    ));
    assert_eq!(state.screen, Screen::Settings, "Back answered the code");
    state.snapshot.bluetooth.confirm = Some(request);
    state.screen = Screen::System;
    handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::ConfirmPairing { accept: true })
    ));
    assert_eq!(state.screen, Screen::System);
    assert!(rx.try_recv().is_err());
    assert_eq!(spaced("042137"), "042 137");
}

#[test]
fn taps_open_tiles_go_back_turn_pages_and_answer_pairing() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        core_connected: true,
        ..UiState::default()
    };
    tap_tile(&mut state, 2, &tx);
    assert_eq!(state.screen, Screen::System);
    swipe_system(&mut state, -1);
    assert_eq!(state.system_page, SYSTEM_PAGES - 1);
    swipe_system(&mut state, 1);
    assert_eq!(state.system_page, 0);
    tap_back(&mut state, &tx);
    assert_eq!(state.screen, Screen::Menu);
    // Pages only turn on System.
    swipe_system(&mut state, 1);
    assert_eq!(state.system_page, 0);
    tap_tile(&mut state, 0, &tx);
    assert_eq!(state.screen, Screen::Phone);
    state.snapshot.wireless.enabled = true;
    tap_pair(&mut state, &tx);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::StartPairing)));
    tap_back(&mut state, &tx);
    assert_eq!(state.screen, Screen::Menu);
    // While the phone's code shows, only its answers take taps.
    state.snapshot.bluetooth.confirm = Some(navbridge_common::PairingRequest {
        device: "Pixel 10".into(),
        code: "042137".into(),
    });
    tap_tile(&mut state, 1, &tx);
    tap_back(&mut state, &tx);
    assert_eq!(state.screen, Screen::Menu);
    assert!(rx.try_recv().is_err());
    answer_pairing(&mut state, false, &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::ConfirmPairing { accept: false })
    ));
    assert_eq!(state.screen, Screen::Menu);
    // With no code showing, an answer does nothing.
    answer_pairing(&mut state, true, &tx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn taps_switch_settings_and_step_an_open_level() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    state.snapshot.projection.dpi = 160;
    state.snapshot.system.audio.volume_percent = Some(50);
    // The hotspot, the DPI and the codec change with one tap.
    tap_setting(&mut state, place(Setting::Hotspot), 0, &tx);
    tap_setting(&mut state, place(Setting::Dpi), 0, &tx);
    tap_setting(&mut state, place(Setting::Codec), 0, &tx);
    let sent: Vec<_> = rx.try_iter().collect();
    assert!(matches!(
        sent[..],
        [
            ClientCommand::SetWireless { enabled: true },
            ClientCommand::SetDpi { dpi: 140 },
            ClientCommand::SetVideoCodec {
                codec: VideoCodec::H265
            }
        ]
    ));
    assert_eq!(selected_setting(&state), Setting::Codec);
    assert!(!state.settings_editing);
    // A level opens; its value's halves step it; its label closes it.
    let volume = place(Setting::Volume);
    tap_setting(&mut state, volume, 0, &tx);
    assert!(state.settings_editing && selected_setting(&state) == Setting::Volume);
    assert!(rx.try_recv().is_err());
    tap_setting(&mut state, volume, 1, &tx);
    tap_setting(&mut state, volume, 1, &tx);
    tap_setting(&mut state, volume, -1, &tx);
    for expected in [51, 52, 51] {
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientCommand::SetVolume { percent }) if percent == expected
        ));
    }
    // Another row's halves do nothing while it is closed.
    tap_setting(&mut state, place(Setting::Codec), 1, &tx);
    assert!(rx.try_recv().is_err());
    tap_setting(&mut state, volume, 0, &tx);
    assert!(!state.settings_editing);
    tap_setting(&mut state, volume, 1, &tx);
    assert!(rx.try_recv().is_err());
    // The microphone, the last row, is on screen with the others.
    tap_setting(&mut state, place(Setting::Microphone), 0, &tx);
    assert!(state.settings_editing && selected_setting(&state) == Setting::Microphone);
}

#[test]
fn wireless_rows_choose_the_default_phone_and_disconnect_it() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    state.snapshot.wireless.enabled = true;
    state.snapshot.bluetooth.phones = ["Pixel 10", "Pixel 8"]
        .into_iter()
        .zip(["64:9D:38:E1:B6:24", "A0:B1:C2:D3:E4:F5"])
        .map(|(name, address)| navbridge_common::BluetoothPhone {
            address: address.into(),
            name: name.into(),
        })
        .collect();
    let text = |state: &UiState, row| {
        let (label, value) = setting_text(state, row);
        format!("{label}: {value}")
    };
    // Any phone at first; a tap steps to each paired phone, then round.
    assert_eq!(text(&state, Setting::DefaultPhone), "Default phone: Any");
    assert_eq!(phone_message(&state), "Waiting for paired phone");
    let default = place(Setting::DefaultPhone);
    tap_setting(&mut state, default, 0, &tx);
    assert_eq!(
        text(&state, Setting::DefaultPhone),
        "Default phone: Pixel 10"
    );
    assert_eq!(phone_message(&state), "Waiting for Pixel 10");
    tap_setting(&mut state, default, 0, &tx);
    tap_setting(&mut state, default, 0, &tx);
    // Enter opens it, and Left steps back, from Any round to the last.
    let press = |state: &mut UiState, action| handle_key_event(state, action, true, false, &tx);
    press(&mut state, KeyAction::Select);
    assert!(state.settings_editing);
    press(&mut state, KeyAction::Left);
    press(&mut state, KeyAction::Select);
    let chosen: Vec<_> = rx
        .try_iter()
        .map(|command| match command {
            ClientCommand::SetDefaultPhone { address } => address,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    let (pixel_10, pixel_8) = (
        Some("64:9D:38:E1:B6:24".to_string()),
        Some("A0:B1:C2:D3:E4:F5".to_string()),
    );
    assert_eq!(chosen, [pixel_10, pixel_8.clone(), None, pixel_8]);
    // Without a wireless session, Disconnect has nothing to end.
    let disconnect = place(Setting::Disconnect);
    assert_eq!(text(&state, Setting::Disconnect), "Disconnect: n/a");
    tap_setting(&mut state, disconnect, 0, &tx);
    assert!(rx.try_recv().is_err());
    // With one, Enter ends it at once, without opening the row...
    state.snapshot.projection.wireless = true;
    state.snapshot.projection.phone_name = "Pixel 10".into();
    assert_eq!(text(&state, Setting::Disconnect), "Disconnect: Pixel 10");
    state.settings_index = disconnect;
    press(&mut state, KeyAction::Select);
    assert!(!state.settings_editing);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::DisconnectPhone)));
    // ...and then the row calls the default phone again, as a tap does.
    state.snapshot.projection.wireless = false;
    assert_eq!(text(&state, Setting::Disconnect), "Connect: Pixel 8");
    assert_eq!(
        phone_message(&state),
        "Disconnected\nConnect in Settings calls the phone again"
    );
    tap_setting(&mut state, disconnect, 0, &tx);
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::ConnectPhone)));
    assert_eq!(text(&state, Setting::Disconnect), "Disconnect: n/a");
    // Without the core, neither row does anything.
    state.core_connected = false;
    tap_setting(&mut state, default, 0, &tx);
    state.snapshot.bluetooth.paused = true;
    tap_setting(&mut state, disconnect, 0, &tx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_fill_four_boxes_in_the_keys_order() {
    let sections: Vec<_> = settings_sections()
        .into_iter()
        .map(|(title, places)| (title, places.map(|i| SETTINGS[i]).collect::<Vec<_>>()))
        .collect();
    use Setting::*;
    assert_eq!(
        sections,
        [
            ("WIRELESS", vec![Hotspot, DefaultPhone, Disconnect]),
            ("VIDEO", vec![Dpi, Codec]),
            ("AUDIO", vec![Volume, CallVolume, Microphone]),
            ("UI", vec![Theme]),
        ]
    );
}

#[test]
fn reverse_gear_covers_every_screen_and_takes_its_keys() {
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    state.snapshot.vehicle.reverse = Some(true);
    assert!(reversing(&state));
    let press = |state: &mut UiState, action| handle_action(state, action, &tx);
    // The screen beneath takes no keys...
    press(&mut state, KeyAction::Right);
    press(&mut state, KeyAction::Select);
    press(&mut state, KeyAction::Back);
    assert_eq!((state.screen, state.settings_index), (Screen::Settings, 0));
    assert!(rx.try_recv().is_err());
    // ...while day and night mode still switch from anywhere.
    press(&mut state, KeyAction::NightMode(true));
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientCommand::SetNightMode { enabled: true })
    ));
    // Out of reverse, or without the core to say so, the screen is back.
    state.snapshot.vehicle.reverse = Some(false);
    assert!(!reversing(&state));
    press(&mut state, KeyAction::Right);
    assert_eq!(state.settings_index, 1);
    state.snapshot.vehicle.reverse = Some(true);
    state.core_connected = false;
    assert!(!reversing(&state));
}

#[test]
fn the_theme_steps_through_amber_blue_and_green_and_outlasts_the_core() {
    use navbridge_common::Theme::{Amber, Blue, Green};
    let (tx, rx) = mpsc::channel();
    let mut state = UiState {
        screen: Screen::Settings,
        core_connected: true,
        ..UiState::default()
    };
    // In its own section, after the hotspot's.
    assert_eq!(section(Setting::Theme), "UI");
    assert_eq!(setting_text(&state, Setting::Theme).1, "Amber");
    tap_setting(&mut state, place(Setting::Theme), 0, &tx);
    assert_eq!(selected_setting(&state), Setting::Theme);
    assert!(!state.settings_editing, "a tap changes it at once");
    // Enter opens it; then Right steps on and Left back, round the ends.
    let press = |state: &mut UiState, action| handle_key_event(state, action, true, false, &tx);
    press(&mut state, KeyAction::Select);
    assert!(state.settings_editing);
    press(&mut state, KeyAction::Right);
    press(&mut state, KeyAction::Right);
    press(&mut state, KeyAction::Left);
    let chosen: Vec<_> = rx
        .try_iter()
        .map(|command| match command {
            ClientCommand::SetTheme { theme } => theme,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(chosen, [Blue, Green, Amber, Green]);
    assert_eq!(setting_text(&state, Setting::Theme).1, "Green");
    // The colours stay while the core is away.
    state.disconnect_projection();
    assert_eq!(state.snapshot.theme, Green);
}

#[test]
fn touches_on_the_projection_reach_the_phone_in_its_pixels() {
    let (tx, rx) = mpsc::channel();
    let mut state = connected_projection();
    let touch = |rx: &mpsc::Receiver<ClientCommand>| match rx.try_recv() {
        Ok(ClientCommand::Touch { kind, x, y }) => Some((kind, x, y)),
        _ => None,
    };
    // The 400x240 window's points, in Android Auto's 800x480.
    touch_projection(&mut state, 0, 200.0, 120.0, &tx);
    assert_eq!(touch(&rx), Some((TouchKind::Down, 400, 240)));
    // Moves come at most every TOUCH_MOVE_INTERVAL.
    touch_projection(&mut state, 1, 201.0, 121.0, &tx);
    assert_eq!(touch(&rx), None);
    state.touch_moved_at = Instant::now().checked_sub(TOUCH_MOVE_INTERVAL * 2);
    touch_projection(&mut state, 1, 210.0, 125.0, &tx);
    assert_eq!(touch(&rx), Some((TouchKind::Move, 420, 250)));
    // The release, past the edge, stays on it.
    touch_projection(&mut state, 2, 400.0, 250.0, &tx);
    assert_eq!(touch(&rx), Some((TouchKind::Up, 799, 479)));
    // Nothing held: no move or release.
    touch_projection(&mut state, 1, 10.0, 10.0, &tx);
    touch_projection(&mut state, 2, 10.0, 10.0, &tx);
    assert_eq!(touch(&rx), None);
    // A finger still down when Escape leaves is lifted first.
    touch_projection(&mut state, 0, 10.0, 10.0, &tx);
    assert_eq!(touch(&rx), Some((TouchKind::Down, 20, 20)));
    handle_key_event(&mut state, KeyAction::Home, true, false, &tx);
    assert_eq!(touch(&rx), Some((TouchKind::Up, 20, 20)));
    assert!(matches!(rx.try_recv(), Ok(ClientCommand::HideProjection)));
    // Without the projection on screen, touches go nowhere.
    touch_projection(&mut state, 0, 10.0, 10.0, &tx);
    assert!(rx.try_recv().is_err());
}

#[test]
fn status_lights_follow_bluetooth_the_hotspot_and_the_gnss_fix() {
    let mut snapshot = AppSnapshot::default();
    assert_eq!(
        status_lights(&snapshot),
        (LedState::Off, LedState::Off, LedState::Off)
    );
    snapshot.bluetooth.ready = true;
    snapshot.wireless.hotspot = true;
    // A receiver without a fix is orange.
    snapshot.gnss.receiver = "u-blox (ttyACM1)".into();
    assert_eq!(
        status_lights(&snapshot),
        (LedState::On, LedState::On, LedState::Waiting)
    );
    snapshot.gnss.position = "48.85837, 2.29448".into();
    assert_eq!(status_lights(&snapshot).2, LedState::On);
    let clock = clock_text();
    assert!(clock.len() == 5 && clock.as_bytes()[2] == b':', "{clock}");
}

#[test]
fn car_buttons_act_as_the_keyboard_bindings_of_the_same_name() {
    use navbridge_common::InputAction::*;
    let table: toml::Table =
        toml::from_str(include_str!("../../../../config/keyboard.toml")).unwrap();
    let bindings = table["bindings"].as_table().unwrap();
    for action in [
        Up,
        Down,
        Left,
        Right,
        Select,
        Back,
        Home,
        NormalMode,
        NightMode,
        ParkingBrakeOn,
        ParkingBrakeOff,
        NextTrack,
        PreviousTrack,
        VoiceAssistant,
        AnswerCall,
        EndCall,
        PhoneHome,
        OpenMaps,
        OpenMedia,
        PlayPause,
    ] {
        let name = serde_json::to_value(action).unwrap();
        let name = name.as_str().unwrap();
        assert!(bindings.contains_key(name), "{name} is not a key binding");
    }
    assert_eq!(KeyAction::from(Select), KeyAction::Select);
    assert_eq!(KeyAction::from(NightMode), KeyAction::NightMode(true));
    assert_eq!(
        KeyAction::from(ParkingBrakeOff),
        KeyAction::ParkingBrake(false)
    );
    assert_eq!(
        KeyAction::from(PlayPause),
        KeyAction::Phone(navbridge_common::PhoneAction::PlayPause)
    );
    // A car button acts as its key: the knob press opens the chosen tile.
    let (tx, _rx) = mpsc::channel();
    let mut state = UiState::default();
    handle_key_event(&mut state, Select.into(), true, false, &tx);
    assert_eq!(state.screen, Screen::Phone);
    // The keyboard acts while there is no I-Bus, as on a PC, and not
    // while the core reads it.
    assert!(keyboard_active(&state));
    state.core_connected = true;
    assert!(keyboard_active(&state));
    state.snapshot.vehicle.interface = true;
    assert!(!keyboard_active(&state));
}

#[test]
fn keyboard_config_keeps_backspace_in_phone_and_escape_in_host() {
    let config: KeyboardConfig =
        toml::from_str(include_str!("../../../../config/keyboard.toml")).unwrap();
    let keymap = KeyMap::new(&config).unwrap();
    for (key, action) in [
        (slint::platform::Key::Backspace, KeyAction::Back),
        (slint::platform::Key::Escape, KeyAction::Home),
        (slint::platform::Key::Home, KeyAction::Home),
        (slint::platform::Key::Return, KeyAction::Select),
    ] {
        let text: slint::SharedString = key.into();
        assert_eq!(keymap.action_for(text.as_str()), Some(action));
    }
}
