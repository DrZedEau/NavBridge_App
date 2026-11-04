//! The RFCOMM socket the Android Auto wireless service listens on, on a channel
//! the kernel finds free.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

/// RFCOMM sockets, as the kernel's Bluetooth stack takes them.
pub(super) const AF_BLUETOOTH: libc::c_int = 31;

pub(super) const BTPROTO_RFCOMM: libc::c_int = 3;

#[repr(C)]
pub(super) struct RfcommAddress {
    pub(super) family: libc::sa_family_t,
    pub(super) device: [u8; 6],
    pub(super) channel: u8,
}

/// Listens on `channel` of the adapter, or on one the kernel finds free for 0,
/// and returns the channel. The socket closes again on return.
pub(super) fn rfcomm_listen(device: [u8; 6], channel: u8) -> io::Result<u8> {
    let mut address = RfcommAddress {
        family: AF_BLUETOOTH as libc::sa_family_t,
        device,
        channel,
    };
    let mut length = std::mem::size_of::<RfcommAddress>() as libc::socklen_t;
    // SAFETY: a socket owned here, and an address of the length given.
    unsafe {
        let fd = libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            BTPROTO_RFCOMM,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = std::os::fd::OwnedFd::from_raw_fd(fd);
        let pointer = (&raw mut address).cast::<libc::sockaddr>();
        if libc::bind(socket.as_raw_fd(), pointer, length) < 0
            || libc::listen(socket.as_raw_fd(), 1) < 0
            || libc::getsockname(socket.as_raw_fd(), pointer, &mut length) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(address.channel)
}

pub(super) fn free_channel(device: [u8; 6]) -> io::Result<u8> {
    rfcomm_listen(device, 0)
}

/// Whether something listens on the channel; after registering, BlueZ.
pub(super) fn listened(device: [u8; 6], channel: u8) -> bool {
    matches!(
        rfcomm_listen(device, channel),
        Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE)
    )
}
