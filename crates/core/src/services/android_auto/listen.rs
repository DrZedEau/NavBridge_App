//! Phones over TCP: those that join the hotspot, and the phone's head unit
//! server for development.

use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};
use std::{io, thread};

use super::tcp;
use tracing::{debug, info, warn};

/// How often the head unit server is tried while it is not answering.
pub(super) const SERVER_RETRY: Duration = Duration::from_secs(5);

/// How often listening on the hotspot's address is tried again after failing,
/// such as while another program holds the port.
pub(super) const LISTEN_RETRY: Duration = Duration::from_secs(2);

/// Development: the head unit server that Android Auto's developer settings
/// start on the phone, tried while no phone is plugged in by USB.
pub(super) struct HeadUnitServer {
    pub(super) address: Option<SocketAddr>,
    pub(super) next_try: Instant,
}

/// Phones that come over the hotspot after the Bluetooth handshake. The
/// address exists only while the hotspot is up, but the socket listens on it
/// from the start, so a phone is accepted the moment it can connect, and keeps
/// working as the hotspot comes and goes.
pub(super) struct WirelessListener {
    pub(super) address: SocketAddr,
    pub(super) listener: Option<TcpListener>,
    pub(super) next_try: Instant,
}

/// A non-blocking listener on `address`, even while no interface has that
/// address (IP_FREEBIND), as the hotspot's while the hotspot starts.
pub(super) fn listen_before_address(address: SocketAddr) -> io::Result<TcpListener> {
    let SocketAddr::V4(address) = address else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an IPv4 address is needed",
        ));
    };
    let raw = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: address.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(*address.ip()).to_be(),
        },
        sin_zero: [0; 8],
    };
    let on: libc::c_int = 1;
    let on_length = std::mem::size_of_val(&on) as libc::socklen_t;
    // SAFETY: a socket owned here, given an option and an address of the
    // lengths passed.
    unsafe {
        let fd = libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = OwnedFd::from_raw_fd(fd);
        let option = (&raw const on).cast();
        let pointer = (&raw const raw).cast::<libc::sockaddr>();
        let length = std::mem::size_of_val(&raw) as libc::socklen_t;
        if libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_FREEBIND, option, on_length) < 0
            || libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, option, on_length) < 0
            || libc::bind(fd, pointer, length) < 0
            || libc::listen(fd, 8) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(TcpListener::from(socket))
    }
}

impl HeadUnitServer {
    pub(super) fn connect(&mut self, wake: &SyncSender<()>) -> Option<tcp::Transport> {
        let address = self.address?;
        let now = Instant::now();
        if now < self.next_try {
            return None;
        }
        // Also spaces out sessions that the server ends at once, such as an
        // `adb forward` whose phone has stopped its server.
        self.next_try = now + SERVER_RETRY;
        tcp::Transport::connect(address, wake)
            .inspect_err(|e| debug!(%e, %address, "head unit server not answering"))
            .ok()
    }
}

impl WirelessListener {
    pub(super) fn new(address: SocketAddr) -> Self {
        Self {
            address,
            listener: None,
            next_try: Instant::now(),
        }
    }

    pub(super) fn accept(&mut self, wake: &SyncSender<()>) -> Option<tcp::Transport> {
        if self.listener.is_none() && Instant::now() >= self.next_try {
            self.next_try = Instant::now() + LISTEN_RETRY;
            match listen_before_address(self.address) {
                Ok(listener) => {
                    info!(address = %self.address, "waiting for wireless Android Auto phones");
                    self.listener = Some(listener);
                }
                Err(e) => warn!(%e, address = %self.address, "cannot wait for wireless phones"),
            }
        }
        let (stream, _) = self.listener.as_ref()?.accept().ok()?;
        tcp::Transport::accepted(stream, wake)
            .inspect_err(|e| warn!(%e, "wireless phone connection failed"))
            .ok()
    }

    /// Sleeps for `pause`, or until a phone connects.
    pub(super) fn wait(&self, pause: Duration) {
        let Some(listener) = &self.listener else {
            thread::sleep(pause);
            return;
        };
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for the listener's open descriptor.
        unsafe { libc::poll(&mut fd, 1, pause.as_millis() as libc::c_int) };
    }
}
