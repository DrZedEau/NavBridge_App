//! What the UI shows and remembers: the screen, the core's last snapshot,
//! and what the phone still holds.

use std::path::PathBuf;
use std::time::Instant;

use navbridge_common::{
    video_frame_path, AppSnapshot, ClientCommand, NavigationKey, DEFAULT_SOCKET_PATH, UI_HEIGHT,
    UI_WIDTH,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Menu = 0,
    /// The connected phone's screen: Android Auto's projection, the only one
    /// in this build, or while there is none, how far a phone has got.
    Phone = 1,
    System = 2,
    Settings = 3,
}

/// The menu's tiles, left to right: Phone, Settings, System.
pub(crate) const MENU_ITEMS: usize = 3;

/// System pages: module and USB, video and audio, GNSS, and the car's I-Bus.
pub(crate) const SYSTEM_PAGES: usize = 4;

/// The USB devices System lists, as many as the board has ports.
pub(crate) const USB_ROWS: usize = 4;

/// The projection video's size, which the core asks the phone for, from its
/// hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoSettings {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) struct UiState {
    pub(crate) screen: Screen,
    pub(crate) menu_index: usize,
    /// The chosen row in Settings, and whether Enter opened it to change it.
    pub(crate) settings_index: usize,
    pub(crate) settings_editing: bool,
    pub(crate) system_page: usize,
    pub(crate) core_connected: bool,
    pub(crate) projection_hidden_by_phone: bool,
    pub(crate) snapshot: AppSnapshot,
    pub(crate) video: Option<VideoSettings>,
    pub(crate) held_navigation: Option<NavigationKey>,
    pub(crate) video_sequence: std::cell::Cell<Option<u64>>,
    /// The size projection frames come in: the panel's 400x240, or 800x480
    /// for a UI drawn twice as large.
    pub(crate) frame_size: (u32, u32),
    /// The core's projection frames, beside its socket.
    pub(crate) frame_path: PathBuf,
    /// Where a finger or the mouse holds the projection, in Android Auto's
    /// pixels, and when it last moved there.
    pub(crate) touch: Option<(u32, u32)>,
    pub(crate) touch_moved_at: Option<Instant>,
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
            frame_path: video_frame_path(DEFAULT_SOCKET_PATH),
            touch: None,
            touch_moved_at: None,
        }
    }
}

impl UiState {
    pub(crate) fn disconnect_projection(&mut self) {
        self.core_connected = false;
        self.held_navigation = None;
        // The theme stays, so the colours do not change while the core restarts.
        self.snapshot = AppSnapshot {
            theme: self.snapshot.theme,
            ..AppSnapshot::default()
        };
        self.video = None;
        self.projection_hidden_by_phone = false;
        self.video_sequence.set(None);
    }
    pub(crate) fn show_projection(&self) -> bool {
        self.core_connected
            && self.screen == Screen::Phone
            && (self.snapshot.projection.phone_connected || self.snapshot.projection.streaming)
    }
}

pub(crate) type Commands = std::sync::mpsc::Sender<ClientCommand>;
