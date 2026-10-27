use std::fs;
use std::path::Path;

use serde::Deserialize;

use navbridge_common::DEFAULT_SOCKET_PATH;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub socket_path: String,
    pub fullscreen: bool,
    pub reconnect_ms: u64,
    /// 1 for the panel's 400x240; 2 draws the UI at 800x480 on a PC, with
    /// Android Auto's video at its full size.
    pub scale: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.into(),
            fullscreen: false,
            reconnect_ms: 1_000,
            scale: 1,
        }
    }
}

impl UiConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&raw)?;
        if !matches!(config.scale, 1 | 2) {
            return Err(format!("scale must be 1 or 2, not {}", config.scale).into());
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyboardConfig {
    pub bindings: KeyboardBindings,
}

impl KeyboardConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyboardBindings {
    pub up: Vec<String>,
    pub down: Vec<String>,
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub select: Vec<String>,
    pub back: Vec<String>,
    pub home: Vec<String>,
    pub quit: Vec<String>,
    #[serde(default = "default_normal_mode_keys")]
    pub normal_mode: Vec<String>,
    #[serde(default = "default_night_mode_keys")]
    pub night_mode: Vec<String>,
    #[serde(default = "default_parking_brake_on_keys")]
    pub parking_brake_on: Vec<String>,
    #[serde(default = "default_parking_brake_off_keys")]
    pub parking_brake_off: Vec<String>,
    #[serde(default)]
    pub next_track: Vec<String>,
    #[serde(default)]
    pub previous_track: Vec<String>,
    #[serde(default)]
    pub voice_assistant: Vec<String>,
    #[serde(default)]
    pub answer_call: Vec<String>,
    #[serde(default)]
    pub end_call: Vec<String>,
    #[serde(default)]
    pub phone_home: Vec<String>,
    #[serde(default)]
    pub open_maps: Vec<String>,
    #[serde(default)]
    pub open_media: Vec<String>,
    #[serde(default = "default_play_pause_keys")]
    pub play_pause: Vec<String>,
}

// Defaults keep older keyboard.toml files working: these actions replaced
// `toggle_night_mode`, `pause_music` and `resume_music`.
fn default_normal_mode_keys() -> Vec<String> {
    vec!["PageUp".into()]
}

fn default_night_mode_keys() -> Vec<String> {
    vec!["PageDown".into()]
}

fn default_parking_brake_on_keys() -> Vec<String> {
    vec!["Control".into(), "ControlR".into()]
}

fn default_parking_brake_off_keys() -> Vec<String> {
    vec!["Alt".into()]
}

fn default_play_pause_keys() -> Vec<String> {
    vec!["P".into()]
}
