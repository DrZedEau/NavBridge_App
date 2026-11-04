use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::iap2::authentication::{answer, Step};
use super::iap2::control::*;
use super::iap2::link::*;
use super::iap2::{CONTROL_SESSION, EXTERNAL_ACCESSORY_SESSION, FILE_TRANSFER_SESSION};
use super::mfi::*;
use super::usb::*;

/// A coprocessor that keeps its registers in memory: busy for the first read
/// of the status register after a signing starts, as the real one is.
#[derive(Default)]
struct FakeChip {
    registers: HashMap<u8, Vec<u8>>,
    writes: Vec<(u8, Vec<u8>)>,
    busy: bool,
}

impl FakeChip {
    fn new(certificate: &[u8]) -> Self {
        let mut chip = Self::default();
        chip.registers.insert(REG_PROTOCOL_MAJOR, vec![3]);
        chip.registers.insert(
            REG_CERTIFICATE_LENGTH,
            (certificate.len() as u16).to_be_bytes().to_vec(),
        );
        chip.registers
            .insert(REG_CERTIFICATE_DATA, certificate.to_vec());
        chip
    }
}

impl Registers for FakeChip {
    fn read(&mut self, register: u8, length: usize) -> io::Result<Vec<u8>> {
        if register == REG_AUTH_CONTROL_STATUS && std::mem::take(&mut self.busy) {
            return Err(io::Error::other("NAK"));
        }
        let mut value = self.registers.get(&register).cloned().unwrap_or_default();
        value.resize(length, 0);
        Ok(value)
    }

    fn write(&mut self, register: u8, data: &[u8]) -> io::Result<()> {
        self.writes.push((register, data.to_vec()));
        if register == REG_AUTH_CONTROL_STATUS && data == [AUTH_START] {
            // The "signature": the challenge reversed.
            let challenge = &self.writes[self.writes.len() - 2].1;
            let signature: Vec<u8> = challenge.iter().rev().copied().collect();
            self.registers.insert(
                REG_SIGNATURE_LENGTH,
                (signature.len() as u16).to_be_bytes().to_vec(),
            );
            self.registers.insert(REG_SIGNATURE_DATA, signature);
            self.registers
                .insert(REG_AUTH_CONTROL_STATUS, vec![AUTH_DONE]);
            self.busy = true;
        }
        Ok(())
    }
}

#[test]
fn the_coprocessor_gives_its_certificate_and_signs_challenges() {
    let certificate: Vec<u8> = (0..=255).cycle().take(608).collect();
    let mut chip = Coprocessor::new(FakeChip::new(&certificate)).unwrap();
    assert_eq!(chip.protocol_major, 3);
    assert_eq!(chip.certificate().unwrap(), certificate);

    let challenge: Vec<u8> = (1..=32).collect();
    let signature = chip.sign(&challenge).unwrap();
    assert_eq!(
        signature,
        challenge.iter().rev().copied().collect::<Vec<_>>()
    );
}

#[test]
fn challenges_the_coprocessor_cannot_sign_are_refused_before_it_is_asked() {
    let mut chip = Coprocessor::new(FakeChip::new(b"certificate")).unwrap();
    for size in [0, 129] {
        let error = chip.sign(&vec![7; size]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn a_bus_without_i2c_finds_no_coprocessor() {
    let error = I2cRegisters::probe(Path::new("/dev/null")).err().unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::ENOTTY), "{error}");
}

#[test]
fn link_packets_sum_to_zero_and_count_their_header() {
    let ack = Packet {
        control: ACK,
        sequence: 0x2B,
        acknowledged: 0x11,
        session: 0,
        data: vec![],
    };
    let bytes = ack.encode();
    assert_eq!(bytes[..8], [0xFF, 0x5A, 0x00, 0x09, ACK, 0x2B, 0x11, 0x00]);
    assert_eq!(bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b)), 0);

    let data = Packet {
        session: CONTROL_SESSION,
        data: vec![1, 2, 3],
        ..ack
    };
    let bytes = data.encode();
    assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 9 + 3 + 1);
    assert_eq!(bytes[9..12], [1, 2, 3]);
    assert_eq!(bytes[12], checksum(&[1, 2, 3]));
}

#[test]
fn packets_are_found_however_the_transport_splits_them() {
    let packets = [
        Packet {
            control: SYN,
            sequence: 0,
            acknowledged: 0,
            session: 0,
            data: Synchronization::accessory().encode(),
        },
        Packet {
            control: ACK,
            sequence: 1,
            acknowledged: 7,
            session: CONTROL_SESSION,
            data: vec![0x40, 0x40, 0x00, 0x06, 0xAA, 0x00],
        },
    ];
    let mut stream = vec![0x00, 0x13];
    stream.extend_from_slice(&DETECT);
    for packet in &packets {
        stream.extend(packet.encode());
    }
    let mut expected = vec![Received::Detected];
    expected.extend(packets.iter().cloned().map(Received::Packet));
    for split in 1..stream.len() {
        let mut parser = Parser::default();
        let mut received = parser.push(&stream[..split]);
        received.extend(parser.push(&stream[split..]));
        assert_eq!(received, expected, "split at {split}");
    }
}

#[test]
fn damaged_packets_are_skipped() {
    let packet = Packet {
        control: ACK,
        sequence: 3,
        acknowledged: 2,
        session: CONTROL_SESSION,
        data: vec![9; 20],
    };
    let mut damaged = packet.encode();
    damaged[12] ^= 0x01;
    let mut parser = Parser::default();
    assert_eq!(parser.push(&damaged), vec![]);
    assert_eq!(
        parser.push(&packet.encode()),
        vec![Received::Packet(packet)]
    );
}

#[test]
fn the_accessorys_synchronization_offers_its_three_sessions() {
    let offer = Synchronization::accessory();
    let bytes = offer.encode();
    assert_eq!(bytes[0], 1, "link version");
    assert_eq!(bytes.len(), 10 + 3 * 3);
    assert_eq!(
        bytes[10..],
        [
            CONTROL_SESSION,
            0,
            1,
            EXTERNAL_ACCESSORY_SESSION,
            2,
            1,
            FILE_TRANSFER_SESSION,
            1,
            1
        ]
    );
    assert_eq!(Synchronization::decode(&bytes), Some(offer));
    assert_eq!(Synchronization::decode(&bytes[..11]), None);
    let mut other_version = bytes.clone();
    other_version[0] = 2;
    assert_eq!(Synchronization::decode(&other_version), None);
}

#[test]
fn control_messages_carry_parameters_and_groups() {
    let message = Message::new(
        IDENTIFICATION_INFORMATION,
        vec![
            Parameter::string(0, "NavBridge"),
            Parameter::u16(1, 0x1234),
            Parameter::group(2, &[Parameter::u8(0, 5), Parameter::flag(1)]),
        ],
    );
    let bytes = message.encode();
    assert_eq!(
        bytes[..6],
        [0x40, 0x40, 0x00, bytes.len() as u8, 0x1D, 0x01]
    );
    assert_eq!(bytes[6..10], [0x00, 4 + 10, 0x00, 0x00]);
    assert_eq!(bytes[10..20], *b"NavBridge\0");

    let decoded = Message::decode(&bytes).unwrap();
    assert_eq!(decoded, message);
    assert_eq!(decoded.parameter(0).unwrap().text(), Some("NavBridge"));
    assert_eq!(
        decoded.parameter(2).unwrap().members().unwrap(),
        vec![Parameter::u8(0, 5), Parameter::flag(1)]
    );
}

#[test]
fn malformed_control_messages_are_refused() {
    let bytes = Message::new(START_IDENTIFICATION, vec![Parameter::u32(0, 1)]).encode();
    assert!(Message::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut wrong_start = bytes.clone();
    wrong_start[0] = 0x41;
    assert!(Message::decode(&wrong_start).is_err());
    let mut short_parameter = bytes.clone();
    short_parameter[7] = 3;
    assert!(Message::decode(&short_parameter).is_err());
}

#[test]
fn authentication_is_answered_by_the_coprocessor() {
    let mut chip = Coprocessor::new(FakeChip::new(b"accessory certificate")).unwrap();
    let ask = Message::new(REQUEST_AUTHENTICATION_CERTIFICATE, vec![]);
    assert_eq!(
        answer(&mut chip, &ask).unwrap(),
        Some(Step::Reply(Message::new(
            AUTHENTICATION_CERTIFICATE,
            vec![Parameter::bytes(0, b"accessory certificate")]
        )))
    );

    let challenge = Message::new(
        REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
        vec![Parameter::bytes(0, &[1, 2, 3])],
    );
    assert_eq!(
        answer(&mut chip, &challenge).unwrap(),
        Some(Step::Reply(Message::new(
            AUTHENTICATION_RESPONSE,
            vec![Parameter::bytes(0, &[3, 2, 1])]
        )))
    );

    let done = Message::new(AUTHENTICATION_SUCCEEDED, vec![]);
    assert_eq!(answer(&mut chip, &done).unwrap(), Some(Step::Succeeded));
    let other = Message::new(START_IDENTIFICATION, vec![]);
    assert_eq!(answer(&mut chip, &other).unwrap(), None);
}

fn fake_device(root: &Path, name: &str, attributes: &[(&str, &str)]) -> PathBuf {
    let device = root.join(name);
    fs::create_dir_all(&device).unwrap();
    for (attribute, value) in attributes {
        fs::write(device.join(attribute), format!("{value}\n")).unwrap();
    }
    device
}

#[test]
fn iphones_are_found_in_sysfs_and_put_in_their_carplay_configuration() {
    let root = std::env::temp_dir().join(format!("navbridge-carplay-usb-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let iphone = fake_device(
        &root,
        "1-1",
        &[
            ("idVendor", "05ac"),
            ("bNumConfigurations", "6"),
            ("bConfigurationValue", "4"),
            ("serial", "00008140-00012345ABCDEF01"),
        ],
    );
    fake_device(&root, "1-1:1.0", &[("bInterfaceClass", "06")]);
    fake_device(
        &root,
        "1-2",
        &[("idVendor", "18d1"), ("bNumConfigurations", "1")],
    );
    fake_device(
        &root,
        "1-3",
        &[("idVendor", "05ac"), ("bNumConfigurations", "4")],
    );
    fake_device(
        &root,
        "usb1",
        &[("idVendor", "1d6b"), ("bNumConfigurations", "1")],
    );

    let found = iphones(&root).unwrap();
    assert_eq!(
        found,
        vec![Iphone {
            sysfs: iphone.clone(),
            serial: Some("00008140-00012345ABCDEF01".into()),
            configuration: Some(DEFAULT_CONFIGURATION),
        }]
    );
    assert!(select(&found[0], CARPLAY_CONFIGURATION).unwrap());
    assert_eq!(
        fs::read_to_string(iphone.join("bConfigurationValue")).unwrap(),
        "6"
    );
    let switched = &iphones(&root).unwrap()[0];
    assert!(!select(switched, CARPLAY_CONFIGURATION).unwrap());
    fs::remove_dir_all(root).unwrap();
}
