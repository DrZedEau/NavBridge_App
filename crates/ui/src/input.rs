use navbridge_common::{InputAction, PhoneAction};
use slint::platform::Key;
use slint::SharedString;

use crate::config::KeyboardConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    Home,
    Quit,
    /// Android Auto in day (false) or night (true) mode.
    NightMode(bool),
    /// The parking brake on (true) or off (false), for Android Auto's
    /// driving restrictions.
    ParkingBrake(bool),
    Phone(PhoneAction),
}

/// A car button's action, which the core passes on, does what the key of the
/// same name in keyboard.toml does.
impl From<InputAction> for KeyAction {
    fn from(action: InputAction) -> Self {
        match action {
            InputAction::Up => Self::Up,
            InputAction::Down => Self::Down,
            InputAction::Left => Self::Left,
            InputAction::Right => Self::Right,
            InputAction::Select => Self::Select,
            InputAction::Back => Self::Back,
            InputAction::Home => Self::Home,
            InputAction::NormalMode => Self::NightMode(false),
            InputAction::NightMode => Self::NightMode(true),
            InputAction::ParkingBrakeOn => Self::ParkingBrake(true),
            InputAction::ParkingBrakeOff => Self::ParkingBrake(false),
            InputAction::NextTrack => Self::Phone(PhoneAction::NextTrack),
            InputAction::PreviousTrack => Self::Phone(PhoneAction::PreviousTrack),
            InputAction::VoiceAssistant => Self::Phone(PhoneAction::VoiceAssistant),
            InputAction::AnswerCall => Self::Phone(PhoneAction::AnswerCall),
            InputAction::EndCall => Self::Phone(PhoneAction::EndCall),
            InputAction::PhoneHome => Self::Phone(PhoneAction::PhoneHome),
            InputAction::OpenMaps => Self::Phone(PhoneAction::OpenMaps),
            InputAction::OpenMedia => Self::Phone(PhoneAction::OpenMedia),
            InputAction::PlayPause => Self::Phone(PhoneAction::PlayPause),
        }
    }
}

pub struct KeyMap {
    bindings: Vec<(String, KeyAction)>,
}

impl KeyMap {
    pub fn new(config: &KeyboardConfig) -> Result<Self, String> {
        let mut bindings = Vec::new();
        let b = &config.bindings;
        add(&mut bindings, &b.up, KeyAction::Up)?;
        add(&mut bindings, &b.down, KeyAction::Down)?;
        add(&mut bindings, &b.left, KeyAction::Left)?;
        add(&mut bindings, &b.right, KeyAction::Right)?;
        add(&mut bindings, &b.select, KeyAction::Select)?;
        add(&mut bindings, &b.back, KeyAction::Back)?;
        add(&mut bindings, &b.home, KeyAction::Home)?;
        add(&mut bindings, &b.quit, KeyAction::Quit)?;
        add(&mut bindings, &b.normal_mode, KeyAction::NightMode(false))?;
        add(&mut bindings, &b.night_mode, KeyAction::NightMode(true))?;
        add(
            &mut bindings,
            &b.parking_brake_on,
            KeyAction::ParkingBrake(true),
        )?;
        add(
            &mut bindings,
            &b.parking_brake_off,
            KeyAction::ParkingBrake(false),
        )?;
        add(
            &mut bindings,
            &b.next_track,
            KeyAction::Phone(PhoneAction::NextTrack),
        )?;
        add(
            &mut bindings,
            &b.previous_track,
            KeyAction::Phone(PhoneAction::PreviousTrack),
        )?;
        add(
            &mut bindings,
            &b.voice_assistant,
            KeyAction::Phone(PhoneAction::VoiceAssistant),
        )?;
        add(
            &mut bindings,
            &b.answer_call,
            KeyAction::Phone(PhoneAction::AnswerCall),
        )?;
        add(
            &mut bindings,
            &b.end_call,
            KeyAction::Phone(PhoneAction::EndCall),
        )?;
        add(
            &mut bindings,
            &b.phone_home,
            KeyAction::Phone(PhoneAction::PhoneHome),
        )?;
        add(
            &mut bindings,
            &b.open_maps,
            KeyAction::Phone(PhoneAction::OpenMaps),
        )?;
        add(
            &mut bindings,
            &b.open_media,
            KeyAction::Phone(PhoneAction::OpenMedia),
        )?;
        add(
            &mut bindings,
            &b.play_pause,
            KeyAction::Phone(PhoneAction::PlayPause),
        )?;
        Ok(Self { bindings })
    }

    pub fn action_for(&self, text: &str) -> Option<KeyAction> {
        let normalized = normalize_event_text(text);
        self.bindings
            .iter()
            .find_map(|(token, action)| (token == &normalized).then_some(*action))
    }
}

fn add(
    out: &mut Vec<(String, KeyAction)>,
    names: &[String],
    action: KeyAction,
) -> Result<(), String> {
    for name in names {
        out.push((configured_key_to_event_text(name)?, action));
    }
    Ok(())
}

fn configured_key_to_event_text(name: &str) -> Result<String, String> {
    let lower = name.trim().to_ascii_lowercase();
    let key = match lower.as_str() {
        "up" | "uparrow" => Some(Key::UpArrow),
        "down" | "downarrow" => Some(Key::DownArrow),
        "left" | "leftarrow" => Some(Key::LeftArrow),
        "right" | "rightarrow" => Some(Key::RightArrow),
        "return" | "enter" => Some(Key::Return),
        "escape" | "esc" => Some(Key::Escape),
        "backspace" => Some(Key::Backspace),
        "home" => Some(Key::Home),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "tab" => Some(Key::Tab),
        "space" => Some(Key::Space),
        // Modifiers on their own, such as a switch the board reports as a key.
        "control" | "ctrl" => Some(Key::Control),
        "controlr" | "rightcontrol" => Some(Key::ControlR),
        "alt" => Some(Key::Alt),
        "altgr" => Some(Key::AltGr),
        "shift" => Some(Key::Shift),
        _ => None,
    };

    if let Some(key) = key {
        let text: SharedString = key.into();
        return Ok(text.to_string());
    }

    let mut chars = lower.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Ok(ch.to_string()),
        _ => Err(format!("unsupported key name in keyboard.toml: {name}")),
    }
}

fn normalize_event_text(text: &str) -> String {
    if text.chars().count() == 1
        && text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
    {
        text.to_ascii_lowercase()
    } else {
        text.to_string()
    }
}
