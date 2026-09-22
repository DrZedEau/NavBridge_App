mod config;
mod input;
mod ipc;

use std::cell::RefCell;
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use config::{KeyboardConfig, UiConfig};
use input::{KeyAction, KeyMap};
use ipc::IpcEvent;
use navbridge_common::{
    AppSnapshot, CarplayEvent, ClientCommand, NavigationKey, ServerEvent, MEDIA_HEIGHT,
    MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC, VIDEO_FRAME_PATH,
};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Menu = 0,
    Carplay = 1,
    System = 2,
    Ibus = 3,
}

struct UiState {
    screen: Screen,
    menu_index: usize,
    pairing_index: usize,
    system_page: usize,
    core_connected: bool,
    core_message: String,
    carplay_hidden_by_phone: bool,
    snapshot: AppSnapshot,
    held_navigation: Option<NavigationKey>,
    video_sequence: std::cell::Cell<Option<u64>>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Menu,
            menu_index: 0,
            pairing_index: 0,
            system_page: 0,
            core_connected: false,
            core_message: "core offline".into(),
            carplay_hidden_by_phone: false,
            snapshot: AppSnapshot::default(),
            held_navigation: None,
            video_sequence: std::cell::Cell::new(None),
        }
    }
}

impl UiState {
    fn disconnect_projection(&mut self) {
        self.core_connected = false;
        self.held_navigation = None;
        self.snapshot = AppSnapshot::default();
        self.carplay_hidden_by_phone = false;
        self.video_sequence.set(None);
    }
    fn show_projection(&self) -> bool {
        self.core_connected
            && self.screen == Screen::Carplay
            && (self.snapshot.carplay.phone_connected || self.snapshot.carplay.streaming)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .compact()
        .init();

    if let Err(err) = run() {
        eprintln!("navbridge-ui: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (ui_config_path, keyboard_path) = parse_args()?;
    let ui_config = UiConfig::load(&ui_config_path)?;
    let keyboard_config = KeyboardConfig::load(&keyboard_path)?;
    let keymap = Rc::new(KeyMap::new(&keyboard_config)?);

    let ui = AppWindow::new()?;
    ui.window().set_fullscreen(ui_config.fullscreen);

    let state = Rc::new(RefCell::new(UiState::default()));
    render_state(&ui, &state.borrow());

    let (command_tx, event_rx) = ipc::spawn(ui_config.socket_path.clone(), ui_config.reconnect_ms);

    {
        let state = Rc::clone(&state);
        let keymap = Rc::clone(&keymap);
        let command_tx = command_tx.clone();
        let weak = ui.as_weak();
        ui.on_key_event(move |text, pressed, repeat| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let text = text.to_string();
            let Some(action) = keymap.action_for(&text) else {
                return;
            };

            let mut state = state.borrow_mut();
            handle_key_event(&mut state, action, pressed, repeat, &command_tx);
            render_state(&ui, &state);
        });
    }

    // IPC runs on a worker thread. This tiny timer transfers already-parsed
    // state into Slint's main/UI thread without coupling the UI to backend code.
    let ipc_timer = Timer::default();
    {
        let state = Rc::clone(&state);
        let weak = ui.as_weak();
        ipc_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut changed = false;
            let mut state = state.borrow_mut();
            while let Ok(event) = event_rx.try_recv() {
                changed = true;
                match event {
                    IpcEvent::Connected => {
                        state.core_connected = true;
                        state.core_message = "core connected".into();
                    }
                    IpcEvent::Disconnected(reason) => {
                        state.disconnect_projection();
                        ui.set_video_active(false);
                        ui.set_carplay_frame(Image::default());
                        state.core_message = format!("core offline: {}", short(&reason, 28));
                    }
                    IpcEvent::Server(ServerEvent::Hello {
                        ui_width,
                        ui_height,
                        media_width,
                        media_height,
                        media_dpi,
                        ..
                    }) => {
                        // Reject an incompatible backend early: the frontend can
                        // be 400x240, while media stays 800x480 with configurable density.
                        if (ui_width, ui_height) != (UI_WIDTH, UI_HEIGHT)
                            || (media_width, media_height) != (MEDIA_WIDTH, MEDIA_HEIGHT)
                            || media_dpi == 0
                        {
                            state.core_connected = false;
                            state.core_message = "core media geometry mismatch".into();
                        }
                    }
                    IpcEvent::Server(ServerEvent::Snapshot { snapshot }) => {
                        let was_connected = state.snapshot.carplay.phone_connected;
                        let now_connected = snapshot.carplay.phone_connected;
                        state.snapshot = snapshot;

                        // Snapshot transition is a safety net for a UI that was
                        // started/reconnected after the real-time event occurred.
                        if now_connected && !was_connected && !state.carplay_hidden_by_phone {
                            state.screen = Screen::Carplay;
                        } else if was_connected && !now_connected {
                            state.carplay_hidden_by_phone = false;
                            if state.screen == Screen::Carplay {
                                state.screen = Screen::Menu;
                            }
                        }
                    }
                    IpcEvent::Server(ServerEvent::CarplayEvent { event }) => match event {
                        CarplayEvent::PhoneConnected => {
                            state.carplay_hidden_by_phone = false;
                            state.screen = Screen::Carplay;
                        }
                        CarplayEvent::StreamingReady => {
                            if !state.carplay_hidden_by_phone {
                                state.screen = Screen::Carplay;
                            }
                        }
                        CarplayEvent::PhoneDisconnected => {
                            state.carplay_hidden_by_phone = false;
                            state.screen = Screen::Menu;
                        }
                        CarplayEvent::RequestHostUi => {
                            state.carplay_hidden_by_phone = true;
                            state.screen = Screen::Menu;
                        }
                    },
                    IpcEvent::Server(ServerEvent::Error { message }) => {
                        state.core_message = format!("core error: {}", short(&message, 28));
                    }
                }
            }
            if changed {
                render_state(&ui, &state);
            }
        });
    }

    // Projection frames are exchanged out-of-band through tmpfs. The USB/backend
    // session remains 800x480; FFmpeg publishes an exact 400x240 RGBA view for
    // Slint so video never travels through the JSON control socket.
    let video_timer = Timer::default();
    {
        let state = Rc::clone(&state);
        let weak = ui.as_weak();
        video_timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            let show_projection = {
                let state = state.borrow();
                state.show_projection()
            };
            if !show_projection {
                ui.set_video_active(false);
                return;
            }

            let previous = state.borrow().video_sequence.get();
            // Open once: atomic replacement cannot mix a header from one frame
            // with pixels from another. Unchanged frames cost only 20 bytes.
            let result = std::fs::File::open(VIDEO_FRAME_PATH)
                .and_then(|mut file| read_projection(&mut file, previous));
            match result {
                Ok(Some((sequence, pixels))) => {
                    state.borrow().video_sequence.set(Some(sequence));
                    ui.set_carplay_frame(Image::from_rgba8(pixels));
                }
                Ok(None) => return,
                Err(_) => {
                    ui.set_video_active(false);
                    ui.set_carplay_frame(Image::default());
                    return;
                }
            }
            ui.set_video_active(true);
        });
    }

    ui.run()?;
    drop(video_timer);
    drop(ipc_timer);
    Ok(())
}

fn navigation_key(action: KeyAction) -> Option<NavigationKey> {
    match action {
        KeyAction::Up => Some(NavigationKey::Up),
        KeyAction::Down => Some(NavigationKey::Down),
        KeyAction::Left => Some(NavigationKey::Left),
        KeyAction::Right => Some(NavigationKey::Right),
        KeyAction::Select => Some(NavigationKey::Select),
        KeyAction::Back => Some(NavigationKey::Back),
        _ => None,
    }
}

fn release_navigation(state: &mut UiState, commands: &std::sync::mpsc::Sender<ClientCommand>) {
    if let Some(key) = state.held_navigation.take() {
        let _ = commands.send(ClientCommand::NavigationInput {
            key,
            pressed: false,
        });
    }
}

fn handle_key_event(
    state: &mut UiState,
    action: KeyAction,
    pressed: bool,
    repeat: bool,
    commands: &std::sync::mpsc::Sender<ClientCommand>,
) {
    if !pressed {
        if state.held_navigation.is_some() && state.held_navigation == navigation_key(action) {
            release_navigation(state, commands);
        }
        return;
    }
    if repeat && (state.screen != Screen::System || action == KeyAction::ToggleNightMode) {
        return;
    }
    handle_action(state, action, commands);
}

fn handle_action(
    state: &mut UiState,
    action: KeyAction,
    commands: &std::sync::mpsc::Sender<ClientCommand>,
) {
    if action == KeyAction::ToggleNightMode {
        let _ = commands.send(ClientCommand::ToggleNightMode);
        return;
    }
    if action == KeyAction::Quit {
        release_navigation(state, commands);
        let _ = slint::quit_event_loop();
        return;
    }
    if action == KeyAction::Home {
        release_navigation(state, commands);
        state.carplay_hidden_by_phone = true;
        state.screen = Screen::Menu;
        return;
    }

    match state.screen {
        Screen::Menu => match action {
            KeyAction::Up | KeyAction::Left => {
                state.menu_index = state.menu_index.checked_sub(1).unwrap_or(2)
            }
            KeyAction::Down | KeyAction::Right => state.menu_index = (state.menu_index + 1) % 3,
            KeyAction::Select => {
                state.screen = match state.menu_index {
                    0 => {
                        // Reopening projection is local navigation. Re-applying
                        // Android settings here can interrupt an active session.
                        state.carplay_hidden_by_phone = false;
                        if state.snapshot.carplay.phone_connected
                            || state.snapshot.carplay.streaming
                        {
                            let _ = commands.send(ClientCommand::ResumeCarplay);
                        } else {
                            let _ = commands.send(ClientCommand::StartCarplay);
                        }
                        Screen::Carplay
                    }
                    1 => {
                        state.system_page = 0;
                        Screen::System
                    }
                    _ => Screen::Ibus,
                }
            }
            _ => {}
        },
        Screen::Carplay => {
            if state.core_connected && state.snapshot.carplay.phone_connected {
                if let KeyAction::Phone(action) = action {
                    release_navigation(state, commands);
                    let _ = commands.send(ClientCommand::PhoneAction { action });
                } else if let Some(key) = navigation_key(action) {
                    release_navigation(state, commands);
                    let _ = commands.send(ClientCommand::NavigationInput { key, pressed: true });
                    state.held_navigation = Some(key);
                }
            } else if state.core_connected
                && state.snapshot.usb.selected.is_some()
                && !state.snapshot.carplay.streaming
            {
                match action {
                    KeyAction::Up | KeyAction::Down | KeyAction::Left | KeyAction::Right => {
                        state.pairing_index = 1 - state.pairing_index
                    }
                    KeyAction::Select => {
                        let _ = commands.send(if state.pairing_index == 0 {
                            ClientCommand::PairPhone
                        } else {
                            ClientCommand::StartCarplay
                        });
                    }
                    _ => (),
                }
            }
        }
        Screen::System => match action {
            KeyAction::Up | KeyAction::Left => state.system_page = (state.system_page + 2) % 3,
            KeyAction::Down | KeyAction::Right => state.system_page = (state.system_page + 1) % 3,
            KeyAction::Back => state.screen = Screen::Menu,
            _ => (),
        },
        Screen::Ibus => {
            if action == KeyAction::Back {
                state.screen = Screen::Menu;
            }
        }
    }
}

fn render_state(ui: &AppWindow, state: &UiState) {
    if ui.get_screen() != state.screen as i32 {
        ui.set_video_active(false);
        ui.set_carplay_frame(Image::default());
        let baseline = if state.screen == Screen::Carplay {
            std::fs::File::open(VIDEO_FRAME_PATH)
                .ok()
                .and_then(|mut file| read_frame_header(&mut file).ok())
        } else {
            None
        };
        state.video_sequence.set(baseline);
    }
    ui.set_screen(state.screen as i32);
    ui.set_menu_index(state.menu_index as i32);
    ui.set_pairing_index(state.pairing_index as i32);
    ui.set_system_page(state.system_page as i32);
    ui.set_pairing_available(
        state.core_connected
            && state.snapshot.usb.selected.is_some()
            && !state.snapshot.carplay.phone_connected
            && !state.snapshot.carplay.streaming,
    );
    let name = &state.snapshot.carplay.dongle_bluetooth_name;
    ui.set_pairing_hint(if name.is_empty() {
        "Enable Bluetooth and Wi-Fi on your phone".into()
    } else {
        format!("Bluetooth: {name}").into()
    });
    ui.set_core_connected(state.core_connected);
    ui.set_core_label(state.core_message.clone().into());

    let usb_label = state
        .snapshot
        .usb
        .selected
        .as_ref()
        .map(|usb| {
            format!(
                "USB {:04x}:{:04x}  bus {}:{}  {}",
                usb.vendor_id, usb.product_id, usb.bus, usb.address, usb.speed
            )
        })
        .or_else(|| {
            state
                .snapshot
                .usb
                .error
                .as_ref()
                .map(|e| format!("USB error: {e}"))
        })
        .unwrap_or_else(|| "No compatible dongle detected".into());
    ui.set_usb_label(short(&usb_label, 58).into());

    let carplay = &state.snapshot.carplay;
    ui.set_carplay_status(carplay.status.clone().into());
    ui.set_ibus_status(state.snapshot.ibus_status.clone().into());

    ui.set_gnss_receiver(
        if state.core_connected {
            available(&state.snapshot.gnss.receiver)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_status(
        if state.core_connected {
            available(&state.snapshot.gnss.status)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_satellites(
        if state.core_connected {
            available(&state.snapshot.gnss.satellites)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_position(
        if state.core_connected {
            available(&state.snapshot.gnss.position)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_altitude(
        if state.core_connected {
            available(&state.snapshot.gnss.altitude)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_speed(
        if state.core_connected {
            available(&state.snapshot.gnss.speed)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_hdop(
        if state.core_connected {
            available(&state.snapshot.gnss.hdop)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_gnss_utc(
        if state.core_connected {
            available(&state.snapshot.gnss.utc)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_cpu_usage(available(&state.snapshot.system.cpu_usage).into());
    ui.set_ram_usage(available(&state.snapshot.system.memory).into());
    ui.set_cpu_temperature(available(&state.snapshot.system.cpu_temp).into());
    let (status, model, firmware) = dongle_details(&state.snapshot, state.core_connected);
    ui.set_dongle_status(status.into());
    ui.set_dongle_model(model.into());
    ui.set_dongle_firmware(firmware.into());
    let connected = state.core_connected && (carplay.phone_connected || carplay.streaming);
    ui.set_connected_phone(
        if connected {
            known(&carplay.phone_name)
        } else {
            "Not connected".into()
        }
        .into(),
    );
    ui.set_connection_type(
        if connected {
            match carplay.connection_method.as_str() {
                "AndroidAuto" => "Android Auto".into(),
                "CarPlay" => "CarPlay".into(),
                _ => "Unknown".into(),
            }
        } else {
            "n/a".to_string()
        }
        .into(),
    );
    ui.set_connection_transport(
        if connected {
            known(&carplay.connection_transport)
        } else {
            "n/a".into()
        }
        .into(),
    );
    ui.set_audio_output_rate(
        if state.core_connected {
            available(&state.snapshot.system.audio_output_rate)
        } else {
            "Unavailable".into()
        }
        .into(),
    );
    ui.set_app_resolution(format!("{UI_WIDTH} x {UI_HEIGHT}").into());
    ui.set_phone_resolution(
        if connected && carplay.source_video_width > 0 && carplay.source_video_height > 0 {
            format!(
                "{} x {}",
                carplay.source_video_width, carplay.source_video_height
            )
        } else {
            "n/a".into()
        }
        .into(),
    );
}

fn known(value: &str) -> String {
    if value.trim().is_empty() {
        "Unknown".into()
    } else {
        value.into()
    }
}

fn available(value: &str) -> String {
    if value.trim().is_empty() {
        "n/a".into()
    } else {
        value.into()
    }
}

fn dongle_details(snapshot: &AppSnapshot, core_connected: bool) -> (String, String, String) {
    if !core_connected {
        return ("Unavailable".into(), "n/a".into(), "n/a".into());
    }
    let Some(device) = snapshot.usb.selected.as_ref() else {
        return ("Not detected".into(), "n/a".into(), "n/a".into());
    };
    let status = if snapshot.carplay.phone_connected {
        "Connected"
    } else {
        "Detected"
    };
    let model = if device.product.trim().is_empty() {
        format!("USB {:04x}:{:04x}", device.vendor_id, device.product_id)
    } else {
        device.product.clone()
    };
    (
        status.into(),
        model,
        available(&snapshot.carplay.dongle_firmware),
    )
}

fn read_frame_header(reader: &mut impl Read) -> io::Result<u64> {
    let mut header = [0u8; VIDEO_FRAME_HEADER_SIZE];
    reader.read_exact(&mut header)?;
    let width = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if header[..4] != VIDEO_FRAME_MAGIC || (width, height) != (UI_WIDTH, UI_HEIGHT) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid video header",
        ));
    }
    Ok(u64::from_le_bytes(header[12..20].try_into().unwrap()))
}

fn read_projection(
    reader: &mut impl Read,
    previous: Option<u64>,
) -> io::Result<Option<(u64, SharedPixelBuffer<Rgba8Pixel>)>> {
    let sequence = read_frame_header(reader)?;
    if previous == Some(sequence) {
        return Ok(None);
    }
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(UI_WIDTH, UI_HEIGHT);
    reader.read_exact(pixels.make_mut_bytes())?;
    if reader.read(&mut [0u8; 1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized video frame",
        ));
    }
    Ok(Some((sequence, pixels)))
}

fn short(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn parse_args() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let mut ui = PathBuf::from("config/frontend.toml");
    let mut keyboard = PathBuf::from("config/keyboard.toml");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => ui = PathBuf::from(args.next().ok_or("--config needs a path")?),
            "--keyboard" => keyboard = PathBuf::from(args.next().ok_or("--keyboard needs a path")?),
            "-h" | "--help" => {
                println!(
                    "NavBridge UI\n\nUsage: navbridge-ui [--config PATH] [--keyboard PATH]\n\nUI: 400x240; media session: 800x480; DPI configured in core.toml"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok((ui, keyboard))
}

#[cfg(test)]
mod navigation_tests {
    use super::*;
    use std::sync::mpsc;

    fn connected_projection() -> UiState {
        let mut state = UiState {
            screen: Screen::Carplay,
            core_connected: true,
            ..UiState::default()
        };
        state.snapshot.carplay.phone_connected = true;
        state
    }

    #[test]
    fn tab_toggles_once_per_press_on_every_screen() {
        let config: KeyboardConfig =
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
        let map = KeyMap::new(&config).unwrap();
        let tab: slint::SharedString = slint::platform::Key::Tab.into();
        let action = map.action_for(tab.as_str()).unwrap();
        assert_eq!(action, KeyAction::ToggleNightMode);
        for screen in [Screen::Menu, Screen::Carplay, Screen::System, Screen::Ibus] {
            let mut state = UiState {
                screen,
                ..UiState::default()
            };
            let (tx, rx) = mpsc::channel();
            handle_key_event(&mut state, action, true, false, &tx);
            handle_key_event(&mut state, action, true, true, &tx);
            handle_key_event(&mut state, action, false, false, &tx);
            assert!(matches!(rx.try_recv(), Ok(ClientCommand::ToggleNightMode)));
            assert!(rx.try_recv().is_err());
            assert_eq!(state.screen, screen);
        }
    }

    #[test]
    fn phone_shortcuts_send_once_and_only_in_connected_phone_view() {
        let config: KeyboardConfig =
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
        let map = KeyMap::new(&config).unwrap();
        for (letter, expected) in [
            ("n", navbridge_common::PhoneAction::NextTrack),
            ("b", navbridge_common::PhoneAction::PreviousTrack),
            ("v", navbridge_common::PhoneAction::VoiceAssistant),
            ("a", navbridge_common::PhoneAction::AnswerCall),
            ("h", navbridge_common::PhoneAction::PhoneHome),
            ("p", navbridge_common::PhoneAction::PauseMusic),
            ("s", navbridge_common::PhoneAction::ResumeMusic),
        ] {
            let action = map.action_for(letter).unwrap();
            assert_eq!(action, KeyAction::Phone(expected));
            assert_eq!(map.action_for(&letter.to_uppercase()), Some(action));
            let mut state = UiState::default();
            state.screen = Screen::Carplay;
            state.core_connected = true;
            state.snapshot.carplay.phone_connected = true;
            let (tx, rx) = std::sync::mpsc::channel();
            handle_key_event(&mut state, action, true, false, &tx);
            handle_key_event(&mut state, action, true, true, &tx);
            handle_key_event(&mut state, action, false, false, &tx);
            assert!(
                matches!(rx.try_recv(), Ok(ClientCommand::PhoneAction { action }) if action == expected)
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(state.screen, Screen::Carplay);
            state.snapshot.carplay.phone_connected = false;
            handle_key_event(&mut state, action, true, false, &tx);
            state.snapshot.carplay.phone_connected = true;
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
            assert_eq!(state.screen, Screen::Carplay);
        }
    }

    #[test]
    fn home_releases_phone_key_and_returns_to_menu() {
        let (tx, rx) = mpsc::channel();
        let mut state = connected_projection();
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        handle_key_event(&mut state, KeyAction::Home, true, false, &tx);
        handle_key_event(&mut state, KeyAction::Select, false, false, &tx);
        assert_eq!(state.screen, Screen::Menu);
        assert!(state.carplay_hidden_by_phone);
        let commands: Vec<_> = rx.try_iter().collect();
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            commands[1],
            ClientCommand::NavigationInput { pressed: false, .. }
        ));
    }

    #[test]
    fn menu_entry_release_does_not_click_phone_and_pairing_selection_works() {
        let (tx, rx) = mpsc::channel();
        let mut state = UiState::default();
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        state.core_connected = true;
        state.snapshot.carplay.phone_connected = true;
        handle_key_event(&mut state, KeyAction::Select, false, false, &tx);
        assert!(matches!(rx.try_recv(), Ok(ClientCommand::StartCarplay)));
        assert!(rx.try_recv().is_err());
        state.snapshot.carplay.phone_connected = false;
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        assert!(
            rx.try_recv().is_err(),
            "cannot pair without a detected dongle"
        );
        state.snapshot.usb.selected = Some(navbridge_common::UsbDeviceInfo::default());
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        handle_key_event(&mut state, KeyAction::Select, true, true, &tx);
        handle_key_event(&mut state, KeyAction::Select, false, false, &tx);
        assert!(matches!(rx.try_recv(), Ok(ClientCommand::PairPhone)));
        assert!(rx.try_recv().is_err());
        handle_key_event(&mut state, KeyAction::Right, true, false, &tx);
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        assert!(matches!(rx.try_recv(), Ok(ClientCommand::StartCarplay)));
    }

    #[test]
    fn system_round_trip_preserves_phone_session_and_keyboard_routing() {
        let (tx, rx) = mpsc::channel();
        let mut state = connected_projection();
        for action in [
            KeyAction::Home,
            KeyAction::Down,
            KeyAction::Select,
            KeyAction::Back,
            KeyAction::Up,
            KeyAction::Select,
        ] {
            handle_key_event(&mut state, action, true, false, &tx);
            handle_key_event(&mut state, action, false, false, &tx);
        }
        assert_eq!(state.screen, Screen::Carplay);
        assert!(!state.carplay_hidden_by_phone);
        assert!(matches!(rx.try_recv(), Ok(ClientCommand::ResumeCarplay)));
        assert!(
            rx.try_recv().is_err(),
            "reopening a connected session must not send StartCarplay"
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
        state.snapshot.carplay.phone_connected = false;
        state.snapshot.carplay.streaming = true;
        handle_key_event(&mut state, KeyAction::Select, true, false, &tx);
        assert_eq!(state.screen, Screen::Carplay);
        assert!(matches!(rx.try_recv(), Ok(ClientCommand::ResumeCarplay)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn keyboard_config_keeps_backspace_in_phone_and_escape_in_host() {
        let config: KeyboardConfig =
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
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
}

#[cfg(test)]
mod disconnect_tests {
    use super::*;
    #[test]
    fn reconnect_requires_a_fresh_snapshot_before_showing_projection() {
        let mut state = UiState {
            screen: Screen::Carplay,
            core_connected: true,
            ..Default::default()
        };
        state.snapshot.carplay.streaming = true;
        state.video_sequence.set(Some(42));
        state.held_navigation = Some(NavigationKey::Up);
        assert!(state.show_projection());
        state.disconnect_projection();
        assert!(!state.show_projection());
        assert!(state.held_navigation.is_none());
        assert_eq!(state.video_sequence.get(), None);
        state.core_connected = true;
        assert!(!state.show_projection());
        state.snapshot.carplay.streaming = true;
        assert!(state.show_projection());
    }
}

#[cfg(test)]
mod frame_read_tests {
    use super::*;
    fn header() -> Vec<u8> {
        [
            VIDEO_FRAME_MAGIC.to_vec(),
            UI_WIDTH.to_le_bytes().to_vec(),
            UI_HEIGHT.to_le_bytes().to_vec(),
            42u64.to_le_bytes().to_vec(),
        ]
        .concat()
    }
    #[test]
    fn unchanged_frame_does_not_read_or_allocate_pixels() {
        let mut reader = io::Cursor::new(header());
        assert!(read_projection(&mut reader, Some(42)).unwrap().is_none());
        assert_eq!(reader.position(), VIDEO_FRAME_HEADER_SIZE as u64);
    }
    #[test]
    fn new_frame_loads_directly_and_rejects_bad_lengths_and_geometry() {
        let mut data = header();
        assert!(read_projection(&mut &data[..], None).is_err());
        data.extend(vec![123; (UI_WIDTH * UI_HEIGHT * 4) as usize]);
        let (sequence, pixels) = read_projection(&mut &data[..], None).unwrap().unwrap();
        assert_eq!(sequence, 42);
        assert!(pixels.as_bytes().iter().all(|&b| b == 123));
        data.push(0);
        assert!(read_projection(&mut &data[..], None).is_err());
        data[4] ^= 1;
        assert!(read_projection(&mut &data[..], Some(42)).is_err());
    }
}
