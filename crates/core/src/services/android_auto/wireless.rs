//! The Bluetooth half of wireless Android Auto. Over an RFCOMM connection the
//! head unit tells the phone its address, then gives it the hotspot's details;
//! the phone joins the hotspot and opens the Android Auto session over TCP.
//! Each message is a 2-byte length and a 2-byte id, big-endian, then protobuf.
use super::session::generated::aa as p;
use crate::services::wifi::Hotspot;
use protobuf::Message;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use tracing::debug;

/// The Bluetooth service that phones look for in a wireless head unit. Its
/// RFCOMM channel is any free one: phones look it up.
pub const SERVICE_UUID: &str = "4de17a00-52cb-11e6-bdf4-0800200c9a66";
/// Where phones open the Android Auto session once on the hotspot.
pub const PORT: u16 = 5288;
/// How long the phone may take over each answer; joining the hotspot takes
/// longest, usually a few seconds.
const ANSWER_TIME: Duration = Duration::from_secs(20);
const SEND_TIME: Duration = Duration::from_secs(5);

mod id {
    pub const START_REQUEST: u16 = 1;
    pub const INFO_REQUEST: u16 = 2;
    pub const INFO_RESPONSE: u16 = 3;
    pub const CONNECT_STATUS: u16 = 6;
}
const WPA2_PERSONAL: u32 = 8;
/// The access point is the head unit's own, as opposed to a car's fixed one.
const DYNAMIC: u32 = 1;

/// Runs the head unit's side of the setup on `link`, the RFCOMM connection
/// that BlueZ handed over.
pub fn handshake(link: &mut UnixStream, hotspot: &Hotspot) -> io::Result<()> {
    // BlueZ hands connections over non-blocking, where a read before the
    // phone answers fails at once ("Resource temporarily unavailable").
    link.set_nonblocking(false)?;
    link.set_read_timeout(Some(ANSWER_TIME))?;
    link.set_write_timeout(Some(SEND_TIME))?;
    let mut start = p::WifiStartRequest::new();
    start.set_ip_address(hotspot.address.to_string());
    start.set_port(PORT.into());
    send(link, id::START_REQUEST, &start)?;
    let (id, _) = receive(link).map_err(unanswered)?;
    if id != id::INFO_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the phone sent message {id} instead of asking for the Wi-Fi"),
        ));
    }
    let mut info = p::WifiInfoResponse::new();
    info.set_ssid(hotspot.ssid.clone());
    info.set_key(hotspot.key.clone());
    info.set_bssid(hotspot.bssid.clone());
    info.set_security_mode(WPA2_PERSONAL);
    info.set_access_point_type(DYNAMIC);
    send(link, id::INFO_RESPONSE, &info)?;
    // The phone then reports starting, then joining the network. Only a
    // failure to join matters: the session itself arrives over TCP.
    for _ in 0..2 {
        let Ok((id, body)) = receive(link) else { break };
        debug!(id, ?body, "wireless setup message");
        if id == id::CONNECT_STATUS {
            match p::WifiConnectStatus::parse_from_bytes(&body).map(|s| s.status()) {
                Ok(0) | Err(_) => break,
                Ok(status) => {
                    return Err(io::Error::other(format!(
                        "the phone could not join the hotspot (status {status})"
                    )))
                }
            }
        }
    }
    Ok(())
}

/// A read that timed out also reports "Resource temporarily unavailable".
fn unanswered(e: io::Error) -> io::Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "the phone did not ask for the Wi-Fi within {} s",
                ANSWER_TIME.as_secs()
            ),
        ),
        io::ErrorKind::UnexpectedEof => io::Error::new(
            e.kind(),
            "the phone closed the connection instead of asking for the Wi-Fi",
        ),
        _ => e,
    }
}

fn send(link: &mut impl Write, id: u16, message: &impl Message) -> io::Result<()> {
    let body = message.write_to_bytes().map_err(io::Error::other)?;
    let length = u16::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "setup message too long"))?;
    let frame = [&length.to_be_bytes()[..], &id.to_be_bytes(), &body].concat();
    link.write_all(&frame)
}

fn receive(link: &mut impl Read) -> io::Result<(u16, Vec<u8>)> {
    let mut header = [0u8; 4];
    link.read_exact(&mut header)?;
    let length = u16::from_be_bytes([header[0], header[1]]);
    let id = u16::from_be_bytes([header[2], header[3]]);
    let mut body = vec![0; length.into()];
    link.read_exact(&mut body)?;
    Ok((id, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn hotspot() -> Hotspot {
        Hotspot {
            address: Ipv4Addr::new(10, 42, 0, 1),
            ssid: "NavBridge-984B".into(),
            key: "secret".into(),
            bssid: "2c:cf:67:e1:98:4b".into(),
        }
    }

    #[test]
    fn the_phone_learns_where_to_connect_then_how_to_join_the_hotspot() {
        let (mut head_unit, mut phone) = UnixStream::pair().unwrap();
        let phone = thread::spawn(move || {
            let (id, body) = receive(&mut phone).unwrap();
            assert_eq!(id, 1);
            let start = p::WifiStartRequest::parse_from_bytes(&body).unwrap();
            assert_eq!((start.ip_address(), start.port()), ("10.42.0.1", 5288));
            // An empty Wi-Fi request, as the phone sends it.
            phone.write_all(&[0, 0, 0, 2]).unwrap();
            let (id, body) = receive(&mut phone).unwrap();
            assert_eq!(id, 3);
            let info = p::WifiInfoResponse::parse_from_bytes(&body).unwrap();
            assert_eq!(info.ssid(), "NavBridge-984B");
            assert_eq!(info.key(), "secret");
            assert_eq!(info.bssid(), "2c:cf:67:e1:98:4b");
            assert_eq!((info.security_mode(), info.access_point_type()), (8, 1));
            // Joined, then connected.
            phone.write_all(&[0, 0, 0, 7, 0, 0, 0, 6]).unwrap();
        });
        handshake(&mut head_unit, &hotspot()).unwrap();
        phone.join().unwrap();
    }

    #[test]
    fn a_phone_that_does_not_ask_for_the_wifi_gets_nothing() {
        let (mut head_unit, mut phone) = UnixStream::pair().unwrap();
        let phone = thread::spawn(move || {
            receive(&mut phone).unwrap();
            // A version request instead of the Wi-Fi request.
            phone.write_all(&[0, 0, 0, 4]).unwrap();
            let mut rest = Vec::new();
            phone.read_to_end(&mut rest).unwrap();
            rest
        });
        assert!(handshake(&mut head_unit, &hotspot()).is_err());
        drop(head_unit);
        assert!(phone.join().unwrap().is_empty(), "the hotspot key was sent");
    }

    #[test]
    fn a_link_handed_over_non_blocking_waits_for_the_phone() {
        let (mut head_unit, mut phone) = UnixStream::pair().unwrap();
        // As BlueZ hands it over.
        head_unit.set_nonblocking(true).unwrap();
        let phone = thread::spawn(move || {
            receive(&mut phone).unwrap();
            // Answers take a moment over Bluetooth.
            thread::sleep(Duration::from_millis(200));
            phone.write_all(&[0, 0, 0, 2]).unwrap();
            receive(&mut phone).unwrap();
            // Started, then joined: status 0.
            phone.write_all(&[0, 0, 0, 7, 0, 2, 0, 6, 8, 0]).unwrap();
        });
        handshake(&mut head_unit, &hotspot()).unwrap();
        phone.join().unwrap();
    }

    #[test]
    fn a_phone_that_cannot_join_the_hotspot_says_so() {
        let (mut head_unit, mut phone) = UnixStream::pair().unwrap();
        let phone = thread::spawn(move || {
            receive(&mut phone).unwrap();
            phone.write_all(&[0, 0, 0, 2]).unwrap();
            receive(&mut phone).unwrap();
            phone.write_all(&[0, 0, 0, 7]).unwrap();
            // Status -3, as a phone that could not join sends it.
            let status = [8, 0xfd, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1];
            phone.write_all(&[0, 11, 0, 6]).unwrap();
            phone.write_all(&status).unwrap();
        });
        let e = handshake(&mut head_unit, &hotspot()).unwrap_err();
        assert_eq!(
            e.to_string(),
            "the phone could not join the hotspot (status -3)"
        );
        phone.join().unwrap();
    }

    #[test]
    fn a_silent_phone_is_named_as_such() {
        let e = unanswered(io::ErrorKind::WouldBlock.into());
        assert_eq!(
            e.to_string(),
            "the phone did not ask for the Wi-Fi within 20 s"
        );
    }
}
