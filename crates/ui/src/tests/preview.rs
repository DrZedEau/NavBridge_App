//! Renders the screens as the panel shows them, with sample data, into PPM
//! images in /tmp: `cargo test -p navbridge-ui --features navbridge-ui/direct-kms
//! -- --ignored preview`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{render::*, settings::*, state::*, AppWindow};
use navbridge_common::{StreamFormat, VehicleSnapshot, UI_HEIGHT, UI_WIDTH};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::ComponentHandle;

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
    snapshot.projection.decoder = "FFmpeg 7.1.5 h264 (software)".into();
    snapshot.system.cpu_usage = "12.5%".into();
    snapshot.system.memory = "1.2 / 7.9 GB".into();
    snapshot.system.cpu_temp = "52.3 °C".into();
    snapshot.system.audio.codec = "USB audio CODEC (08bb:2902)".into();
    snapshot.system.usb_devices = vec![
        "Google Inc. Android Open Accessory device".into(),
        "U-Blox AG [u-blox 7]".into(),
        "Texas Instruments PCM2912A Audio Codec".into(),
        "Arduino SA Arduino Micro".into(),
    ];
    snapshot.system.audio.output = Some(StreamFormat {
        channels: 2,
        rate: 48000,
    });
    snapshot.system.audio.input = Some(StreamFormat {
        channels: 1,
        rate: 16000,
    });
    snapshot.system.audio.volume_percent = Some(75);
    snapshot.system.audio.call_volume_db = Some(6);
    snapshot.system.audio.microphone_percent = Some(48);
    // Two paired phones; the first, in a wireless session.
    snapshot.bluetooth.phones = vec![
        navbridge_common::BluetoothPhone {
            address: "64:9D:38:E1:B6:24".into(),
            name: "Pixel 10".into(),
        },
        navbridge_common::BluetoothPhone {
            address: "A0:B1:C2:D3:E4:F5".into(),
            name: "Galaxy S25 Ultra".into(),
        },
    ];
    snapshot.projection.phone_name = "Pixel 10".into();
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
        ignition: Some(navbridge_common::Ignition::On),
        speed_kmh: Some(88),
        rpm: Some(2400),
        coolant_c: Some(90),
        outside_c: Some(14),
        range_km: Some(512),
        consumption_dl: Some(98),
        handbrake: Some(false),
        low_beam: Some(false),
        reverse: Some(false),
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
        ("settings-dpi", Screen::Settings, 0),
        ("system-module", Screen::System, 0),
        ("system-video", Screen::System, 1),
        ("system-gnss", Screen::System, 2),
        ("system-ibus", Screen::System, 3),
        ("settings-theme", Screen::Settings, 0),
        ("settings-phone", Screen::Settings, 0),
        ("menu-blue", Screen::Menu, 0),
        ("settings-green", Screen::Settings, 0),
        ("menu-green", Screen::Menu, 0),
        ("reverse", Screen::Phone, 0),
        ("pairing", Screen::Menu, 0),
    ] {
        stage(&mut state, name);
        state.screen = screen;
        state.system_page = page;
        render_state(&ui, &state);
        capture(&ui, &window, name, 1);
    }
    // Taps reach their callbacks: a menu tile, a page's title, a row.
    let taps = Rc::new(RefCell::new(Vec::new()));
    let log = |taps: &Rc<RefCell<Vec<String>>>| Rc::clone(taps);
    let t = log(&taps);
    ui.on_tile_tapped(move |index| t.borrow_mut().push(format!("tile {index}")));
    let t = log(&taps);
    ui.on_back_tapped(move || t.borrow_mut().push("back".into()));
    let t = log(&taps);
    ui.on_setting_tapped(move |index, half| t.borrow_mut().push(format!("row {index} {half}")));
    let tap = |x: f32, y: f32| {
        let position = slint::LogicalPosition::new(x, y);
        let button = slint::platform::PointerEventButton::Left;
        window.dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        window.dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    };
    // The pairing code, drawn last, keeps taps from the menu beneath.
    state.screen = Screen::Menu;
    render_state(&ui, &state);
    tap(200.0, 120.0);
    assert!(taps.borrow().is_empty());
    state.snapshot.bluetooth.confirm = None;
    render_state(&ui, &state);
    tap(200.0, 120.0);
    state.screen = Screen::Settings;
    state.settings_editing = false;
    render_state(&ui, &state);
    tap(40.0, 15.0);
    // Every row of the first box, and a row in each other: the
    // microphone, the codec, the theme.
    for (x, y) in [
        (100.0, 60.0),
        (100.0, 89.0),
        (100.0, 113.0),
        (250.0, 113.0),
        (100.0, 190.0),
        (250.0, 167.0),
    ] {
        tap(x, y);
    }
    // The speakers' volume, open: its label, its value's left and right.
    state.settings_index = place(Setting::Volume);
    state.settings_editing = true;
    render_state(&ui, &state);
    for x in [230.0, 320.0, 370.0] {
        tap(x, 60.0);
    }
    assert_eq!(
        *taps.borrow(),
        [
            "tile 1", "back", "row 0 0", "row 1 0", "row 2 0", "row 7 0", "row 4 0", "row 8 0",
            "row 5 0", "row 5 -1", "row 5 1"
        ]
    );
    state.settings_editing = false;

    // As a PC window draws them at frontend.toml's scale = 2: the screens
    // the README shows, from docs/screenshots.
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    window.set_size(slint::PhysicalSize::new(UI_WIDTH * 2, UI_HEIGHT * 2));
    ui.set_double_size(true);
    for (name, screen, page) in [
        ("menu", Screen::Menu, 0),
        ("phone", Screen::Phone, 0),
        ("settings", Screen::Settings, 0),
        ("pairing", Screen::Menu, 0),
        ("system-module", Screen::System, 0),
        ("system-video", Screen::System, 1),
        ("system-gnss", Screen::System, 2),
        ("system-ibus", Screen::System, 3),
    ] {
        stage(&mut state, name);
        state.screen = screen;
        state.system_page = page;
        render_state(&ui, &state);
        capture(&ui, &window, &format!("{name}-800x480"), 2);
    }
}

/// Sets up the sample state a preview `name` shows, beyond its screen and
/// page.
fn stage(state: &mut UiState, name: &str) {
    // The speakers' volume, opened to change it.
    state.settings_editing = matches!(name, "settings-open" | "settings-phone");
    state.settings_index = place(match name {
        "settings-open" => Setting::Volume,
        "settings-dpi" => Setting::Dpi,
        "settings-theme" => Setting::Theme,
        // The default phone, opened, with a long name; and Connect,
        // once Disconnect has ended the session.
        "settings-phone" => Setting::DefaultPhone,
        // The audio box's last row.
        "settings-green" => Setting::Microphone,
        _ => Setting::Hotspot,
    });
    let bluetooth = &mut state.snapshot.bluetooth;
    bluetooth.default_phone = Some(if name == "settings-phone" {
        "A0:B1:C2:D3:E4:F5".into()
    } else {
        "64:9D:38:E1:B6:24".into()
    });
    bluetooth.paused = name == "settings-phone";
    // A wireless session, where the hotspot is on.
    state.snapshot.projection.wireless = !matches!(name, "settings-dpi" | "settings-phone");
    // The other themes.
    state.snapshot.theme = match name {
        "settings-theme" | "menu-blue" => navbridge_common::Theme::Blue,
        "settings-green" | "menu-green" => navbridge_common::Theme::Green,
        _ => navbridge_common::Theme::Amber,
    };
    // And the hotspot's switch off, on an unchosen row; on the Phone
    // page, pairing open, then not, then the hotspot off.
    state.snapshot.wireless.enabled = !matches!(name, "settings-dpi" | "phone-off");
    state.snapshot.bluetooth.pairing = name == "phone";
    // The car in reverse, over the Phone page.
    state.snapshot.vehicle.reverse = Some(name == "reverse");
    state.snapshot.bluetooth.confirm =
        (name == "pairing").then(|| navbridge_common::PairingRequest {
            device: "Pixel 10".into(),
            code: "042137".into(),
        });
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
