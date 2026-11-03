//! Settings' rows: their sections, labels and values, and what changing one
//! sends the core.

use crate::state::UiState;
use navbridge_common::{ClientCommand, VideoCodec, CALL_VOLUME_DB};

/// The projection DPIs Settings offers.
pub(crate) const DPI_CHOICES: [u32; 2] = [160, 140];

/// Settings' rows, in their sections' boxes left to right, then top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    /// Wireless Android Auto: the hotspot, and Bluetooth for phones.
    Hotspot,
    /// The one paired phone called for wireless Android Auto, or any.
    DefaultPhone,
    /// Ends the wireless session; then, Connect phone calls phones again.
    Disconnect,
    /// The UI's colours.
    Theme,
    Dpi,
    /// The video codec the phone is asked for.
    Codec,
    Volume,
    /// How much louder than the speakers' volume calls play.
    CallVolume,
    Microphone,
}

pub(crate) const SETTINGS: [Setting; 9] = [
    Setting::Hotspot,
    Setting::DefaultPhone,
    Setting::Disconnect,
    Setting::Dpi,
    Setting::Codec,
    Setting::Volume,
    Setting::CallVolume,
    Setting::Microphone,
    Setting::Theme,
];

/// The themes Settings offers, in the order it steps through them.
pub(crate) const THEMES: [navbridge_common::Theme; 3] = [
    navbridge_common::Theme::Amber,
    navbridge_common::Theme::Blue,
    navbridge_common::Theme::Green,
];

pub(crate) fn settings_index(state: &UiState) -> usize {
    state.settings_index.min(SETTINGS.len() - 1)
}

pub(crate) fn selected_setting(state: &UiState) -> Setting {
    SETTINGS[settings_index(state)]
}

/// A row's place in `SETTINGS`, as taps name it.
#[cfg(test)]
pub(crate) fn place(row: Setting) -> usize {
    SETTINGS.iter().position(|&setting| setting == row).unwrap()
}

/// Settings' sections in order, each with its rows' places in `SETTINGS`.
pub(crate) fn settings_sections() -> Vec<(&'static str, std::ops::Range<usize>)> {
    let mut sections: Vec<(&'static str, std::ops::Range<usize>)> = Vec::new();
    for (index, &row) in SETTINGS.iter().enumerate() {
        match sections.last_mut() {
            Some((title, places)) if *title == section(row) => places.end = index + 1,
            _ => sections.push((section(row), index..index + 1)),
        }
    }
    sections
}

/// Changes the open row's value, up or down: the hotspot, the DPI and the
/// video codec switch, the audio codec's levels move by 1%, the call volume by
/// 1 dB, the theme and the default phone step to the next or previous one, and
/// Disconnect ends the wireless session, or once ended calls phones again.
pub(crate) fn change_setting(
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
        // Any phone, then each paired one; past either end, round again.
        Setting::DefaultPhone => {
            let bluetooth = &mut snapshot.bluetooth;
            let choices: Vec<Option<String>> = std::iter::once(None)
                .chain(bluetooth.phones.iter().map(|p| Some(p.address.clone())))
                .collect();
            let i = choices
                .iter()
                .position(|c| *c == bluetooth.default_phone)
                .unwrap_or(0);
            let n = choices.len();
            let address = choices[if up { i + 1 } else { i + n - 1 } % n].clone();
            let _ = commands.send(ClientCommand::SetDefaultPhone {
                address: address.clone(),
            });
            bluetooth.default_phone = address;
        }
        Setting::Disconnect => {
            if snapshot.bluetooth.paused {
                let _ = commands.send(ClientCommand::ConnectPhone);
                snapshot.bluetooth.paused = false;
            } else if snapshot.projection.wireless {
                let _ = commands.send(ClientCommand::DisconnectPhone);
                snapshot.bluetooth.paused = true;
            }
        }
        Setting::Dpi => {
            let dpi = match snapshot.projection.dpi {
                dpi if dpi == DPI_CHOICES[0] => DPI_CHOICES[1],
                _ => DPI_CHOICES[0],
            };
            let _ = commands.send(ClientCommand::SetDpi { dpi });
            snapshot.projection.dpi = dpi;
        }
        Setting::Codec => {
            let codec = match snapshot.projection.codec {
                VideoCodec::H264 => VideoCodec::H265,
                VideoCodec::H265 => VideoCodec::H264,
            };
            let _ = commands.send(ClientCommand::SetVideoCodec { codec });
            snapshot.projection.codec = codec;
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
        Setting::CallVolume => {
            if let Some(db) = louder(audio.call_volume_db, up) {
                let _ = commands.send(ClientCommand::SetCallVolume { db });
                audio.call_volume_db = Some(db);
            }
        }
        // The next theme, or the one before; past either end, round again.
        Setting::Theme => {
            let i = THEMES
                .iter()
                .position(|&t| t == snapshot.theme)
                .unwrap_or(0);
            let theme = THEMES[if up { i + 1 } else { i + THEMES.len() - 1 } % THEMES.len()];
            let _ = commands.send(ClientCommand::SetTheme { theme });
            snapshot.theme = theme;
        }
    }
}

/// A level's next 1% step, unless it has none that way or the codec is missing.
pub(crate) fn moved(level: Option<u8>, up: bool) -> Option<u8> {
    let level = level?;
    Some(step(level, up)).filter(|&next| next != level)
}

/// The next 1% step from a level.
pub(crate) fn step(percent: u8, up: bool) -> u8 {
    if up {
        percent.saturating_add(1).min(100)
    } else {
        percent.saturating_sub(1)
    }
}

/// The call volume's next 1 dB step, unless it has none that way or calls do
/// not play in the car.
pub(crate) fn louder(db: Option<i8>, up: bool) -> Option<i8> {
    let db = db?;
    let next = if up {
        db.saturating_add(1)
    } else {
        db.saturating_sub(1)
    };
    CALL_VOLUME_DB.contains(&next).then_some(next)
}

/// The section a row is in.
pub(crate) fn section(row: Setting) -> &'static str {
    match row {
        Setting::Hotspot | Setting::DefaultPhone | Setting::Disconnect => "WIRELESS",
        Setting::Dpi | Setting::Codec => "VIDEO",
        Setting::Volume | Setting::CallVolume | Setting::Microphone => "AUDIO",
        Setting::Theme => "UI",
    }
}

/// The default phone's name, while it is paired; none for any phone.
pub(crate) fn default_phone(state: &UiState) -> Option<&str> {
    let bluetooth = &state.snapshot.bluetooth;
    let address = bluetooth.default_phone.as_deref()?;
    bluetooth
        .phones
        .iter()
        .find(|phone| phone.address == address)
        .map(|phone| phone.name.as_str())
}

/// A row's label and value; the hotspot's switch shows its value instead.
pub(crate) fn setting_text(state: &UiState, row: Setting) -> (&'static str, String) {
    let snapshot = &state.snapshot;
    let percent = |level: Option<u8>| level.map_or_else(|| "n/a".into(), |p| format!("{p}%"));
    let number = |value: u32| match value {
        0 => "n/a".into(),
        value => value.to_string(),
    };
    match row {
        Setting::Hotspot => ("Hotspot", String::new()),
        Setting::DefaultPhone => (
            "Default phone",
            default_phone(state).unwrap_or("Any").into(),
        ),
        // A wireless session's phone, to disconnect; once disconnected, the
        // phones that Connect calls.
        Setting::Disconnect if snapshot.bluetooth.paused => {
            ("Connect", default_phone(state).unwrap_or("Any").into())
        }
        Setting::Disconnect => (
            "Disconnect",
            if !snapshot.projection.wireless {
                "n/a".into()
            } else if snapshot.projection.phone_name.is_empty() {
                "Phone".into()
            } else {
                snapshot.projection.phone_name.clone()
            },
        ),
        Setting::Dpi => ("DPI", number(snapshot.projection.dpi)),
        Setting::Codec => ("Codec", snapshot.projection.codec.name().into()),
        Setting::Volume => ("Speakers", percent(snapshot.system.audio.volume_percent)),
        Setting::CallVolume => (
            "Calls",
            snapshot
                .system
                .audio
                .call_volume_db
                .map_or_else(|| "n/a".into(), decibels),
        ),
        Setting::Microphone => (
            "Microphone",
            percent(snapshot.system.audio.microphone_percent),
        ),
        Setting::Theme => (
            "Theme",
            match snapshot.theme {
                navbridge_common::Theme::Amber => "Amber",
                navbridge_common::Theme::Blue => "Blue",
                navbridge_common::Theme::Green => "Green",
            }
            .into(),
        ),
    }
}

/// A gain as Settings shows it: "+6 dB", "0 dB", "-3 dB".
pub(crate) fn decibels(db: i8) -> String {
    match db {
        0 => "0 dB".into(),
        db => format!("{db:+} dB"),
    }
}
