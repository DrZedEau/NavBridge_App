use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use navbridge_common::ipc_output::{OutputQueue, MAX_COMMAND_BYTES, MAX_EVENT_BYTES};
use navbridge_common::{ClientCommand, ServerEvent};
use tracing::{debug, warn};

#[derive(Debug)]
pub enum IpcEvent {
    Connected,
    Disconnected(String),
    Server(ServerEvent),
}

pub fn spawn(
    socket_path: String,
    reconnect_ms: u64,
) -> (Sender<ClientCommand>, Receiver<IpcEvent>) {
    let (command_tx, command_rx) = mpsc::channel::<ClientCommand>();
    let (event_tx, event_rx) = mpsc::channel::<IpcEvent>();

    thread::spawn(move || loop {
        match UnixStream::connect(&socket_path) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(1)));
                let _ = event_tx.send(IpcEvent::Connected);

                let mut output = OutputQueue::default();
                output
                    .enqueue(
                        &ClientCommand::Hello {
                            client: "navbridge-ui".into(),
                        },
                        MAX_COMMAND_BYTES,
                    )
                    .expect("small hello");
                output
                    .enqueue(&ClientCommand::RequestSnapshot, MAX_COMMAND_BYTES)
                    .expect("small request");

                let reader_stream = match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(err) => {
                        let _ = event_tx.send(IpcEvent::Disconnected(err.to_string()));
                        continue;
                    }
                };
                let mut reader = reader_stream;
                let mut pending = Vec::new();

                'connected: loop {
                    for _ in 0..64 {
                        let Ok(command) = command_rx.try_recv() else {
                            break;
                        };
                        if let Err(err) = output.enqueue(&command, MAX_COMMAND_BYTES) {
                            let _ = event_tx.send(IpcEvent::Disconnected(err.to_string()));
                            break 'connected;
                        }
                    }

                    if let Err(err) = output.flush(&mut stream) {
                        let _ = event_tx.send(IpcEvent::Disconnected(err.to_string()));
                        break;
                    }
                    match read_events(&mut reader, &mut pending) {
                        Ok(events) => {
                            for event in events {
                                let _ = event_tx.send(IpcEvent::Server(event));
                            }
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                ErrorKind::WouldBlock
                                    | ErrorKind::TimedOut
                                    | ErrorKind::Interrupted
                            ) => {}
                        Err(err) => {
                            debug!(%err, "IPC connection lost");
                            let _ = event_tx.send(IpcEvent::Disconnected(err.to_string()));
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                let _ = event_tx.send(IpcEvent::Disconnected(err.to_string()));
            }
        }

        thread::sleep(Duration::from_millis(reconnect_ms.clamp(100, 10_000)));
    });

    (command_tx, event_rx)
}

// A timeout does not end a JSON message. Retain bytes until its newline arrives.
fn read_events(reader: &mut impl Read, pending: &mut Vec<u8>) -> std::io::Result<Vec<ServerEvent>> {
    let mut chunk = [0u8; 4096];
    let n = reader.read(&mut chunk)?;
    if n == 0 {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "backend closed connection",
        ));
    }
    pending.extend_from_slice(&chunk[..n]);
    let mut events = Vec::new();
    while let Some(end) = pending.iter().position(|&b| b == b'\n') {
        if end > MAX_EVENT_BYTES {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "IPC message too large",
            ));
        }
        match serde_json::from_slice(&pending[..end]) {
            Ok(event) => events.push(event),
            Err(err) => warn!(%err, "invalid IPC event"),
        }
        pending.drain(..=end);
    }
    if pending.len() > MAX_EVENT_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "IPC message too large",
        ));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Timeout;
    impl Read for Timeout {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(ErrorKind::TimedOut))
        }
    }
    #[test]
    fn fragmented_event_survives_timeout_and_preserves_following_event() {
        let mut encoded = serde_json::to_vec(&ServerEvent::Error {
            message: "test".into(),
        })
        .unwrap();
        encoded.push(b'\n');
        let mut pending = Vec::new();
        assert!(read_events(&mut &encoded[..8], &mut pending)
            .unwrap()
            .is_empty());
        assert_eq!(
            read_events(&mut Timeout, &mut pending).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        let tail = [encoded[8..].to_vec(), encoded.clone()].concat();
        let events = read_events(&mut &tail[..], &mut pending).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| matches!(e, ServerEvent::Error { message } if message == "test")));
        assert!(pending.is_empty());
    }
    #[test]
    fn oversized_unterminated_message_is_rejected() {
        let mut pending = vec![b'x'; 1024 * 1024];
        assert_eq!(
            read_events(&mut &b"x"[..], &mut pending)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
    }
}
