use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use navbridge_common::ipc_output::{MAX_COMMAND_BYTES, MAX_EVENT_BYTES};
use navbridge_common::{ClientCommand, ServerEvent};
use tracing::{debug, warn};

// Commands are written as soon as they are queued. This only bounds how long a
// connection closed by the core goes unnoticed while no command is sent.
const CLOSE_CHECK: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum IpcEvent {
    Connected,
    Disconnected(String),
    Server(ServerEvent),
}

/// Queues events for the UI thread and calls `wake` after each one, so the UI
/// takes them at once instead of on a timer.
#[derive(Clone)]
struct EventSender {
    events: Sender<IpcEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSender {
    /// Returns false once the UI has stopped listening.
    fn send(&self, event: IpcEvent) -> bool {
        if self.events.send(event).is_err() {
            return false;
        }
        (self.wake)();
        true
    }
}

/// `video_size` is the size the UI shows projection frames at, which the core
/// is asked to publish them in.
pub fn spawn(
    socket_path: String,
    reconnect_ms: u64,
    video_size: (u32, u32),
    wake: impl Fn() + Send + Sync + 'static,
) -> (Sender<ClientCommand>, Receiver<IpcEvent>) {
    let (command_tx, command_rx) = mpsc::channel::<ClientCommand>();
    let (event_tx, event_rx) = mpsc::channel::<IpcEvent>();
    let events = EventSender {
        events: event_tx,
        wake: Arc::new(wake),
    };

    thread::spawn(move || loop {
        let reason = match UnixStream::connect(&socket_path) {
            Ok(stream) => serve(stream, video_size, &command_rx, &events),
            Err(err) => err.to_string(),
        };
        if !events.send(IpcEvent::Disconnected(reason)) {
            return;
        }
        thread::sleep(Duration::from_millis(reconnect_ms.clamp(100, 10_000)));
    });

    (command_tx, event_rx)
}

/// Write each command the moment it is queued while a second thread blocks on
/// reading events, so neither direction waits for the other. Returns why the
/// connection ended, once no more events can arrive from it.
fn serve(
    mut stream: UnixStream,
    (video_width, video_height): (u32, u32),
    commands: &Receiver<ClientCommand>,
    events: &EventSender,
) -> String {
    let read_half = match stream.try_clone() {
        Ok(read_half) => read_half,
        Err(err) => return err.to_string(),
    };
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    events.send(IpcEvent::Connected);
    let reader_events = events.clone();
    let reader = thread::spawn(move || read_until_closed(read_half, &reader_events));

    let hello = ClientCommand::Hello {
        client: "navbridge-ui".into(),
    };
    let size = ClientCommand::SetFrameSize {
        width: video_width,
        height: video_height,
    };
    let mut written = send(&mut stream, &hello)
        .and_then(|()| send(&mut stream, &size))
        .and_then(|()| send(&mut stream, &ClientCommand::RequestSnapshot));
    while written.is_ok() && !reader.is_finished() {
        match commands.recv_timeout(CLOSE_CHECK) {
            Ok(command) => written = send(&mut stream, &command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Unblocks the reader when the write side ended first.
    let _ = stream.shutdown(Shutdown::Both);
    let closed = reader
        .join()
        .unwrap_or_else(|_| "IPC reader panicked".into());
    match written {
        Ok(()) => closed,
        Err(err) => {
            debug!(%err, "IPC write failed");
            err.to_string()
        }
    }
}

fn send(stream: &mut UnixStream, command: &ClientCommand) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(command).map_err(std::io::Error::other)?;
    if line.len() > MAX_COMMAND_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "IPC command too large",
        ));
    }
    line.push(b'\n');
    stream.write_all(&line)
}

fn read_until_closed(mut stream: UnixStream, events: &EventSender) -> String {
    let mut pending = Vec::new();
    loop {
        match read_events(&mut stream, &mut pending) {
            Ok(batch) => {
                for event in batch {
                    events.send(IpcEvent::Server(event));
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => {
                debug!(%err, "IPC connection lost");
                return err.to_string();
            }
        }
    }
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
    use navbridge_common::ProjectionEvent;
    struct Timeout;
    impl Read for Timeout {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(ErrorKind::TimedOut))
        }
    }
    fn phone_connected() -> ServerEvent {
        ServerEvent::ProjectionEvent {
            event: ProjectionEvent::PhoneConnected,
        }
    }
    fn is_phone_connected(event: &ServerEvent) -> bool {
        matches!(
            event,
            ServerEvent::ProjectionEvent {
                event: ProjectionEvent::PhoneConnected
            }
        )
    }
    #[test]
    fn fragmented_event_survives_timeout_and_preserves_following_event() {
        let mut encoded = serde_json::to_vec(&phone_connected()).unwrap();
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
        assert!(events.iter().all(is_phone_connected));
        assert!(pending.is_empty());
    }
    #[test]
    fn commands_go_out_while_the_reader_blocks_and_core_close_ends_it() {
        use std::io::BufRead;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (ui, core) = UnixStream::pair().unwrap();
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let woken = Arc::clone(&wakes);
        let events = EventSender {
            events: event_tx,
            wake: Arc::new(move || {
                woken.fetch_add(1, Ordering::Relaxed);
            }),
        };
        command_tx
            .send(ClientCommand::SetNightMode { enabled: true })
            .unwrap();
        let serving = thread::spawn(move || serve(ui, (400, 240), &command_rx, &events));
        // The core has sent nothing yet, so the reader is blocked throughout.
        let mut lines = std::io::BufReader::new(core.try_clone().unwrap()).lines();
        for kind in [
            "hello",
            "set_frame_size",
            "request_snapshot",
            "set_night_mode",
        ] {
            let line = lines.next().unwrap().unwrap();
            assert!(line.contains(&format!(r#""type":"{kind}""#)), "{line}");
        }
        let mut event = serde_json::to_vec(&phone_connected()).unwrap();
        event.push(b'\n');
        (&core).write_all(&event).unwrap();
        drop((lines, core));
        assert_eq!(serving.join().unwrap(), "backend closed connection");
        assert!(matches!(event_rx.try_recv(), Ok(IpcEvent::Connected)));
        assert!(matches!(event_rx.try_recv(), Ok(IpcEvent::Server(e)) if is_phone_connected(&e)));
        assert!(event_rx.try_recv().is_err());
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "one wake per event");
        drop(command_tx);
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
