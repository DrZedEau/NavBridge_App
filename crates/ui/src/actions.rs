//! Keys, the car's buttons and taps: what each does on each screen.

use crate::input::KeyAction;
use crate::render::pair_option;
use crate::settings::{change_setting, selected_setting, settings_index, Setting, SETTINGS};
use crate::state::{Commands, Screen, UiState, MENU_ITEMS, SYSTEM_PAGES};
use navbridge_common::{ClientCommand, NavigationKey, TouchKind};

pub(crate) fn navigation_key(action: KeyAction) -> Option<NavigationKey> {
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

/// The keyboard acts only while the core reads no I-Bus, as on a PC: in the
/// car, the buttons come from the I-Bus, and the board's typed keys for them
/// would act twice.
pub(crate) fn keyboard_active(state: &UiState) -> bool {
    !(state.core_connected && state.snapshot.vehicle.interface)
}

/// Releases a key or a finger the phone still holds.
pub(crate) fn release_navigation(state: &mut UiState, commands: &Commands) {
    if let Some(key) = state.held_navigation.take() {
        let _ = commands.send(ClientCommand::NavigationInput {
            key,
            pressed: false,
        });
    }
    if let Some((x, y)) = state.touch.take() {
        let _ = commands.send(ClientCommand::Touch {
            kind: TouchKind::Up,
            x,
            y,
        });
    }
}

/// While the phone's pairing code shows, only its two answers take taps.
pub(crate) fn pairing_shown(state: &UiState) -> bool {
    state.snapshot.bluetooth.confirm.is_some()
}

/// The car is in reverse, as the core reports it from the I-Bus.
pub(crate) fn reversing(state: &UiState) -> bool {
    state.core_connected && state.snapshot.vehicle.reverse == Some(true)
}

/// A menu tile: chosen and opened, as Enter does.
pub(crate) fn tap_tile(state: &mut UiState, index: usize, commands: &Commands) {
    if state.screen != Screen::Menu || pairing_shown(state) {
        return;
    }
    state.menu_index = index.min(MENU_ITEMS - 1);
    handle_action(state, KeyAction::Select, commands);
}

/// A page's title: back to the menu, as Escape does.
pub(crate) fn tap_back(state: &mut UiState, commands: &Commands) {
    if !pairing_shown(state) {
        handle_action(state, KeyAction::Home, commands);
    }
}

pub(crate) fn tap_pair(state: &mut UiState, commands: &Commands) {
    if state.screen == Screen::Phone && !pairing_shown(state) {
        waiting_page(state, KeyAction::Select, commands);
    }
}

/// System's pages, turned by a swipe or a tap: -1 back, 1 on.
pub(crate) fn swipe_system(state: &mut UiState, delta: i32) {
    if state.screen == Screen::System && !pairing_shown(state) {
        let step = if delta < 0 { SYSTEM_PAGES - 1 } else { 1 };
        state.system_page = (state.system_page + step) % SYSTEM_PAGES;
    }
}

pub(crate) fn answer_pairing(state: &mut UiState, accept: bool, commands: &Commands) {
    if pairing_shown(state) {
        let key = if accept {
            KeyAction::Select
        } else {
            KeyAction::Back
        };
        handle_action(state, key, commands);
    }
}

/// A Settings row tapped, its place in `SETTINGS`: the hotspot, the default
/// phone, the theme, the DPI and the codec change at once, and Disconnect acts;
/// a level opens, then its value's halves (`half` -1 and 1) step it down and
/// up, and a tap on its label closes it.
pub(crate) fn tap_setting(state: &mut UiState, index: usize, half: i32, commands: &Commands) {
    if state.screen != Screen::Settings || pairing_shown(state) {
        return;
    }
    let index = index.min(SETTINGS.len() - 1);
    let open = state.settings_editing && settings_index(state) == index;
    if half != 0 {
        if open {
            change_setting(state, half > 0, commands);
        }
        return;
    }
    state.settings_index = index;
    state.settings_editing = false;
    if open || !state.core_connected {
        return;
    }
    match SETTINGS[index] {
        Setting::Hotspot
        | Setting::DefaultPhone
        | Setting::Disconnect
        | Setting::Dpi
        | Setting::Codec
        | Setting::Theme => change_setting(state, true, commands),
        Setting::Volume | Setting::CallVolume | Setting::Microphone => {
            state.settings_editing = true
        }
    }
}

pub(crate) fn handle_key_event(
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

pub(crate) fn handle_action(
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
    // The reverse camera covers every screen: keys act on none of them.
    if reversing(state) {
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
        // switch at once, and disconnects or connects the phone.
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
                    if matches!(
                        selected_setting(state),
                        Setting::Hotspot | Setting::Disconnect
                    ) {
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

/// The Phone page while there is no phone: in wireless mode, Enter switches
/// pairing on or off, and Back returns to the menu.
pub(crate) fn waiting_page(
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

/// Held keys repeat while paging through System and changing a level.
pub(crate) fn repeats(state: &UiState, action: KeyAction) -> bool {
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
                    Setting::Volume | Setting::CallVolume | Setting::Microphone
                )
        }
        _ => false,
    }
}
