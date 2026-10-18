use navbridge_common::ipc_output::{OutputQueue, MAX_COMMAND_BYTES, MAX_EVENT_BYTES};
use navbridge_common::{
    AppSnapshot, ClientCommand, ServerEvent, MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH,
};
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

const MAX_CLIENTS: usize = 8;
const READ_CALLS_PER_TICK: usize = 8;

struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: OutputQueue,
}

fn append_commands(input: &mut Vec<u8>, bytes: &[u8], commands: &mut Vec<ClientCommand>) -> bool {
    input.extend_from_slice(bytes);
    while let Some(end) = input.iter().position(|&b| b == b'\n') {
        if end > MAX_COMMAND_BYTES {
            return false;
        }
        if end != 0 {
            match serde_json::from_slice(&input[..end]) {
                Ok(command) => commands.push(command),
                Err(err) => warn!(%err, "invalid IPC command"),
            }
        }
        input.drain(..=end);
    }
    input.len() <= MAX_COMMAND_BYTES
}

pub struct IpcServer {
    media_dpi: u32,
    media_fps: u32,
    listener: UnixListener,
    clients: Vec<Client>,
    socket_path: String,
}
impl IpcServer {
    /// The DPI and frame rate requested from the phone are sent in each hello.
    pub fn bind(
        socket_path: &str,
        media_dpi: u32,
        media_fps: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let path = Path::new(socket_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
        info!(socket = socket_path, "IPC server listening");
        Ok(Self {
            media_dpi,
            media_fps,
            listener,
            clients: Vec::new(),
            socket_path: socket_path.into(),
        })
    }
    pub fn accept_pending(&mut self) {
        for _ in 0..MAX_CLIENTS {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() >= MAX_CLIENTS {
                        continue;
                    }
                    if let Err(err) = stream.set_nonblocking(true) {
                        warn!(%err, "failed to configure IPC client");
                        continue;
                    }
                    let mut client = Client {
                        stream,
                        input: Vec::with_capacity(1024),
                        output: OutputQueue::default(),
                    };
                    let hello = ServerEvent::Hello {
                        version: env!("CARGO_PKG_VERSION").into(),
                        ui_width: UI_WIDTH,
                        ui_height: UI_HEIGHT,
                        media_width: MEDIA_WIDTH,
                        media_height: MEDIA_HEIGHT,
                        media_dpi: self.media_dpi,
                        media_fps: self.media_fps,
                    };
                    if client.output.enqueue(&hello, MAX_EVENT_BYTES).is_ok() {
                        self.clients.push(client);
                        info!(clients = self.clients.len(), "UI connected");
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    warn!(%err, "IPC accept failed");
                    break;
                }
            }
        }
    }
    pub fn read_commands(&mut self) -> Vec<ClientCommand> {
        let mut commands = Vec::new();
        self.clients.retain_mut(|client| {
            for _ in 0..READ_CALLS_PER_TICK {
                let mut buffer = [0u8; 2048];
                match client.stream.read(&mut buffer) {
                    Ok(0) => return false,
                    Ok(n) => {
                        if !append_commands(&mut client.input, &buffer[..n], &mut commands) {
                            warn!("disconnecting IPC client with oversized command");
                            return false;
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) => {
                        debug!(%err, "IPC client read ended");
                        return false;
                    }
                }
            }
            true
        });
        commands
    }
    pub fn flush_pending(&mut self) {
        self.clients.retain_mut(|client| {
            if let Err(err) = client.output.flush(&mut client.stream) {
                debug!(%err, "IPC client write ended");
                false
            } else {
                true
            }
        });
    }
    /// Sleep until a client connects, sends data or can take queued output,
    /// one of `wake` becomes readable, or `timeout` passes.
    pub fn wait(&self, wake: &[BorrowedFd<'_>], timeout: Duration) {
        let mut fds = vec![poll_fd(self.listener.as_fd(), libc::POLLIN)];
        fds.extend(wake.iter().map(|fd| poll_fd(*fd, libc::POLLIN)));
        fds.extend(self.clients.iter().map(|client| {
            let events = if client.output.is_empty() {
                libc::POLLIN
            } else {
                libc::POLLIN | libc::POLLOUT
            };
            poll_fd(client.stream.as_fd(), events)
        }));
        // Round up: a deadline less than 1 ms away must sleep, not spin.
        let millis = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int;
        // SAFETY: the descriptors are borrowed from sockets that outlive the call.
        // The result is not needed: the caller re-checks every source after waking.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    }
    pub fn broadcast_snapshot(&mut self, snapshot: AppSnapshot) {
        self.broadcast_event(ServerEvent::Snapshot { snapshot });
    }
    pub fn broadcast_event(&mut self, event: ServerEvent) {
        self.clients.retain_mut(|client| {
            if let Err(err) = client.output.enqueue(&event, MAX_EVENT_BYTES) {
                warn!(%err, "disconnecting IPC client after output limit");
                false
            } else {
                true
            }
        });
    }
}
impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}
fn poll_fd(fd: BorrowedFd<'_>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_commands_and_oversized_lines() {
        let mut bytes = serde_json::to_vec(&ClientCommand::RequestSnapshot).unwrap();
        bytes.push(b'\n');
        let mut input = Vec::new();
        let mut commands = Vec::new();
        assert!(append_commands(&mut input, &bytes[..4], &mut commands));
        assert!(commands.is_empty());
        assert!(append_commands(&mut input, &bytes[4..], &mut commands));
        assert!(matches!(
            commands.as_slice(),
            [ClientCommand::RequestSnapshot]
        ));
        assert!(!append_commands(
            &mut input,
            &vec![b'x'; MAX_COMMAND_BYTES + 1],
            &mut commands
        ));
        input.clear();
        let mut oversized = vec![b'x'; MAX_COMMAND_BYTES + 1];
        oversized.push(b'\n');
        assert!(!append_commands(&mut input, &oversized, &mut commands));
    }
    #[test]
    fn connected_clients_are_capped() {
        let path = format!("/tmp/navbridge-ipc-limit-{}.sock", std::process::id());
        let mut server = IpcServer::bind(&path, 160, 60).unwrap();
        let mut peers = Vec::new();
        for _ in 0..MAX_CLIENTS + 2 {
            peers.push(UnixStream::connect(&path).unwrap());
            server.accept_pending();
        }
        assert_eq!(server.clients.len(), MAX_CLIENTS);
        server.flush_pending();
    }
    fn woke_early(server: &IpcServer, wake: &[BorrowedFd<'_>]) -> bool {
        let started = std::time::Instant::now();
        server.wait(wake, Duration::from_secs(5));
        started.elapsed() < Duration::from_secs(1)
    }
    #[test]
    fn wait_sleeps_until_a_client_or_a_wake_socket_needs_attention() {
        use std::io::Write;
        let path = format!("/tmp/navbridge-ipc-wait-{}.sock", std::process::id());
        let mut server = IpcServer::bind(&path, 160, 60).unwrap();
        let (mut event, events) = UnixStream::pair().unwrap();
        let (mut stop, stops) = UnixStream::pair().unwrap();
        let wake = [events.as_fd(), stops.as_fd()];
        let started = std::time::Instant::now();
        server.wait(&wake, Duration::from_millis(50));
        assert!(
            started.elapsed() >= Duration::from_millis(45),
            "idle wait returned early"
        );
        let mut client = UnixStream::connect(&path).unwrap();
        assert!(
            woke_early(&server, &wake),
            "new client did not wake the loop"
        );
        server.accept_pending();
        server.flush_pending();
        client
            .write_all(b"{\"type\":\"request_snapshot\"}\n")
            .unwrap();
        assert!(woke_early(&server, &wake), "command did not wake the loop");
        assert!(matches!(
            server.read_commands().as_slice(),
            [ClientCommand::RequestSnapshot]
        ));
        event.write_all(&[1]).unwrap();
        assert!(
            woke_early(&server, &wake),
            "projection event did not wake the loop"
        );
        (&events).read_exact(&mut [0]).unwrap();
        stop.write_all(&[1]).unwrap();
        assert!(
            woke_early(&server, &wake),
            "stop request did not wake the loop"
        );
    }
}
