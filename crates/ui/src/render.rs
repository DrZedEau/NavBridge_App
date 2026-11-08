//! Puts the UI's state on screen: each page's texts, lights and values.

use crate::actions::reversing;
use crate::projection::{read_frame_header, refresh_projection};
use crate::settings_page::{
    default_phone, setting_text, settings_index, settings_sections, Setting, SETTINGS,
};
use crate::state::{Screen, UiState, VideoSettings, USB_ROWS};
use crate::{AppWindow, LedState, Palette, SettingsRow, SettingsSection, Theme};
use navbridge_common::{AppSnapshot, StreamFormat, VehicleSnapshot, UI_HEIGHT};
use slint::{ComponentHandle, Image};

pub(crate) fn render_state(ui: &AppWindow, state: &UiState) {
    ui.set_page_width(state.ui_width() as f32);
    if ui.get_screen() != state.screen as i32 {
        ui.set_video_active(false);
        ui.set_projection_frame(Image::default());
        let baseline = if state.screen == Screen::Phone {
            std::fs::File::open(&state.frame_path)
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
    let devices: Vec<slint::SharedString> = usb_rows(&state.snapshot, state.core_connected)
        .into_iter()
        .map(Into::into)
        .collect();
    ui.set_usb_devices(slint::ModelRc::new(slint::VecModel::from(devices)));
    // On the panel, the window takes the display mode; until it is shown, the
    // size this UI is designed for.
    let size = ui.window().size();
    let (width, height) = if size.width > 0 && size.height > 0 {
        (size.width, size.height)
    } else {
        (state.ui_width(), UI_HEIGHT)
    };
    ui.set_ui_resolution(format!("{width}x{height}").into());
    ui.set_projection_resolution(projection_resolution(state.video).into());
    ui.set_projection_decoder(core(&state.snapshot.projection.decoder));
    let audio = &state.snapshot.system.audio;
    let present = state.core_connected && audio.codec != "Not detected";
    let (output_type, output_rate) = stream_text(audio.output, present, state.core_connected);
    ui.set_audio_output_type(output_type.into());
    ui.set_audio_output_rate(output_rate.into());
    let (input_type, input_rate) = stream_text(audio.input, present, state.core_connected);
    ui.set_audio_input_type(input_type.into());
    ui.set_audio_input_rate(input_rate.into());
    let sections: Vec<SettingsSection> = settings_sections()
        .into_iter()
        .map(|(title, places)| {
            let rows: Vec<SettingsRow> = places
                .map(|index| {
                    let row = SETTINGS[index];
                    let (label, value) = setting_text(state, row);
                    SettingsRow {
                        index: index as i32,
                        label: label.into(),
                        value: value.into(),
                        toggle: row == Setting::Hotspot,
                        on: state.snapshot.wireless.enabled,
                    }
                })
                .collect();
            SettingsSection {
                title: title.into(),
                rows: slint::ModelRc::new(slint::VecModel::from(rows)),
            }
        })
        .collect();
    ui.set_settings_sections(slint::ModelRc::new(slint::VecModel::from(sections)));
    ui.set_settings_index(settings_index(state) as i32);
    ui.global::<Palette>()
        .set_theme(match state.snapshot.theme {
            navbridge_common::Theme::Amber => Theme::Amber,
            navbridge_common::Theme::Blue => Theme::Blue,
            navbridge_common::Theme::Green => Theme::Green,
        });
    ui.set_settings_editing(state.settings_editing);
    ui.set_pair_option(pair_option(state));
    ui.set_pairing(state.snapshot.bluetooth.pairing);
    let pairing = state.snapshot.bluetooth.confirm.as_ref();
    ui.set_pairing_request(pairing.is_some());
    ui.set_reversing(reversing(state));
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
    ui.set_vehicle_range(kilometres(vehicle.range_km).into());
    ui.set_vehicle_consumption(consumption(vehicle.consumption_dl).into());
    // Frames that arrive while projection cannot be shown are skipped; pick up
    // the newest one as soon as it can be, without waiting for the phone.
    refresh_projection(ui, state);
}

pub(crate) fn available(value: &str) -> String {
    if value.trim().is_empty() {
        "n/a".into()
    } else {
        value.into()
    }
}

/// With wireless mode on, the Phone page offers pairing a phone.
pub(crate) fn pair_option(state: &UiState) -> bool {
    state.core_connected && state.snapshot.wireless.enabled
}

/// The Phone page's message until a phone connects. With the hotspot on,
/// paired phones are called, or the default one, except while pairing is open:
/// then, how to pair; and none once Settings disconnected the phone.
pub(crate) fn phone_message(state: &UiState) -> String {
    let bluetooth = &state.snapshot.bluetooth;
    if !pair_option(state) {
        return "Waiting for phone".into();
    }
    if bluetooth.paused && !bluetooth.pairing {
        return "Disconnected\nConnect in Settings calls the phone again".into();
    }
    if !bluetooth.pairing {
        return default_phone(state).map_or("Waiting for paired phone".into(), |name| {
            format!("Waiting for {name}")
        });
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

/// The menu's status lights: Bluetooth ready for phones, the hotspot up, and a
/// GNSS fix, orange while a receiver has none.
pub(crate) fn status_lights(snapshot: &AppSnapshot) -> (LedState, LedState, LedState) {
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
pub(crate) fn clock_text() -> String {
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
pub(crate) fn spaced(code: &str) -> String {
    match (code.get(..3), code.get(3..)) {
        (Some(head), Some(tail)) if code.len() == 6 => format!("{head} {tail}"),
        _ => code.into(),
    }
}

/// A reading the core reports, which is unavailable while it is not connected.
pub(crate) fn core_value(connected: bool, value: &str) -> String {
    if connected {
        available(value)
    } else {
        "Unavailable".into()
    }
}

/// The I-Bus link, with how many frames it carried.
pub(crate) fn vehicle_status(vehicle: &VehicleSnapshot, core_connected: bool) -> String {
    let status = core_value(core_connected, &vehicle.status);
    match (core_connected, vehicle.frames, vehicle.bad_lines) {
        (false, ..) | (true, 0, 0) => status,
        (true, frames, 0) => format!("{status} · {frames} frames"),
        (true, frames, bad) => format!("{status} · {frames} frames, {bad} damaged"),
    }
}

/// "120 km/h", or "n/a" until the car has said.
pub(crate) fn reading(value: Option<impl std::fmt::Display>, unit: &str) -> String {
    value.map_or_else(|| "n/a".into(), |value| format!("{value} {unit}"))
}

/// The GNSS speed beside the car's own, while there is a fix.
pub(crate) fn gnss_speed(speed: &str) -> String {
    if speed.trim().is_empty() {
        String::new()
    } else {
        format!("GPS {speed}")
    }
}

/// "1 024 km".
pub(crate) fn kilometres(km: Option<u16>) -> String {
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

/// Tenths of a litre per 100 km, as "13.2 L/100 km".
pub(crate) fn consumption(dl: Option<u16>) -> String {
    match dl {
        Some(dl) => format!("{}.{} L/100 km", dl / 10, dl % 10),
        None => "n/a".into(),
    }
}

/// An Audio row pair: stereo or mono, and the sampling rate, while the stream
/// runs; "Idle" otherwise.
pub(crate) fn stream_text(
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
pub(crate) fn projection_resolution(video: Option<VideoSettings>) -> String {
    match video {
        Some(v) => format!("AA {}x{} · CarPlay n/a", v.width, v.height),
        None => "Unavailable".into(),
    }
}

/// System's USB Devices: the plugged-in devices' names, in the order of their
/// ports, then Empty for the rest of the rows; Unavailable without the core.
pub(crate) fn usb_rows(snapshot: &AppSnapshot, core_connected: bool) -> Vec<String> {
    let devices = &snapshot.system.usb_devices;
    (0..USB_ROWS)
        .map(|row| match devices.get(row) {
            _ if !core_connected => "Unavailable".into(),
            Some(name) => name.clone(),
            None => "Empty".into(),
        })
        .collect()
}
