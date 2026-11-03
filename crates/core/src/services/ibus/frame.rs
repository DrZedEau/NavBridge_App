//! I-Bus frames as the carrier board's microcontroller passes them on, one
//! line each. "IBUS1 RX 7323494 8004BF110329" is the bus, the direction, the
//! microcontroller's milliseconds since it started, and the frame in hex:
//! source, length of what follows, destination, message id and data, and a
//! checksum that makes the XOR of all the bytes zero.
use navbridge_common::Ignition;
use serde::Deserialize;

/// The instrument cluster (IKE), which reports speed, temperatures and more.
const CLUSTER: u8 = 0x80;
/// The light control module (LCM), which reports the lamps.
const LIGHTS: u8 = 0xD0;
/// The board monitor's panel (BMBT): radio presets, arrows, knob, soft keys.
const BOARD_MONITOR: u8 = 0xF0;
/// The steering wheel's buttons (MFL).
const STEERING_WHEEL: u8 = 0x50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub source: u8,
    pub destination: u8,
    /// The message id, then its data.
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    Ignition(Ignition),
    /// Road speed in steps of 2 km/h, engine speed in steps of 100 rpm.
    Speed {
        kmh: u16,
        rpm: u16,
    },
    /// Degrees Celsius.
    Temperatures {
        outside: i8,
        coolant: i8,
    },
    /// From the cluster's on-board computer, as it shows them: the range
    /// left, in km, and the average consumption ("Consumption 1"), in tenths
    /// of a litre per 100 km; `None` while it shows none, or another unit.
    Range(Option<u16>),
    Consumption(Option<u16>),
    /// From the cluster's sensor status: the handbrake is pulled, and the
    /// gearbox, manual or automatic, is in reverse.
    Sensors {
        handbrake: bool,
        reverse: bool,
    },
    /// The headlights' low beam is on, from the light module's lamp status.
    LowBeam(bool),
}

/// The frame on one of the microcontroller's lines. Lines that are not
/// received frames, such as its own messages, give `Ok(None)`; frames that
/// cannot be read, an error.
pub fn parse_line(line: &str) -> Result<Option<Frame>, String> {
    let mut fields = line.split_whitespace();
    let (Some(bus), Some("RX"), Some(time), Some(hex), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Ok(None);
    };
    if !bus.starts_with("IBUS") {
        return Ok(None);
    }
    time.parse::<u64>()
        .map_err(|_| format!("time {time:?} is not a number"))?;
    let bytes = bytes(hex).ok_or_else(|| format!("{hex:?} is not hex bytes"))?;
    frame(&bytes).map(Some)
}

fn bytes(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

fn frame(bytes: &[u8]) -> Result<Frame, String> {
    // Source, length, destination, message id, checksum.
    if bytes.len() < 5 {
        return Err(format!("{} bytes are too few for a frame", bytes.len()));
    }
    if usize::from(bytes[1]) != bytes.len() - 2 {
        return Err(format!(
            "the length byte says {} but {} bytes follow",
            bytes[1],
            bytes.len() - 2
        ));
    }
    if bytes.iter().fold(0, |sum, byte| sum ^ byte) != 0 {
        return Err("the checksum does not match".into());
    }
    Ok(Frame {
        source: bytes[0],
        destination: bytes[2],
        message: bytes[3..bytes.len() - 1].to_vec(),
    })
}

/// What a frame says about the car, for the messages NavBridge knows.
pub fn decode(frame: &Frame) -> Option<Reading> {
    match (frame.source, frame.message.as_slice()) {
        (CLUSTER, &[0x11, keys, ..]) => Some(Reading::Ignition(ignition(keys))),
        (CLUSTER, &[0x18, speed, rpm, ..]) => Some(Reading::Speed {
            kmh: u16::from(speed) * 2,
            rpm: u16::from(rpm) * 100,
        }),
        (CLUSTER, &[0x19, outside, coolant, ..]) => Some(Reading::Temperatures {
            outside: outside as i8,
            coolant: coolant as i8,
        }),
        // The on-board computer's values, as text, to every display: all of
        // them as the ignition comes on, then from time to time. The first
        // byte says which value, 0x06 the range and 0x04 consumption 1; the
        // second holds display flags.
        (CLUSTER, &[0x24, 0x06, _, ref text @ ..]) => Some(Reading::Range(
            tenths(text, "KM").and_then(|tenths| u16::try_from(tenths / 10).ok()),
        )),
        (CLUSTER, &[0x24, 0x04, _, ref text @ ..]) => Some(Reading::Consumption(
            tenths(text, "L/100").and_then(|tenths| u16::try_from(tenths).ok()),
        )),
        // The handbrake is its first byte's lowest bit; the others are
        // warnings, such as worn brake pads. The next byte's high half is the
        // gearbox's position, 1 in reverse with a manual one too, and its
        // lowest bit whether the engine runs.
        (CLUSTER, &[0x13, sensors, ref rest @ ..]) => Some(Reading::Sensors {
            handbrake: sensors & 0x01 != 0,
            reverse: rest.first().is_some_and(|drive| drive >> 4 == 0x1),
        }),
        // Sent at each change and every 30 seconds while lamps are on. Its
        // first byte: 0x01 parking lights, 0x02 low beam, 0x04 high beam.
        (LIGHTS, &[0x5B, lamps, ..]) => Some(Reading::LowBeam(lamps & 0x02 != 0)),
        _ => None,
    }
}

/// A number as the on-board computer shows it, in tenths: " 473" is 4730,
/// "13.2" is 132. Some clusters add the unit, which must then be `unit`;
/// dashes, no number or another unit (miles, mpg) give `None`.
pub(super) fn tenths(text: &[u8], unit: &str) -> Option<u32> {
    let text = std::str::from_utf8(text).ok()?;
    let text = text.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, rest) = text.split_at(end);
    if !(rest.trim().is_empty() || rest.trim().eq_ignore_ascii_case(unit)) {
        return None;
    }
    let (whole, tenth) = number.split_once('.').unwrap_or((number, "0"));
    if tenth.len() != 1 {
        return None;
    }
    whole
        .parse::<u32>()
        .ok()?
        .checked_mul(10)?
        .checked_add(tenth.parse().ok()?)
}

/// A button on the board monitor or the steering wheel, by the name
/// core.toml's [ibus.buttons] gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[serde(rename = "preset_1")]
    Preset1,
    #[serde(rename = "preset_2")]
    Preset2,
    #[serde(rename = "preset_3")]
    Preset3,
    #[serde(rename = "preset_4")]
    Preset4,
    #[serde(rename = "preset_5")]
    Preset5,
    #[serde(rename = "preset_6")]
    Preset6,
    /// The panel's arrows, > and <.
    Next,
    Previous,
    Am,
    Fm,
    Mode,
    Tone,
    Menu,
    RadioMenu,
    Telephone,
    KnobPress,
    KnobLeft,
    KnobRight,
    /// The board monitor's soft keys.
    Select,
    Info,
    /// The steering wheel's + and -, send/end, and R/T.
    WheelNext,
    WheelPrevious,
    WheelSendEnd,
    WheelRt,
}

/// A button's message: pressed, still held (repeated while it is), or
/// released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Press,
    Hold,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    pub button: Button,
    pub phase: Phase,
    /// Detents the knob turned; 1 for a button.
    pub steps: u8,
}

/// The button a frame reports, if any.
pub fn button(frame: &Frame) -> Option<Press> {
    let press = |button, code: u8, hold: u8, release: u8| {
        let phase = if code & release != 0 {
            Phase::Release
        } else if code & hold != 0 {
            Phase::Hold
        } else {
            Phase::Press
        };
        Press {
            button,
            phase,
            steps: 1,
        }
    };
    match (frame.source, frame.message.as_slice()) {
        // The panel's buttons: the low six bits name one, 0x40 marks it held
        // and 0x80 released, whichever unit it is sent to.
        (BOARD_MONITOR, &[0x48, code, ..]) => {
            Some(press(panel_button(code & 0x3F)?, code, 0x40, 0x80))
        }
        (BOARD_MONITOR, &[0x47, _, code, ..]) => {
            let button = match code & 0x3F {
                0x0F => Button::Select,
                0x38 => Button::Info,
                _ => return None,
            };
            Some(press(button, code, 0x40, 0x80))
        }
        // The knob: the top bit means clockwise, the low nibble counts detents.
        (BOARD_MONITOR, &[0x49, turn, ..]) if turn & 0x0F != 0 => Some(Press {
            button: if turn & 0x80 != 0 {
                Button::KnobRight
            } else {
                Button::KnobLeft
            },
            phase: Phase::Press,
            steps: turn & 0x0F,
        }),
        // The steering wheel: 0x10 marks a button held, 0x20 released.
        (STEERING_WHEEL, &[0x3B, code, ..]) => {
            let button = match code & !0x30 {
                0x01 => Button::WheelNext,
                0x08 => Button::WheelPrevious,
                0x80 => Button::WheelSendEnd,
                0x40 => Button::WheelRt,
                _ => return None,
            };
            Some(press(button, code, 0x10, 0x20))
        }
        _ => None,
    }
}

fn panel_button(id: u8) -> Option<Button> {
    Some(match id {
        0x00 => Button::Next,
        0x10 => Button::Previous,
        0x11 => Button::Preset1,
        0x01 => Button::Preset2,
        0x12 => Button::Preset3,
        0x02 => Button::Preset4,
        0x13 => Button::Preset5,
        0x03 => Button::Preset6,
        0x04 => Button::Tone,
        0x05 => Button::KnobPress,
        0x08 => Button::Telephone,
        0x21 => Button::Am,
        0x23 => Button::Mode,
        0x30 => Button::RadioMenu,
        0x31 => Button::Fm,
        0x34 => Button::Menu,
        _ => return None,
    })
}

/// Bit 0: position 1 (terminal R); bit 1: position 2 (terminal 15); bit 2:
/// starting (terminal 50).
pub(super) fn ignition(keys: u8) -> Ignition {
    if keys & 0x04 != 0 {
        Ignition::Starting
    } else if keys & 0x02 != 0 {
        Ignition::On
    } else if keys & 0x01 != 0 {
        Ignition::Accessory
    } else {
        Ignition::Off
    }
}
