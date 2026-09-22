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
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.into(),
            fullscreen: false,
            reconnect_ms: 1_000,
        }
    }
}

impl UiConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
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
    #[serde(default = "default_night_mode_key")]
    pub toggle_night_mode: Vec<String>,
    #[serde(default)]
    pub next_track: Vec<String>,
    #[serde(default)]
    pub previous_track: Vec<String>,
    #[serde(default)]
    pub voice_assistant: Vec<String>,
    #[serde(default)]
    pub answer_call: Vec<String>,
    #[serde(default)]
    pub phone_home: Vec<String>,
    #[serde(default)]
    pub pause_music: Vec<String>,
    #[serde(default)]
    pub resume_music: Vec<String>,

}

fn default_night_mode_key() -> Vec<String> { vec!["Tab".into()] }
