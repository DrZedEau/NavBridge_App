//! iAP2's link layer: the packets that carry the sessions' messages between
//! the iPhone and the accessory, over USB or Bluetooth.
//!
//! A link starts with the accessory sending the detect marker until the
//! iPhone echoes it, then a SYN packet naming the link's limits and the
//! sessions it carries, which the iPhone answers with its own SYN. Each packet
//! has a 9-byte header (start word `FF 5A`, length, control bits, sequence and
//! acknowledgement numbers, session id, checksum) and, if it carries data, the
//! data and a checksum of it. A checksum makes the bytes it covers sum to
//! zero.

/// Sent until the iPhone echoes it: "this accessory speaks iAP2".
pub const DETECT: [u8; 6] = [0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10];

const START: [u8; 2] = [0xFF, 0x5A];
pub const HEADER_SIZE: usize = 9;

pub const SYN: u8 = 0x80;
pub const ACK: u8 = 0x40;
/// Extended acknowledgement: names packets received out of order.
pub const EAK: u8 = 0x20;
pub const RST: u8 = 0x10;
pub const SLP: u8 = 0x08;

/// The checksum byte that makes `bytes` and it sum to zero.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |sum, &b| sum.wrapping_add(b))
        .wrapping_neg()
}

fn sums_to_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// SYN, ACK, EAK, RST and SLP bits.
    pub control: u8,
    pub sequence: u8,
    pub acknowledged: u8,
    /// The session the data belongs to; 0 for link-only packets.
    pub session: u8,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let length = HEADER_SIZE
            + if self.data.is_empty() {
                0
            } else {
                self.data.len() + 1
            };
        let mut packet = Vec::with_capacity(length);
        packet.extend_from_slice(&START);
        packet.extend_from_slice(&(length as u16).to_be_bytes());
        packet.extend_from_slice(&[self.control, self.sequence, self.acknowledged, self.session]);
        packet.push(checksum(&packet));
        if !self.data.is_empty() {
            packet.extend_from_slice(&self.data);
            packet.push(checksum(&self.data));
        }
        packet
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Received {
    /// The iPhone echoed the detect marker.
    Detected,
    Packet(Packet),
}

/// Finds packets in the bytes the transport delivers, however it splits them.
/// Bytes that start no packet, and packets whose checksums fail, are skipped.
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Received> {
        self.pending.extend_from_slice(bytes);
        let mut received = Vec::new();
        loop {
            if self.pending.starts_with(&DETECT) {
                self.pending.drain(..DETECT.len());
                received.push(Received::Detected);
                continue;
            }
            // Skip to what could start a packet or the marker.
            match self.pending.iter().position(|&b| b == 0xFF) {
                Some(0) => {}
                Some(at) => {
                    self.pending.drain(..at);
                    continue;
                }
                None => {
                    self.pending.clear();
                    return received;
                }
            }
            if self.pending.len() < 2 {
                return received;
            }
            if self.pending[1] != START[1] {
                if self.pending.len() < DETECT.len() && DETECT.starts_with(&self.pending) {
                    // The marker, not all here yet.
                    return received;
                }
                self.pending.remove(0);
                continue;
            }
            if self.pending.len() < HEADER_SIZE {
                return received;
            }
            let header = &self.pending[..HEADER_SIZE];
            let length = u16::from_be_bytes([header[2], header[3]]) as usize;
            if !sums_to_zero(header) || length < HEADER_SIZE || length == HEADER_SIZE + 1 {
                self.pending.remove(0);
                continue;
            }
            if self.pending.len() < length {
                return received;
            }
            let packet: Vec<u8> = self.pending.drain(..length).collect();
            if length > HEADER_SIZE && !sums_to_zero(&packet[HEADER_SIZE..]) {
                continue;
            }
            received.push(Received::Packet(Packet {
                control: packet[4],
                sequence: packet[5],
                acknowledged: packet[6],
                session: packet[7],
                data: packet
                    .get(HEADER_SIZE..length - 1)
                    .unwrap_or_default()
                    .to_vec(),
            }));
        }
    }
}

/// What a session carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionKind {
    Control = 0,
    FileTransfer = 1,
    ExternalAccessory = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: u8,
    pub kind: u8,
    pub version: u8,
}

/// A SYN packet's data: the limits the sender works within.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Synchronization {
    /// Packets sent before waiting for an acknowledgement.
    pub max_outstanding: u8,
    pub max_packet_length: u16,
    /// Milliseconds before an unacknowledged packet is sent again.
    pub retransmission_timeout: u16,
    /// Milliseconds a received packet may wait for its acknowledgement.
    pub cumulative_ack_timeout: u16,
    pub max_retransmissions: u8,
    pub max_cumulative_acks: u8,
    pub sessions: Vec<Session>,
}

impl Synchronization {
    const VERSION: u8 = 1;

    /// The accessory's offer: the control session (id 10), the external
    /// accessory one (11) and file transfer (12).
    pub fn accessory() -> Self {
        Self {
            max_outstanding: 30,
            max_packet_length: u16::MAX,
            retransmission_timeout: 4000,
            cumulative_ack_timeout: 500,
            max_retransmissions: 4,
            max_cumulative_acks: 3,
            sessions: vec![
                Session {
                    id: super::CONTROL_SESSION,
                    kind: SessionKind::Control as u8,
                    version: 1,
                },
                Session {
                    id: super::EXTERNAL_ACCESSORY_SESSION,
                    kind: SessionKind::ExternalAccessory as u8,
                    version: 1,
                },
                Session {
                    id: super::FILE_TRANSFER_SESSION,
                    kind: SessionKind::FileTransfer as u8,
                    version: 1,
                },
            ],
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut data = vec![Self::VERSION, self.max_outstanding];
        data.extend_from_slice(&self.max_packet_length.to_be_bytes());
        data.extend_from_slice(&self.retransmission_timeout.to_be_bytes());
        data.extend_from_slice(&self.cumulative_ack_timeout.to_be_bytes());
        data.extend_from_slice(&[self.max_retransmissions, self.max_cumulative_acks]);
        for session in &self.sessions {
            data.extend_from_slice(&[session.id, session.kind, session.version]);
        }
        data
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < 10 || data[0] != Self::VERSION || !(data.len() - 10).is_multiple_of(3) {
            return None;
        }
        let word = |at: usize| u16::from_be_bytes([data[at], data[at + 1]]);
        Some(Self {
            max_outstanding: data[1],
            max_packet_length: word(2),
            retransmission_timeout: word(4),
            cumulative_ack_timeout: word(6),
            max_retransmissions: data[8],
            max_cumulative_acks: data[9],
            sessions: data[10..]
                .chunks_exact(3)
                .map(|s| Session {
                    id: s[0],
                    kind: s[1],
                    version: s[2],
                })
                .collect(),
        })
    }
}
