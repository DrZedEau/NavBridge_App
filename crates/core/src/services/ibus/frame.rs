//! I-Bus frames as the carrier board's microcontroller passes them on, one
//! line each. "IBUS1 RX 7323494 8004BF110329" is the bus, the direction, the
//! microcontroller's milliseconds since it started, and the frame in hex:
//! source, length of what follows, destination, message id and data, and a
//! checksum that makes the XOR of all the bytes zero.
use navbridge_common::Ignition;

/// The instrument cluster (IKE), which reports speed, temperatures and more.
const CLUSTER: u8 = 0x80;

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
    Odometer {
        km: u32,
    },
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
    if frame.source != CLUSTER {
        return None;
    }
    match *frame.message.as_slice() {
        [0x11, keys, ..] => Some(Reading::Ignition(ignition(keys))),
        [0x18, speed, rpm, ..] => Some(Reading::Speed {
            kmh: u16::from(speed) * 2,
            rpm: u16::from(rpm) * 100,
        }),
        [0x19, outside, coolant, ..] => Some(Reading::Temperatures {
            outside: outside as i8,
            coolant: coolant as i8,
        }),
        [0x17, low, middle, high, ..] => Some(Reading::Odometer {
            km: u32::from_le_bytes([low, middle, high, 0]),
        }),
        _ => None,
    }
}

/// Bit 0: position 1 (terminal R); bit 1: position 2 (terminal 15); bit 2:
/// starting (terminal 50).
fn ignition(keys: u8) -> Ignition {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(line: &str) -> Option<Reading> {
        decode(&parse_line(line).unwrap().unwrap())
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
        // 0x01E240 km, least significant byte first.
        assert_eq!(
            read("IBUS1 RX 3 800ABF1740E2010000000081"),
            Some(Reading::Odometer { km: 123_456 })
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
}
