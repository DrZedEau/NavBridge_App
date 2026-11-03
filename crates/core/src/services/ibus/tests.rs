use std::{
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
};

use super::frame::*;
use super::*;

#[test]
fn buttons_act_on_their_press_alone() {
    let bindings = BTreeMap::from([
        (Button::KnobPress, InputAction::Select),
        (Button::KnobRight, InputAction::Right),
    ]);
    let press = |button, phase, steps| Press {
        button,
        phase,
        steps,
    };
    let input = |action, pressed| ButtonInput { action, pressed };
    assert_eq!(
        inputs(press(Button::KnobPress, Phase::Press, 1), &bindings),
        [
            input(InputAction::Select, true),
            input(InputAction::Select, false)
        ]
    );
    // Held and released: nothing more.
    assert!(inputs(press(Button::KnobPress, Phase::Hold, 1), &bindings).is_empty());
    assert!(inputs(press(Button::KnobPress, Phase::Release, 1), &bindings).is_empty());
    // A detent each.
    assert_eq!(
        inputs(press(Button::KnobRight, Phase::Press, 2), &bindings).len(),
        4
    );
    // A button with nothing to do.
    assert!(inputs(press(Button::Telephone, Phase::Press, 1), &bindings).is_empty());
}

/// What the service sent the board, waiting up to a second for it.
fn sent_to_board(board: &mut File) -> String {
    let mut fd = libc::pollfd {
        fd: board.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, for the board side's open descriptor.
    if unsafe { libc::poll(&mut fd, 1, 1000) } <= 0 {
        return String::new();
    }
    let mut bytes = [0u8; 64];
    let n = board.read(&mut bytes).unwrap();
    String::from_utf8_lossy(&bytes[..n]).into()
}

fn wait_for_inputs(ibus: &IbusService, count: usize) -> Vec<ButtonInput> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut taken = Vec::new();
    while taken.len() < count {
        assert!(Instant::now() < deadline, "only {taken:?}");
        taken.extend(ibus.take().inputs);
        thread::sleep(Duration::from_millis(20));
    }
    taken
}

#[test]
fn buttons_reach_the_ui_and_the_board_stops_typing_keys() {
    let (mut board, device) = pseudo_terminal();
    let ibus = IbusService::start(IbusConfig {
        enabled: true,
        device: device.display().to_string(),
        ..IbusConfig::default()
    });
    wait_for(&ibus, |v| v.status == "No I-Bus traffic");
    // The board is asked to stop typing keys itself.
    assert_eq!(sent_to_board(&mut board), "KEYS OFF\n");
    // The knob pressed, and turned two detents to the left.
    board
        .write_all(b"IBUS1 RX 1 F0043B480582\r\nIBUS1 RX 2 F0043B490284\r\n")
        .unwrap();
    let taken = wait_for_inputs(&ibus, 6);
    let presses = taken
        .iter()
        .filter(|input| input.pressed)
        .map(|input| input.action)
        .collect::<Vec<_>>();
    assert_eq!(
        presses,
        [InputAction::Select, InputAction::Left, InputAction::Left]
    );
    assert_eq!(ibus.snapshot().bad_lines, 0);
}

#[test]
fn the_ignition_off_starts_the_shutdown_delay_and_coming_back_ends_it() {
    let start = Instant::now();
    let at = |seconds: u64| start + Duration::from_secs(seconds);
    let mut state = State {
        shutdown_delay: Some(Duration::from_secs(10)),
        ..State::default()
    };
    state.frame(Some(Reading::Ignition(Ignition::On)), at(0));
    assert_eq!(state.ignition_off_at, None);
    state.frame(Some(Reading::Ignition(Ignition::Off)), at(1));
    // Repeated, it still counts from the first.
    state.frame(Some(Reading::Ignition(Ignition::Off)), at(3));
    assert_eq!(state.ignition_off_at, Some(at(1)));
    // The bus falling quiet or the interface going away changes nothing.
    state.closed("Interface lost".into());
    assert_eq!(state.ignition_off_at, Some(at(1)));
    // Position 1, as for the radio, is not off.
    state.frame(Some(Reading::Ignition(Ignition::Accessory)), at(4));
    assert_eq!(state.ignition_off_at, None);
}

#[test]
fn the_shutdown_is_due_once_the_ignition_has_stayed_off() {
    let (mut board, device) = pseudo_terminal();
    let ibus = IbusService::start(IbusConfig {
        enabled: true,
        device: device.display().to_string(),
        shutdown_delay_s: 0,
        ..IbusConfig::default()
    });
    wait_for(&ibus, |v| v.status == "No I-Bus traffic");
    assert!(!ibus.shutdown_due());
    // The cluster: ignition off, then back on for an engine restart.
    board.write_all(b"IBUS1 RX 1 8004BF11002A\r\n").unwrap();
    wait_for(&ibus, |v| v.ignition == Some(Ignition::Off));
    assert!(ibus.shutdown_due());
    board.write_all(b"IBUS1 RX 2 8004BF110329\r\n").unwrap();
    wait_for(&ibus, |v| v.ignition == Some(Ignition::On));
    assert!(!ibus.shutdown_due());
    // With shutdown off in core.toml, never.
    let (mut board, device) = pseudo_terminal();
    let kept_on = IbusService::start(IbusConfig {
        enabled: true,
        device: device.display().to_string(),
        shutdown: false,
        shutdown_delay_s: 0,
        ..IbusConfig::default()
    });
    wait_for(&kept_on, |v| v.status == "No I-Bus traffic");
    board.write_all(b"IBUS1 RX 1 8004BF11002A\r\n").unwrap();
    wait_for(&kept_on, |v| v.ignition == Some(Ignition::Off));
    assert!(!kept_on.shutdown_due());
}

#[test]
fn readings_follow_the_bus_and_expire() {
    let start = Instant::now();
    let at = |seconds: u64| start + Duration::from_secs(seconds);
    let mut state = State::default();
    assert_eq!(state.snapshot(start).status, "No I-Bus traffic");
    state.frame(Some(Reading::Speed { kmh: 50, rpm: 1500 }), start);
    let temperatures = Reading::Temperatures {
        outside: 12,
        coolant: 85,
    };
    assert!(state.frame(Some(temperatures), start));
    assert!(!state.frame(Some(temperatures), start), "unchanged");
    state.frame(Some(Reading::Range(Some(473))), start);
    assert!(state.frame(Some(Reading::Consumption(Some(132))), start));
    assert!(
        !state.frame(Some(Reading::Consumption(Some(132))), start),
        "unchanged"
    );
    assert_eq!(state.snapshot(start).handbrake, None, "not reported yet");
    let in_reverse = Reading::Sensors {
        handbrake: true,
        reverse: true,
    };
    assert!(state.frame(Some(in_reverse), start));
    assert!(!state.frame(Some(in_reverse), start), "unchanged");
    assert!(state.frame(Some(Reading::LowBeam(true)), start));
    let v = state.snapshot(at(1));
    assert_eq!(v.status, "Receiving");
    assert_eq!((v.speed_kmh, v.rpm), (Some(50), Some(1500)));
    assert_eq!((v.outside_c, v.coolant_c), (Some(12), Some(85)));
    assert_eq!(
        (v.range_km, v.consumption_dl, v.ignition, v.frames),
        (Some(473), Some(132), None, 9)
    );
    assert_eq!(
        (v.handbrake, v.reverse, v.low_beam),
        (Some(true), Some(true), Some(true))
    );

    // Other frames keep the bus alive: a speed not sent again goes stale,
    // while temperatures and the on-board computer's values stay.
    state.frame(None, at(9));
    state.frame(Some(Reading::Ignition(Ignition::On)), at(11));
    let v = state.snapshot(at(11));
    assert_eq!((v.speed_kmh, v.rpm), (None, None));
    assert_eq!((v.coolant_c, v.range_km), (Some(85), Some(473)));
    assert_eq!(v.ignition, Some(Ignition::On));
    assert_eq!(
        (v.handbrake, v.reverse),
        (Some(true), Some(true)),
        "until the cluster says otherwise"
    );
    assert_eq!(
        v.low_beam,
        Some(true),
        "until the light module says otherwise"
    );
    // A fresh speed shows while the ignition is on, not once it is off.
    state.frame(Some(Reading::Speed { kmh: 0, rpm: 800 }), at(12));
    assert_eq!(state.snapshot(at(12)).rpm, Some(800));
    state.frame(Some(Reading::Ignition(Ignition::Off)), at(13));
    assert_eq!(state.snapshot(at(13)).rpm, None);

    // A quiet bus: the car is asleep, and nothing is current.
    let v = state.snapshot(at(24));
    assert_eq!(v.status, "No I-Bus traffic");
    assert_eq!((v.ignition, v.coolant_c, v.frames), (None, None, 13));
    assert_eq!((v.range_km, v.consumption_dl), (None, None));
    assert_eq!((v.handbrake, v.reverse, v.low_beam), (None, None, None));
    assert!(v.interface, "the interface is still open");
    // Losing the interface forgets the car at once, but not the counts.
    state.bad_lines += 1;
    state.closed("Interface not connected".into());
    let v = state.snapshot(at(13));
    assert_eq!(v.status, "Interface not connected");
    assert_eq!((v.coolant_c, v.frames, v.bad_lines), (None, 13, 1));
    assert!(!v.interface);
}

/// A pseudo-terminal, standing in for the board: its controlling side,
/// and the device the service opens.
fn pseudo_terminal() -> (File, PathBuf) {
    let (mut board, mut port) = (0, 0);
    // SAFETY: two out-pointers for the new descriptors; no name, settings
    // or window size.
    let opened = unsafe {
        libc::openpty(
            &mut board,
            &mut port,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(opened, 0, "{}", io::Error::last_os_error());
    let device = fs::read_link(format!("/proc/self/fd/{port}")).unwrap();
    // SAFETY: both descriptors were just opened, and are owned here.
    let (board, port) = unsafe { (File::from_raw_fd(board), File::from_raw_fd(port)) };
    // The service opens the device by its name, as it opens the board's.
    drop(port);
    (board, device)
}

fn wait_for(ibus: &IbusService, done: impl Fn(&VehicleSnapshot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(&ibus.snapshot()) {
        assert!(Instant::now() < deadline, "stuck at {:?}", ibus.snapshot());
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_interface_is_read_line_by_line_and_its_loss_noticed() {
    let (mut board, device) = pseudo_terminal();
    let ibus = IbusService::start(IbusConfig {
        enabled: true,
        device: device.display().to_string(),
        ..IbusConfig::default()
    });
    // Open, and set to pass bytes through as they come.
    wait_for(&ibus, |v| v.status == "No I-Bus traffic");
    // The board's own message, a damaged frame, and frames, one of them
    // split across writes.
    board
        .write_all(b"IBUS ready\r\nIBUS1 RX 1 8004BF110328\r\nIBUS1 RX 2 8004BF110329\r\nIBUS1 RX 3 8005BF18")
        .unwrap();
    board
        .write_all(b"3C1E00\r\nIBUS1 RX 4 8005BF19F65A8F\r\n")
        .unwrap();
    wait_for(&ibus, |v| v.coolant_c.is_some());
    let v = ibus.snapshot();
    assert_eq!(
        (v.status.as_str(), v.frames, v.bad_lines),
        ("Receiving", 3, 1)
    );
    assert_eq!(v.ignition, Some(Ignition::On));
    assert_eq!((v.speed_kmh, v.rpm), (Some(120), Some(3000)));
    assert_eq!((v.outside_c, v.coolant_c), (Some(-10), Some(90)));
    // New readings call for a snapshot at once; the same ones again do not.
    assert!(ibus.take().changed);
    board.write_all(b"IBUS1 RX 5 8005BF183C1E00\r\n").unwrap();
    wait_for(&ibus, |v| v.frames == 4);
    assert_eq!(ibus.take(), Heard::default());
    // Unplugged: what the car said no longer holds.
    drop(board);
    wait_for(&ibus, |v| v.status != "Receiving" && v.ignition.is_none());
    assert_eq!(ibus.snapshot().frames, 4);
}

#[test]
fn a_missing_or_disabled_interface_says_so() {
    let missing = IbusService::start(IbusConfig {
        enabled: true,
        device: "/dev/serial/by-id/no-such-board".into(),
        ..IbusConfig::default()
    });
    wait_for(&missing, |v| v.status == "Interface not connected");
    let off = IbusService::start(IbusConfig {
        enabled: false,
        ..IbusConfig::default()
    });
    assert_eq!(off.snapshot().status, "Off in core.toml");
}

/// Reads the real interface for a few seconds and prints what it heard.
#[test]
#[ignore = "requires the I-Bus interface"]
fn live_ibus_interface() {
    let ibus = IbusService::start(IbusConfig::default());
    thread::sleep(Duration::from_secs(6));
    let vehicle = ibus.snapshot();
    println!("{vehicle:#?}");
    assert!(vehicle.frames > 0, "no I-Bus frames: {}", vehicle.status);
}

fn read(line: &str) -> Option<Reading> {
    decode(&parse_line(line).unwrap().unwrap())
}

#[test]
fn the_board_monitor_and_steering_wheel_buttons_are_decoded() {
    // A frame's line, its checksum worked out.
    let pressed = |bytes: &[u8]| {
        let sum = bytes.iter().fold(0, |sum, byte| sum ^ byte);
        let hex: String = bytes
            .iter()
            .chain([&sum])
            .map(|b| format!("{b:02X}"))
            .collect();
        button(&parse_line(&format!("IBUS1 RX 1 {hex}")).unwrap().unwrap())
    };
    let tap = |button| {
        Some(Press {
            button,
            phase: Phase::Press,
            steps: 1,
        })
    };
    // As the carrier board's firmware knows them.
    let panel = |to: u8, code: u8| [0xF0, 0x04, to, 0x48, code];
    for (frame, expected) in [
        (panel(0x68, 0x11), Button::Preset1),
        (panel(0x68, 0x01), Button::Preset2),
        (panel(0x68, 0x12), Button::Preset3),
        (panel(0x68, 0x02), Button::Preset4),
        (panel(0x68, 0x13), Button::Preset5),
        (panel(0x68, 0x03), Button::Preset6),
        (panel(0x68, 0x00), Button::Next),
        (panel(0x68, 0x10), Button::Previous),
        (panel(0x68, 0x21), Button::Am),
        (panel(0x68, 0x31), Button::Fm),
        (panel(0x68, 0x04), Button::Tone),
        (panel(0x68, 0x23), Button::Mode),
        (panel(0x68, 0x30), Button::RadioMenu),
        (panel(0xFF, 0x34), Button::Menu),
        (panel(0xFF, 0x08), Button::Telephone),
        (panel(0x3B, 0x05), Button::KnobPress),
        ([0x50, 0x04, 0x68, 0x3B, 0x01], Button::WheelNext),
        ([0x50, 0x04, 0x68, 0x3B, 0x08], Button::WheelPrevious),
        ([0x50, 0x04, 0xC8, 0x3B, 0x80], Button::WheelSendEnd),
        ([0x50, 0x04, 0xFF, 0x3B, 0x40], Button::WheelRt),
    ] {
        assert_eq!(pressed(&frame), tap(expected), "{frame:02X?}");
    }
    assert_eq!(
        pressed(&[0xF0, 0x05, 0xFF, 0x47, 0x00, 0x0F]),
        tap(Button::Select)
    );
    assert_eq!(
        pressed(&[0xF0, 0x05, 0xFF, 0x47, 0x00, 0x38]),
        tap(Button::Info)
    );
    // Held and released, by their bits.
    let phase = |frame: &[u8]| pressed(frame).map(|p| (p.button, p.phase));
    assert_eq!(
        phase(&panel(0x3B, 0x45)),
        Some((Button::KnobPress, Phase::Hold))
    );
    assert_eq!(
        phase(&panel(0x3B, 0x85)),
        Some((Button::KnobPress, Phase::Release))
    );
    assert_eq!(
        phase(&[0x50, 0x04, 0x68, 0x3B, 0x11]),
        Some((Button::WheelNext, Phase::Hold))
    );
    assert_eq!(
        phase(&[0x50, 0x04, 0x68, 0x3B, 0x21]),
        Some((Button::WheelNext, Phase::Release))
    );
    // The knob, in detents either way.
    let knob = |turn: u8| pressed(&[0xF0, 0x04, 0x3B, 0x49, turn]).map(|p| (p.button, p.steps));
    assert_eq!(knob(0x81), Some((Button::KnobRight, 1)));
    assert_eq!(knob(0x83), Some((Button::KnobRight, 3)));
    assert_eq!(knob(0x02), Some((Button::KnobLeft, 2)));
    // The cluster, and buttons NavBridge does not know, are not buttons.
    assert_eq!(pressed(&[0x80, 0x04, 0xBF, 0x11, 0x03]), None);
    assert_eq!(pressed(&panel(0x68, 0x24)), None);
}

#[test]
fn frames_heard_in_the_car_are_read() {
    // The cluster reports position 2, every 2 seconds.
    let ignition = parse_line("IBUS1 RX 7323494 8004BF110329")
        .unwrap()
        .unwrap();
    assert_eq!(
        ignition,
        Frame {
            source: 0x80,
            destination: 0xBF,
            message: vec![0x11, 0x03]
        }
    );
    assert_eq!(decode(&ignition), Some(Reading::Ignition(Ignition::On)));
    // The navigation unit and the radio: valid, but not the cluster's.
    for line in [
        "IBUS1 RX 7322469 3B03F001C9",
        "IBUS1 RX 7322493 F0043B0230FD",
        "IBUS1 RX 7322949 680B7FA81340000000001208FD",
        "IBUS1 RX 7323507 3B0580410902F4",
    ] {
        let frame = parse_line(line).unwrap().unwrap();
        assert_eq!(decode(&frame), None, "{line}");
    }
}

#[test]
fn the_clusters_readings_are_decoded() {
    // 0x3C x 2 km/h, 0x1E x 100 rpm.
    assert_eq!(
        read("IBUS1 RX 1 8005BF183C1E00"),
        Some(Reading::Speed {
            kmh: 120,
            rpm: 3000
        })
    );
    // -10 °C outside on a cold start, the engine already at 90 °C.
    assert_eq!(
        read("IBUS1 RX 2 8005bf19f65a8f"),
        Some(Reading::Temperatures {
            outside: -10,
            coolant: 90
        })
    );
    // The on-board computer's range and consumption, as heard in the car.
    assert_eq!(
        read("IBUS1 RX 3 8009FF2406002034373344"),
        Some(Reading::Range(Some(473)))
    );
    assert_eq!(
        read("IBUS1 RX 3 8009FF24040031332E3248"),
        Some(Reading::Consumption(Some(132)))
    );
    // As other clusters show them, and what is not a value in km or L/100.
    assert_eq!(tenths(b"  70", "KM"), Some(700));
    assert_eq!(tenths(b"123 KM ", "KM"), Some(1230));
    assert_eq!(tenths(b"12.3 L/100", "L/100"), Some(123));
    assert_eq!(tenths(b"----", "KM"), None);
    assert_eq!(tenths(b"  45 MLS", "KM"), None);
    assert_eq!(tenths(b"18.6 MPG", "L/100"), None);
    assert_eq!(tenths(b"", "KM"), None);
    // Handbrake on, brake pads worn, engine running, no gear: a manual
    // gearbox out of reverse.
    assert_eq!(
        read("IBUS1 RX 4 800ABF1305010000000086A4"),
        Some(Reading::Sensors {
            handbrake: true,
            reverse: false
        })
    );
    assert_eq!(
        read("IBUS1 RX 5 800ABF1304010000000086A5"),
        Some(Reading::Sensors {
            handbrake: false,
            reverse: false
        })
    );
    // An automatic one: in reverse with the handbrake on, then in park.
    assert_eq!(
        read("IBUS1 RX 6 800ABF130111000000004670"),
        Some(Reading::Sensors {
            handbrake: true,
            reverse: true
        })
    );
    assert_eq!(
        read("IBUS1 RX 7 800ABF1302B00000000047D3"),
        Some(Reading::Sensors {
            handbrake: false,
            reverse: false
        })
    );
    // The early three-byte status, in neutral.
    assert_eq!(
        read("IBUS1 RX 8 8006BF130171005A"),
        Some(Reading::Sensors {
            handbrake: true,
            reverse: false
        })
    );
    // The light module: parking lights and low beam, parking lights
    // alone, everything off.
    assert_eq!(
        read("IBUS1 RX 6 D008BF5B03000000013E"),
        Some(Reading::LowBeam(true))
    );
    assert_eq!(
        read("IBUS1 RX 7 D008BF5B01000000013C"),
        Some(Reading::LowBeam(false))
    );
    assert_eq!(
        read("IBUS1 RX 8 D008BF5B00000000013D"),
        Some(Reading::LowBeam(false))
    );
    assert_eq!(ignition(0x00), Ignition::Off);
    assert_eq!(ignition(0x01), Ignition::Accessory);
    assert_eq!(ignition(0x03), Ignition::On);
    assert_eq!(ignition(0x07), Ignition::Starting);
    // The same message id from another module is not the cluster's speed.
    let other = Frame {
        source: 0x3B,
        destination: 0xBF,
        message: vec![0x18, 0x3C, 0x1E],
    };
    assert_eq!(decode(&other), None);
    // Nor the light module's message id from another module.
    let other = Frame {
        source: 0x80,
        destination: 0xBF,
        message: vec![0x5B, 0x03],
    };
    assert_eq!(decode(&other), None);
}

#[test]
fn damaged_frames_are_errors_and_other_lines_are_skipped() {
    for damaged in [
        "IBUS1 RX 1 8004BF110328", // checksum
        "IBUS1 RX 1 8005BF110329", // length
        "IBUS1 RX 1 8004BF11032",  // half a byte
        "IBUS1 RX 1 8004BF11032G", // not hex
        "IBUS1 RX 1 +F04BF110329", // a sign is not hex either
        "IBUS1 RX 1 8002BF3D",     // too short
        "IBUS1 RX later 8004BF110329",
    ] {
        assert!(parse_line(damaged).is_err(), "{damaged}");
    }
    for other in [
        "",
        "IBUS ready",
        "IBUS1 TX 1 8004BF110329",
        "IBUS1 RX 1 8004BF110329 extra",
        "CAN1 RX 1 8004BF110329",
    ] {
        assert_eq!(parse_line(other), Ok(None), "{other}");
    }
}
