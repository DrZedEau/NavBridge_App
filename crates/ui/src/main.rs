mod config;
mod frame_watch;
mod input;
mod ipc;

use std::cell::RefCell;
use std::env;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use config::{KeyboardConfig, UiConfig};
use input::{KeyAction, KeyMap};
use ipc::IpcEvent;
use navbridge_common::{
    AppSnapshot, ClientCommand, Ignition, NavigationKey, ProjectionEvent, ServerEvent,
    StreamFormat, VehicleSnapshot, MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH,
    VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC, VIDEO_FRAME_PATH,
};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Menu = 0,
    /// The connected phone's screen: Android Auto's projection, the only one
    /// in this build, or while there is none, how far a phone has got.
    Phone = 1,
    System = 2,
    Settings = 3,
}

/// The menu's tiles, left to right: Phone, Settings, System.
const MENU_ITEMS: usize = 3;
/// System pages: module and USB, video and audio, GNSS, and the car's I-Bus.
const SYSTEM_PAGES: usize = 4;
/// The projection DPIs and frame rates Settings offers.
const DPI_CHOICES: [u32; 2] = [160, 140];
const FPS_CHOICES: [u32; 2] = [60, 30];

/// Settings' rows, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    /// Wireless Android Auto: the hotspot, and Bluetooth for phones.
    Hotspot,
    Dpi,
    Fps,
    Volume,
    Microphone,
}
const SETTINGS: [Setting; 5] = [
    Setting::Hotspot,
    Setting::Dpi,
    Setting::Fps,
    Setting::Volume,
    Setting::Microphone,
];

/// The projection video's size, which the core asks the phone for, from its
/// hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoSettings {
    width: u32,
    height: u32,
}

struct UiState {
    screen: Screen,
    menu_index: usize,
    /// The chosen row in Settings, and whether Enter opened it to change it.
    settings_index: usize,
    settings_editing: bool,
    system_page: usize,
    core_connected: bool,
    projection_hidden_by_phone: bool,
    snapshot: AppSnapshot,
    video: Option<VideoSettings>,
    held_navigation: Option<NavigationKey>,
    video_sequence: std::cell::Cell<Option<u64>>,
    /// The size projection frames come in: the panel's 400x240, or 800x480
    /// for a UI drawn twice as large.
    frame_size: (u32, u32),
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Menu,
            menu_index: 0,
            settings_index: 0,
            settings_editing: false,
            system_page: 0,
            core_connected: false,
            projection_hidden_by_phone: false,
            snapshot: AppSnapshot::default(),
            video: None,
            held_navigation: None,
            video_sequence: std::cell::Cell::new(None),
            frame_size: (UI_WIDTH, UI_HEIGHT),
        }
    }
}

impl UiState {
    fn disconnect_projection(&mut self) {
        self.core_connected = false;
        self.held_navigation = None;
        self.snapshot = AppSnapshot::default();
        self.video = None;
        self.projection_hidden_by_phone = false;
        self.video_sequence.set(None);
    }
    fn show_projection(&self) -> bool {
        self.core_connected
            && self.screen == Screen::Phone
            && (self.snapshot.projection.phone_connected || self.snapshot.projection.streaming)
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

    // Twice as large: the same 400x240 layout drawn at 800x480. The window
    // reads this as it opens, before any other thread runs.
    if ui_config.scale > 1 {
        env::set_var("SLINT_SCALE_FACTOR", ui_config.scale.to_string());
    }
    let frame_size = (UI_WIDTH * ui_config.scale, UI_HEIGHT * ui_config.scale);

    let ui = AppWindow::new()?;
    ui.window().set_fullscreen(ui_config.fullscreen);
    ui.set_double_size(ui_config.scale > 1);

    let state = Rc::new(RefCell::new(UiState {
        frame_size,
        ..UiState::default()
    }));
    render_state(&ui, &state.borrow());

    // The menu's clock, checked every second and redrawn when the minute changes.
    ui.set_clock(clock_text().into());
    let clock = slint::Timer::default();
    {
        let weak = ui.as_weak();
        clock.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let now = clock_text();
                    if ui.get_clock() != now.as_str() {
                        ui.set_clock(now.into());
                    }
                }
            },
        );
    }

    let weak = ui.as_weak();
    let (command_tx, event_rx) = ipc::spawn(
        ui_config.socket_path.clone(),
        ui_config.reconnect_ms,
        frame_size,
        move || {
            let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_core_message());
        },
    );

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

    // IPC runs on a worker thread, which wakes this handler for each message
    // from the core to apply it on Slint's main/UI thread.
    {
        let state = Rc::clone(&state);
        let weak = ui.as_weak();
        ui.on_core_message(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut changed = false;
            let mut state = state.borrow_mut();
            while let Ok(event) = event_rx.try_recv() {
                changed = true;
                match event {
                    IpcEvent::Connected => state.core_connected = true,
                    IpcEvent::Disconnected(reason) => {
                        tracing::debug!(%reason, "core offline");
                        state.disconnect_projection();
                        ui.set_video_active(false);
                        ui.set_projection_frame(Image::default());
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
                            tracing::warn!("core media geometry mismatch");
                            state.core_connected = false;
                        } else {
                            state.video = Some(VideoSettings {
                                width: media_width,
                                height: media_height,
                            });
                        }
                    }
                    IpcEvent::Server(ServerEvent::Snapshot { snapshot }) => {
                        let was_connected = state.snapshot.projection.phone_connected;
                        let now_connected = snapshot.projection.phone_connected;
                        state.snapshot = snapshot;

                        // Snapshot transition is a safety net for a UI that was
                        // started/reconnected after the real-time event occurred.
                        if now_connected && !was_connected && !state.projection_hidden_by_phone {
                            state.screen = Screen::Phone;
                        } else if was_connected && !now_connected {
                            state.projection_hidden_by_phone = false;
                            if state.screen == Screen::Phone {
                                state.screen = Screen::Menu;
                            }
                        }
                    }
                    IpcEvent::Server(ServerEvent::ProjectionEvent { event }) => match event {
                        ProjectionEvent::PhoneConnected => {
                            state.projection_hidden_by_phone = false;
                            state.screen = Screen::Phone;
                        }
                        ProjectionEvent::StreamingReady => {
                            if !state.projection_hidden_by_phone {
                                state.screen = Screen::Phone;
                            }
                        }
                        ProjectionEvent::PhoneDisconnected => {
                            state.projection_hidden_by_phone = false;
                            state.screen = Screen::Menu;
                        }
                        ProjectionEvent::RequestHostUi => {
                            state.projection_hidden_by_phone = true;
                            state.screen = Screen::Menu;
                        }
                    },
                }
            }
            if changed {
                render_state(&ui, &state);
            }
        });
    }

    // Projection frames are exchanged out-of-band through tmpfs. The USB/backend
    // session remains 800x480; FFmpeg publishes an exact 400x240 RGBA view for
    // Slint so video never travels through the JSON control socket. Each frame
    // is loaded as soon as the core publishes it; LinuxKMS shows it at the
    // next vblank.
    {
        let state = Rc::clone(&state);
        let weak = ui.as_weak();
        let frame_stats = env::var_os("NAVBRIDGE_FRAME_STATS").is_some();
        let mut sample_start = Instant::now();
        let mut sample_frames = 0u64;
        ui.on_frame_ready(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !refresh_projection(&ui, &state.borrow()) || !frame_stats {
                return;
            }
            sample_frames += 1;
            let seconds = sample_start.elapsed().as_secs_f64();
            if seconds >= 5.0 {
                tracing::info!(
                    fps = sample_frames as f64 / seconds,
                    frames = sample_frames,
                    seconds,
                    "projection UI frame updates"
                );
                sample_start = Instant::now();
                sample_frames = 0;
            }
        });
    }
    let weak = ui.as_weak();
    frame_watch::spawn(Path::new(VIDEO_FRAME_PATH), move || {
        let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_frame_ready());
    })?;

    ui.run()?;
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
    if repeat && !repeats(state, action) {
        return;
    }
    handle_action(state, action, commands);
}

fn handle_action(
    state: &mut UiState,
    action: KeyAction,
    commands: &std::sync::mpsc::Sender<ClientCommand>,
) {
    if let KeyAction::NightMode(enabled) = action {
        let _ = commands.send(ClientCommand::SetNightMode { enabled });
        return;
    }
    if let KeyAction::ParkingBrake(engaged) = action {
        let _ = commands.send(ClientCommand::SetParkingBrake { engaged });
        return;
    }
    if action == KeyAction::Quit {
        release_navigation(state, commands);
        let _ = slint::quit_event_loop();
        return;
    }
    // The pairing code shows over any screen and takes Enter and Back.
    if state.snapshot.bluetooth.confirm.is_some()
        && matches!(action, KeyAction::Select | KeyAction::Back)
    {
        let accept = action == KeyAction::Select;
        let _ = commands.send(ClientCommand::ConfirmPairing { accept });
        state.snapshot.bluetooth.confirm = None;
        return;
    }
    if action == KeyAction::Home {
        release_navigation(state, commands);
        // As when the phone asks for the car's screen: it can stop its video.
        if state.screen == Screen::Phone
            && (state.snapshot.projection.phone_connected || state.snapshot.projection.streaming)
        {
            let _ = commands.send(ClientCommand::HideProjection);
        }
        state.projection_hidden_by_phone = true;
        state.screen = Screen::Menu;
        return;
    }

    match state.screen {
        Screen::Menu => match action {
            KeyAction::Up | KeyAction::Left => {
                state.menu_index = (state.menu_index + MENU_ITEMS - 1) % MENU_ITEMS
            }
            KeyAction::Down | KeyAction::Right => {
                state.menu_index = (state.menu_index + 1) % MENU_ITEMS
            }
            KeyAction::Select => {
                state.screen = match state.menu_index {
                    0 => {
                        // Reopening projection only gives the phone its video
                        // focus back; the session itself keeps running.
                        state.projection_hidden_by_phone = false;
                        if state.snapshot.projection.phone_connected
                            || state.snapshot.projection.streaming
                        {
                            let _ = commands.send(ClientCommand::ResumeProjection);
                        }
                        Screen::Phone
                    }
                    1 => {
                        state.settings_index = 0;
                        state.settings_editing = false;
                        Screen::Settings
                    }
                    _ => {
                        state.system_page = 0;
                        Screen::System
                    }
                }
            }
            _ => {}
        },
        Screen::Phone => {
            if state.core_connected && state.snapshot.projection.phone_connected {
                if let KeyAction::Phone(action) = action {
                    release_navigation(state, commands);
                    let _ = commands.send(ClientCommand::PhoneAction { action });
                } else if let Some(key) = navigation_key(action) {
                    release_navigation(state, commands);
                    let _ = commands.send(ClientCommand::NavigationInput { key, pressed: true });
                    state.held_navigation = Some(key);
                }
            } else {
                waiting_page(state, action, commands);
            }
        }
        Screen::System => match action {
            KeyAction::Up | KeyAction::Left => {
                state.system_page = (state.system_page + SYSTEM_PAGES - 1) % SYSTEM_PAGES
            }
            KeyAction::Down | KeyAction::Right => {
                state.system_page = (state.system_page + 1) % SYSTEM_PAGES
            }
            KeyAction::Back => state.screen = Screen::Menu,
            _ => (),
        },
        // Left/Right choose a row and Enter opens it; then Left/Right change
        // its value and Enter or Back close it again. Enter flips the hotspot's
        // switch at once.
        Screen::Settings => {
            let rows = SETTINGS.len();
            let index = settings_index(state);
            match (state.settings_editing, action) {
                (false, KeyAction::Left | KeyAction::Up) => {
                    state.settings_index = (index + rows - 1) % rows
                }
                (false, KeyAction::Right | KeyAction::Down) => {
                    state.settings_index = (index + 1) % rows
                }
                (false, KeyAction::Select) if state.core_connected => {
                    if selected_setting(state) == Setting::Hotspot {
                        change_setting(state, true, commands)
                    } else {
                        state.settings_editing = true
                    }
                }
                (false, KeyAction::Back) => state.screen = Screen::Menu,
                (true, KeyAction::Right | KeyAction::Up) => change_setting(state, true, commands),
                (true, KeyAction::Left | KeyAction::Down) => change_setting(state, false, commands),
                (true, KeyAction::Select | KeyAction::Back) => state.settings_editing = false,
                _ => (),
            }
        }
    }
}

fn render_state(ui: &AppWindow, state: &UiState) {
    if ui.get_screen() != state.screen as i32 {
        ui.set_video_active(false);
        ui.set_projection_frame(Image::default());
        let baseline = if state.screen == Screen::Phone {
            std::fs::File::open(VIDEO_FRAME_PATH)
                .ok()
                .and_then(|mut file| read_frame_header(&mut file, state.frame_size).ok())
        } else {
            None
        };
        state.video_sequence.set(baseline);
    }
    ui.set_screen(state.screen as i32);
    ui.set_menu_index(state.menu_index as i32);
    ui.set_system_page(state.system_page as i32);
    ui.set_core_connected(state.core_connected);
    let (bt, wifi, gnss) = status_lights(&state.snapshot);
    ui.set_bt_led(bt);
    ui.set_wifi_led(wifi);
    ui.set_gnss_led(gnss);

    ui.set_phone_message(phone_message(state).into());

    let gnss = &state.snapshot.gnss;
    let core =
        |value: &str| -> slint::SharedString { core_value(state.core_connected, value).into() };
    ui.set_gnss_receiver(core(&gnss.receiver));
    ui.set_gnss_status(core(&gnss.status));
    ui.set_gnss_satellites(core(&gnss.satellites));
    ui.set_gnss_position(core(&gnss.position));
    ui.set_gnss_altitude(core(&gnss.altitude));
    ui.set_gnss_speed(core(&gnss.speed));
    ui.set_gnss_hdop(core(&gnss.hdop));
    ui.set_gnss_utc(core(&gnss.utc));
    ui.set_cpu_usage(available(&state.snapshot.system.cpu_usage).into());
    ui.set_ram_usage(available(&state.snapshot.system.memory).into());
    ui.set_cpu_temperature(available(&state.snapshot.system.cpu_temp).into());
    let (phone, gnss, ibus) = usb_devices(&state.snapshot, state.core_connected);
    ui.set_usb_phone(phone.into());
    ui.set_usb_gnss(gnss.into());
    ui.set_usb_ibus(ibus.into());
    // On the panel, the window takes the display mode; until it is shown, the
    // size this UI is designed for.
    let size = ui.window().size();
    let (width, height) = if size.width > 0 && size.height > 0 {
        (size.width, size.height)
    } else {
        (UI_WIDTH, UI_HEIGHT)
    };
    ui.set_ui_resolution(format!("{width}x{height}").into());
    ui.set_projection_resolution(projection_resolution(state.video).into());
    ui.set_projection_decoder(core(&state.snapshot.projection.decoder));
    let audio = &state.snapshot.system.audio;
    ui.set_audio_codec(core(&audio.codec));
    let present = state.core_connected && audio.codec != "Not detected";
    let (output_type, output_rate) = stream_text(audio.output, present, state.core_connected);
    ui.set_audio_output_type(output_type.into());
    ui.set_audio_output_rate(output_rate.into());
    let (input_type, input_rate) = stream_text(audio.input, present, state.core_connected);
    ui.set_audio_input_type(input_type.into());
    ui.set_audio_input_rate(input_rate.into());
    let rows: Vec<SettingsRow> = SETTINGS
        .into_iter()
        .map(|row| {
            let (label, value) = setting_text(state, row);
            SettingsRow {
                section: section(row).into(),
                label: label.into(),
                value: value.into(),
                toggle: row == Setting::Hotspot,
                on: state.snapshot.wireless.enabled,
            }
        })
        .collect();
    ui.set_settings_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    ui.set_settings_index(settings_index(state) as i32);
    ui.set_settings_editing(state.settings_editing);
    ui.set_pair_option(pair_option(state));
    ui.set_pairing(state.snapshot.bluetooth.pairing);
    let pairing = state.snapshot.bluetooth.confirm.as_ref();
    ui.set_pairing_request(pairing.is_some());
    if let Some(request) = pairing {
        ui.set_pairing_device(request.device.clone().into());
        ui.set_pairing_code(spaced(&request.code).into());
    }
    let vehicle = &state.snapshot.vehicle;
    ui.set_vehicle_status(vehicle_status(vehicle, state.core_connected).into());
    ui.set_vehicle_speed(reading(vehicle.speed_kmh, "km/h").into());
    ui.set_vehicle_gnss_speed(gnss_speed(&state.snapshot.gnss.speed).into());
    ui.set_vehicle_rpm(reading(vehicle.rpm, "rpm").into());
    ui.set_vehicle_coolant(reading(vehicle.coolant_c, "°C").into());
    ui.set_vehicle_outside(reading(vehicle.outside_c, "°C").into());
    ui.set_vehicle_ignition(ignition_text(vehicle.ignition).into());
    ui.set_vehicle_odometer(odometer(vehicle.odometer_km).into());
    // Frames that arrive while projection cannot be shown are skipped; pick up
    // the newest one as soon as it can be, without waiting for the phone.
    refresh_projection(ui, state);
}

/// Shows the newest projection frame if the UI has not shown it yet. Returns
/// whether a new frame went on screen.
fn refresh_projection(ui: &AppWindow, state: &UiState) -> bool {
    if !state.show_projection() {
        ui.set_video_active(false);
        return false;
    }
    let previous = state.video_sequence.get();
    // Open once: atomic replacement cannot mix a header from one frame
    // with pixels from another. Unchanged frames cost only 20 bytes.
    let result = std::fs::File::open(VIDEO_FRAME_PATH)
        .and_then(|mut file| read_projection(&mut file, previous, state.frame_size));
    match result {
        Ok(Some((sequence, pixels))) => {
            state.video_sequence.set(Some(sequence));
            ui.set_projection_frame(Image::from_rgba8(pixels));
            ui.set_video_active(true);
            true
        }
        Ok(None) => false,
        Err(_) => {
            ui.set_video_active(false);
            ui.set_projection_frame(Image::default());
            false
        }
    }
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

/// With wireless mode on, the Phone page offers pairing a phone.
fn pair_option(state: &UiState) -> bool {
    state.core_connected && state.snapshot.wireless.enabled
}

/// The Phone page's message until a phone connects. With the hotspot on,
/// paired phones are called, except while pairing is open: then, how to pair.
fn phone_message(state: &UiState) -> String {
    let bluetooth = &state.snapshot.bluetooth;
    if !pair_option(state) {
        return "Waiting for phone".into();
    }
    if !bluetooth.pairing {
        return "Waiting for paired phone".into();
    }
    let name = if bluetooth.name.is_empty() {
        "NavBridge"
    } else {
        &bluetooth.name
    };
    // Until the core's snapshot confirms it, the time left is not known yet.
    match bluetooth.pairing_seconds {
        0 => format!("On your phone, pair with {name}"),
        left => format!(
            "On your phone, pair with {name}\n{}:{:02} left",
            left / 60,
            left % 60
        ),
    }
}

/// The Phone page while there is no phone: in wireless mode, Enter switches
/// pairing on or off, and Back returns to the menu.
fn waiting_page(
    state: &mut UiState,
    action: KeyAction,
    commands: &std::sync::mpsc::Sender<ClientCommand>,
) {
    match action {
        KeyAction::Select if pair_option(state) => {
            // Shown at once; the core's snapshot switches it off again when
            // the time for pairing is up.
            let pairing = &mut state.snapshot.bluetooth.pairing;
            let _ = commands.send(if *pairing {
                ClientCommand::StopPairing
            } else {
                ClientCommand::StartPairing
            });
            *pairing = !*pairing;
        }
        KeyAction::Back => state.screen = Screen::Menu,
        _ => (),
    }
}

fn settings_index(state: &UiState) -> usize {
    state.settings_index.min(SETTINGS.len() - 1)
}

fn selected_setting(state: &UiState) -> Setting {
    SETTINGS[settings_index(state)]
}

/// Changes the open row's value, up or down: the hotspot, the DPI and the
/// frame rate switch, the levels move by 1%.
fn change_setting(
    state: &mut UiState,
    up: bool,
    commands: &std::sync::mpsc::Sender<ClientCommand>,
) {
    if !state.core_connected {
        return;
    }
    let setting = selected_setting(state);
    let snapshot = &mut state.snapshot;
    let audio = &mut snapshot.system.audio;
    // Each change shows at once; the core's next snapshot confirms it.
    match setting {
        Setting::Hotspot => {
            let enabled = !snapshot.wireless.enabled;
            let _ = commands.send(ClientCommand::SetWireless { enabled });
            snapshot.wireless.enabled = enabled;
        }
        Setting::Dpi => {
            let dpi = match snapshot.projection.dpi {
                dpi if dpi == DPI_CHOICES[0] => DPI_CHOICES[1],
                _ => DPI_CHOICES[0],
            };
            let _ = commands.send(ClientCommand::SetDpi { dpi });
            snapshot.projection.dpi = dpi;
        }
        Setting::Fps => {
            let fps = match snapshot.projection.fps {
                fps if fps == FPS_CHOICES[0] => FPS_CHOICES[1],
                _ => FPS_CHOICES[0],
            };
            let _ = commands.send(ClientCommand::SetFps { fps });
            snapshot.projection.fps = fps;
        }
        // At either end, a held key sends nothing more.
        Setting::Volume => {
            if let Some(percent) = moved(audio.volume_percent, up) {
                let _ = commands.send(ClientCommand::SetVolume { percent });
                audio.volume_percent = Some(percent);
            }
        }
        Setting::Microphone => {
            if let Some(percent) = moved(audio.microphone_percent, up) {
                let _ = commands.send(ClientCommand::SetMicrophone { percent });
                audio.microphone_percent = Some(percent);
            }
        }
    }
}

/// A level's next 1% step, unless it has none that way or the codec is missing.
fn moved(level: Option<u8>, up: bool) -> Option<u8> {
    let level = level?;
    Some(step(level, up)).filter(|&next| next != level)
}

/// The next 1% step from a level.
fn step(percent: u8, up: bool) -> u8 {
    if up {
        percent.saturating_add(1).min(100)
    } else {
        percent.saturating_sub(1)
    }
}

/// Held keys repeat while paging through System and changing a level.
fn repeats(state: &UiState, action: KeyAction) -> bool {
    match state.screen {
        Screen::System => !matches!(action, KeyAction::NightMode(_) | KeyAction::ParkingBrake(_)),
        Screen::Settings => {
            state.settings_editing
                && matches!(
                    action,
                    KeyAction::Left | KeyAction::Right | KeyAction::Up | KeyAction::Down
                )
                && matches!(
                    selected_setting(state),
                    Setting::Volume | Setting::Microphone
                )
        }
        _ => false,
    }
}

/// The section a row starts, if it starts one.
fn section(row: Setting) -> &'static str {
    match row {
        Setting::Hotspot => "WIRELESS",
        Setting::Dpi => "VIDEO",
        Setting::Volume => "AUDIO",
        Setting::Fps | Setting::Microphone => "",
    }
}

/// A row's label and value; the hotspot's switch shows its value instead.
fn setting_text(state: &UiState, row: Setting) -> (&'static str, String) {
    let snapshot = &state.snapshot;
    let percent = |level: Option<u8>| level.map_or_else(|| "n/a".into(), |p| format!("{p}%"));
    let number = |value: u32| match value {
        0 => "n/a".into(),
        value => value.to_string(),
    };
    match row {
        Setting::Hotspot => ("Hotspot", String::new()),
        Setting::Dpi => ("DPI", number(snapshot.projection.dpi)),
        Setting::Fps => ("FPS", number(snapshot.projection.fps)),
        Setting::Volume => (
            "Speakers volume",
            percent(snapshot.system.audio.volume_percent),
        ),
        Setting::Microphone => (
            "Microphone volume",
            percent(snapshot.system.audio.microphone_percent),
        ),
    }
}

/// The menu's status lights: Bluetooth ready for phones, the hotspot up, and a
/// GNSS fix, orange while a receiver has none.
fn status_lights(snapshot: &AppSnapshot) -> (LedState, LedState, LedState) {
    let light = |on: bool| if on { LedState::On } else { LedState::Off };
    let gnss = if !snapshot.gnss.position.trim().is_empty() {
        LedState::On
    } else if !snapshot.gnss.receiver.trim().is_empty() {
        LedState::Waiting
    } else {
        LedState::Off
    };
    (
        light(snapshot.bluetooth.ready),
        light(snapshot.wireless.hotspot),
        gnss,
    )
}

/// Local time, "14:05".
fn clock_text() -> String {
    // SAFETY: localtime_r fills the tm it is given, from the current time.
    let local = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut local);
        local
    };
    format!("{:02}:{:02}", local.tm_hour, local.tm_min)
}

/// "123 456", as phones show pairing codes.
fn spaced(code: &str) -> String {
    match (code.get(..3), code.get(3..)) {
        (Some(head), Some(tail)) if code.len() == 6 => format!("{head} {tail}"),
        _ => code.into(),
    }
}

/// A reading the core reports, which is unavailable while it is not connected.
fn core_value(connected: bool, value: &str) -> String {
    if connected {
        available(value)
    } else {
        "Unavailable".into()
    }
}

/// The I-Bus link, with how many frames it carried.
fn vehicle_status(vehicle: &VehicleSnapshot, core_connected: bool) -> String {
    let status = core_value(core_connected, &vehicle.status);
    match (core_connected, vehicle.frames, vehicle.bad_lines) {
        (false, ..) | (true, 0, 0) => status,
        (true, frames, 0) => format!("{status} · {frames} frames"),
        (true, frames, bad) => format!("{status} · {frames} frames, {bad} damaged"),
    }
}

/// "120 km/h", or "n/a" until the car has said.
fn reading(value: Option<impl std::fmt::Display>, unit: &str) -> String {
    value.map_or_else(|| "n/a".into(), |value| format!("{value} {unit}"))
}

/// The GNSS speed beside the car's own, while there is a fix.
fn gnss_speed(speed: &str) -> String {
    if speed.trim().is_empty() {
        String::new()
    } else {
        format!("GPS {speed}")
    }
}

fn ignition_text(ignition: Option<Ignition>) -> String {
    match ignition {
        None => "n/a",
        Some(Ignition::Off) => "Off",
        Some(Ignition::Accessory) => "Position 1",
        Some(Ignition::On) => "On",
        Some(Ignition::Starting) => "Starting",
    }
    .into()
}

/// "123 456 km".
fn odometer(km: Option<u32>) -> String {
    let Some(km) = km else {
        return "n/a".into();
    };
    let digits = km.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    format!("{grouped} km")
}

/// An Audio row pair: stereo or mono, and the sampling rate, while the stream
/// runs; "Idle" otherwise.
fn stream_text(
    format: Option<StreamFormat>,
    codec_present: bool,
    core_connected: bool,
) -> (String, String) {
    let both = |text: &str| (text.to_string(), text.to_string());
    match format {
        _ if !core_connected => both("Unavailable"),
        _ if !codec_present => both("n/a"),
        None => both("Idle"),
        Some(format) => (
            match format.channels {
                1 => "Mono".into(),
                2 => "Stereo".into(),
                channels => format!("{channels} channels"),
            },
            format!("{} kHz", f64::from(format.rate) / 1000.0),
        ),
    }
}

/// The projection resolution row. CarPlay is not part of this build, so only
/// Android Auto has a resolution.
fn projection_resolution(video: Option<VideoSettings>) -> String {
    match video {
        Some(v) => format!("AA {}x{} · CarPlay n/a", v.width, v.height),
        None => "Unavailable".into(),
    }
}

/// System's USB Devices rows: the phone, the GNSS receiver and the I-Bus
/// interface. A phone on Wi-Fi shows too, as it is the connected one.
fn usb_devices(snapshot: &AppSnapshot, core_connected: bool) -> (String, String, String) {
    if !core_connected {
        return (
            "Unavailable".into(),
            "Unavailable".into(),
            "Unavailable".into(),
        );
    }
    let projection = &snapshot.projection;
    let connected = projection.phone_connected || projection.streaming;
    let phone = match &snapshot.usb.selected {
        Some(device) => {
            let model = if device.product.trim().is_empty() {
                format!("USB {:04x}:{:04x}", device.vendor_id, device.product_id)
            } else {
                device.product.clone()
            };
            let state = if connected { "Connected" } else { "Detected" };
            format!("{model} · {state}")
        }
        None if connected => format!("{} · Wireless", known(&projection.phone_name)),
        None => "Not detected".into(),
    };
    let gnss = match snapshot.gnss.receiver.trim() {
        "" => "Not detected".into(),
        receiver => receiver.into(),
    };
    let vehicle = &snapshot.vehicle;
    let ibus = if vehicle.interface {
        "Detected".into()
    } else if vehicle.status == "Interface not connected" {
        "Not detected".into()
    } else {
        // Why it cannot be read, such as a missing permission.
        available(&vehicle.status)
    };
    (phone, gnss, ibus)
}

/// A frame's sequence number, if it is a projection frame of `size`.
fn read_frame_header(reader: &mut impl Read, size: (u32, u32)) -> io::Result<u64> {
    let mut header = [0u8; VIDEO_FRAME_HEADER_SIZE];
    reader.read_exact(&mut header)?;
    let width = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if header[..4] != VIDEO_FRAME_MAGIC || (width, height) != size {
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
    size: (u32, u32),
) -> io::Result<Option<(u64, SharedPixelBuffer<Rgba8Pixel>)>> {
    let sequence = read_frame_header(reader, size)?;
    if previous == Some(sequence) {
        return Ok(None);
    }
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(size.0, size.1);
    reader.read_exact(pixels.make_mut_bytes())?;
    if reader.read(&mut [0u8; 1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized video frame",
        ));
    }
    Ok(Some((sequence, pixels)))
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
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
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
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
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
            toml::from_str(include_str!("../../../config/keyboard.toml")).unwrap();
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
        assert_eq!(selected_setting(&state), Setting::Dpi);
        press(&mut state, KeyAction::Left, false);
        press(&mut state, KeyAction::Left, false);
        assert_eq!(selected_setting(&state), Setting::Microphone);
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
        assert_eq!(selected_setting(&state), Setting::Dpi);
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
        state.snapshot.projection.fps = 60;
        state.snapshot.system.audio.volume_percent = Some(14);
        state.snapshot.system.audio.microphone_percent = Some(48);
        let press = |state: &mut UiState, action, repeat| {
            handle_key_event(state, action, true, repeat, &tx);
        };
        // The DPI switches between 160 and 140.
        press(&mut state, KeyAction::Right, false);
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
        // The frame rate switches between 60 and 30.
        press(&mut state, KeyAction::Right, false);
        press(&mut state, KeyAction::Select, false);
        press(&mut state, KeyAction::Left, false);
        press(&mut state, KeyAction::Right, false);
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientCommand::SetFps { fps: 30 })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientCommand::SetFps { fps: 60 })
        ));
        press(&mut state, KeyAction::Select, false);
        // Levels move in 1% steps; a held key repeats.
        press(&mut state, KeyAction::Right, false);
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
        state.snapshot.system.audio.microphone_percent = None;
        press(&mut state, KeyAction::Down, false);
        assert!(rx.try_recv().is_err());
        assert_eq!(setting_text(&state, Setting::Microphone).1, "n/a");
        assert_eq!(setting_text(&state, Setting::Dpi).1, "160");
        assert_eq!(setting_text(&state, Setting::Fps).1, "60");
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
    fn usb_devices_list_the_phone_the_gnss_receiver_and_the_ibus_interface() {
        let mut snapshot = AppSnapshot::default();
        snapshot.vehicle.status = "Interface not connected".into();
        let not_detected = || {
            (
                "Not detected".to_string(),
                "Not detected".to_string(),
                "Not detected".to_string(),
            )
        };
        assert_eq!(usb_devices(&snapshot, true), not_detected());
        assert_eq!(usb_devices(&snapshot, false).0, "Unavailable");
        // A phone on USB, before and once Android Auto runs on it.
        snapshot.usb.selected = Some(navbridge_common::UsbDeviceInfo {
            product: "Pixel 10".into(),
            ..Default::default()
        });
        assert_eq!(usb_devices(&snapshot, true).0, "Pixel 10 · Detected");
        snapshot.projection.phone_connected = true;
        assert_eq!(usb_devices(&snapshot, true).0, "Pixel 10 · Connected");
        // A phone on Wi-Fi is the connected one too.
        snapshot.usb.selected = None;
        snapshot.projection.phone_name = "Pixel 10".into();
        assert_eq!(usb_devices(&snapshot, true).0, "Pixel 10 · Wireless");
        snapshot.gnss.receiver = "u-blox (ttyACM1)".into();
        snapshot.vehicle.interface = true;
        snapshot.vehicle.status = "No I-Bus traffic".into();
        let (_, gnss, ibus) = usb_devices(&snapshot, true);
        assert_eq!(
            (gnss.as_str(), ibus.as_str()),
            ("u-blox (ttyACM1)", "Detected")
        );
        snapshot.vehicle.interface = false;
        snapshot.vehicle.status = "No permission to read the interface".into();
        assert_eq!(
            usb_devices(&snapshot, true).2,
            "No permission to read the interface"
        );
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
    const PANEL: (u32, u32) = (UI_WIDTH, UI_HEIGHT);
    #[test]
    fn unchanged_frame_does_not_read_or_allocate_pixels() {
        let mut reader = io::Cursor::new(header());
        assert!(read_projection(&mut reader, Some(42), PANEL)
            .unwrap()
            .is_none());
        assert_eq!(reader.position(), VIDEO_FRAME_HEADER_SIZE as u64);
    }
    #[test]
    fn new_frame_loads_directly_and_rejects_bad_lengths_and_geometry() {
        let mut data = header();
        assert!(read_projection(&mut &data[..], None, PANEL).is_err());
        data.extend(vec![123; (UI_WIDTH * UI_HEIGHT * 4) as usize]);
        let (sequence, pixels) = read_projection(&mut &data[..], None, PANEL)
            .unwrap()
            .unwrap();
        assert_eq!(sequence, 42);
        assert!(pixels.as_bytes().iter().all(|&b| b == 123));
        // A UI drawn twice as large takes only frames of its size.
        assert!(read_projection(&mut &data[..], None, (800, 480)).is_err());
        data.push(0);
        assert!(read_projection(&mut &data[..], None, PANEL).is_err());
        data[4] ^= 1;
        assert!(read_projection(&mut &data[..], Some(42), PANEL).is_err());
    }
}

#[cfg(test)]
mod vehicle_tests {
    use super::*;

    #[test]
    fn the_ibus_page_shows_readings_and_what_is_missing() {
        let vehicle = VehicleSnapshot {
            status: "Receiving".into(),
            interface: true,
            frames: 1234,
            bad_lines: 2,
            ignition: Some(Ignition::On),
            speed_kmh: Some(120),
            rpm: Some(3000),
            coolant_c: Some(90),
            outside_c: Some(-4),
            odometer_km: Some(123_456),
        };
        assert_eq!(
            vehicle_status(&vehicle, true),
            "Receiving · 1234 frames, 2 damaged"
        );
        assert_eq!(reading(vehicle.speed_kmh, "km/h"), "120 km/h");
        assert_eq!(reading(vehicle.rpm, "rpm"), "3000 rpm");
        assert_eq!(reading(vehicle.outside_c, "°C"), "-4 °C");
        assert_eq!(ignition_text(vehicle.ignition), "On");
        assert_eq!(ignition_text(Some(Ignition::Accessory)), "Position 1");
        assert_eq!(odometer(vehicle.odometer_km), "123 456 km");
        assert_eq!(odometer(Some(1_000)), "1 000 km");
        assert_eq!(odometer(Some(987)), "987 km");
        assert_eq!(gnss_speed("36.0 km/h"), "GPS 36.0 km/h");
        assert_eq!(gnss_speed(""), "");

        let unknown = VehicleSnapshot {
            status: "Interface not connected".into(),
            ..Default::default()
        };
        assert_eq!(vehicle_status(&unknown, true), "Interface not connected");
        assert_eq!(reading(unknown.rpm, "rpm"), "n/a");
        assert_eq!(ignition_text(unknown.ignition), "n/a");
        assert_eq!(odometer(unknown.odometer_km), "n/a");
        // Without the core, nothing is known about the car.
        assert_eq!(vehicle_status(&vehicle, false), "Unavailable");
    }
}

/// Renders the screens as the panel shows them, with sample data, into PPM
/// images in /tmp: `cargo test -p navbridge-ui --features navbridge-ui/direct-kms
/// -- --ignored preview`.
#[cfg(test)]
mod preview {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};

    struct Headless(Rc<MinimalSoftwareWindow>);

    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    #[ignore = "writes preview images to /tmp"]
    fn preview() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
        window.set_size(slint::PhysicalSize::new(UI_WIDTH, UI_HEIGHT));
        let ui = AppWindow::new().unwrap();
        ui.show().unwrap();

        let mut state = UiState {
            core_connected: true,
            ..UiState::default()
        };
        let snapshot = &mut state.snapshot;
        snapshot.bluetooth.ready = true;
        snapshot.bluetooth.pairing = true;
        snapshot.bluetooth.pairing_seconds = 95;
        snapshot.bluetooth.name = "NavBridge-984B".into();
        snapshot.wireless.enabled = true;
        snapshot.wireless.hotspot = true;
        snapshot.wireless.status = "Hotspot NavBridge-984B is on (5 GHz, channel 36)".into();
        snapshot.gnss.receiver = "u-blox (ttyACM1)".into();
        snapshot.gnss.status = "3D fix".into();
        snapshot.gnss.satellites = "9 / 14".into();
        snapshot.gnss.position = "48.85837, 2.29448".into();
        snapshot.gnss.altitude = "35.0 m".into();
        snapshot.gnss.speed = "36.0 km/h".into();
        snapshot.gnss.hdop = "0.9".into();
        snapshot.gnss.utc = "12:05:31".into();
        snapshot.usb.selected = Some(navbridge_common::UsbDeviceInfo {
            product: "Pixel 10".into(),
            ..Default::default()
        });
        snapshot.projection.dpi = 160;
        snapshot.projection.fps = 60;
        snapshot.projection.decoder = "FFmpeg 7.1.5 h264 (software)".into();
        snapshot.system.cpu_usage = "12.5%".into();
        snapshot.system.memory = "1.2 / 7.9 GB".into();
        snapshot.system.cpu_temp = "52.3 °C".into();
        snapshot.system.audio.codec = "USB audio CODEC (08bb:2902)".into();
        snapshot.system.audio.output = Some(StreamFormat {
            channels: 2,
            rate: 48000,
        });
        snapshot.system.audio.input = Some(StreamFormat {
            channels: 1,
            rate: 16000,
        });
        snapshot.system.audio.volume_percent = Some(75);
        snapshot.system.audio.microphone_percent = Some(48);
        state.video = Some(VideoSettings {
            width: 800,
            height: 480,
        });
        let snapshot = &mut state.snapshot;
        snapshot.vehicle = VehicleSnapshot {
            status: "Receiving".into(),
            interface: true,
            frames: 1234,
            bad_lines: 0,
            ignition: Some(Ignition::On),
            speed_kmh: Some(88),
            rpm: Some(2400),
            coolant_c: Some(90),
            outside_c: Some(14),
            odometer_km: Some(187_654),
        };
        for (name, screen, page) in [
            // Each Phone page follows another screen, so that its switch is
            // drawn as it ends up, not as its animation starts.
            ("menu", Screen::Menu, 0),
            ("phone", Screen::Phone, 0),
            ("settings", Screen::Settings, 0),
            ("phone-waiting", Screen::Phone, 0),
            ("settings-open", Screen::Settings, 0),
            ("phone-off", Screen::Phone, 0),
            ("settings-fps", Screen::Settings, 0),
            ("system-module", Screen::System, 0),
            ("system-video", Screen::System, 1),
            ("system-gnss", Screen::System, 2),
            ("system-ibus", Screen::System, 3),
        ] {
            state.screen = screen;
            state.system_page = page;
            // The speakers' volume, opened to change it.
            state.settings_editing = name == "settings-open";
            state.settings_index = match name {
                "settings-open" => 3,
                "settings-fps" => 2,
                _ => 0,
            };
            // And the hotspot's switch off, on an unchosen row; on the Phone
            // page, pairing open, then not, then the hotspot off.
            state.snapshot.wireless.enabled = !matches!(name, "settings-fps" | "phone-off");
            state.snapshot.bluetooth.pairing = name == "phone";
            render_state(&ui, &state);
            capture(&ui, &window, name, 1);
        }
        // The menu as a PC window draws it at frontend.toml's scale = 2.
        window
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
        window.set_size(slint::PhysicalSize::new(UI_WIDTH * 2, UI_HEIGHT * 2));
        ui.set_double_size(true);
        state.screen = Screen::Menu;
        render_state(&ui, &state);
        capture(&ui, &window, "menu-800x480", 2);
    }

    /// Draws the window, `scale` times the UI's size, into /tmp/navbridge-NAME.ppm.
    fn capture(ui: &AppWindow, window: &MinimalSoftwareWindow, name: &str, scale: u32) {
        let (width, height) = (UI_WIDTH * scale, UI_HEIGHT * scale);
        ui.set_clock("14:05".into());
        ui.window().request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        });
        let mut image = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in &pixels {
            image.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
        }
        let path = format!("/tmp/navbridge-{name}.ppm");
        std::fs::write(&path, image).unwrap();
        println!("{path}");
    }
}
