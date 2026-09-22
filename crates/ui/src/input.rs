use navbridge_common::PhoneAction;
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
    ToggleNightMode,
    Phone(PhoneAction),
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
        add(
            &mut bindings,
            &b.toggle_night_mode,
            KeyAction::ToggleNightMode,
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
            &b.phone_home,
            KeyAction::Phone(PhoneAction::PhoneHome),
        )?;
        add(
            &mut bindings,
            &b.pause_music,
            KeyAction::Phone(PhoneAction::PauseMusic),
        )?;
        add(
            &mut bindings,
            &b.resume_music,
            KeyAction::Phone(PhoneAction::ResumeMusic),
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
        "tab" => Some(Key::Tab),
        "space" => Some(Key::Space),
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
