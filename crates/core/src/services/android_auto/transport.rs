//! The link to the phone: USB accessory mode, or TCP. All carry the same
//! Android Auto bytes, and all wake the session thread after each read.
use super::{tcp, usb};
use navbridge_common::UsbDeviceInfo;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

pub enum Transport {
    Usb(usb::Transport),
    /// A phone on the hotspot: wireless Android Auto.
    Wifi(tcp::Transport),
    /// The phone's head unit server, for development.
    Tcp(tcp::Transport),
}

impl Transport {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Usb(_) => "USB",
            Self::Wifi(_) => "Wi-Fi",
            Self::Tcp(_) => "TCP",
        }
    }

    /// The USB device, which the System page shows.
    pub fn usb(&self) -> Option<&UsbDeviceInfo> {
        match self {
            Self::Usb(usb) => Some(&usb.info),
            Self::Wifi(_) | Self::Tcp(_) => None,
        }
    }

    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        match self {
            Self::Usb(usb) => usb.write(data),
            Self::Wifi(tcp) | Self::Tcp(tcp) => tcp.write(data),
        }
    }

    /// The next read from the phone, if one has arrived. An error ends the
    /// session.
    pub fn try_read(&self) -> Option<Result<Vec<u8>, String>> {
        match self {
            Self::Usb(usb) => usb
                .try_read()
                .map(|read| read.map_err(|e| format!("Android USB disconnected: {e}"))),
            Self::Wifi(tcp) | Self::Tcp(tcp) => tcp.try_read().map(|read| {
                read.map_err(|e| format!("Android Auto {} connection ended: {e}", self.kind()))
            }),
        }
    }

    /// Makes the phone start over: USB re-enumerates, TCP closes.
    pub fn reset(&self) {
        match self {
            Self::Usb(usb) => usb.reset(),
            Self::Wifi(tcp) | Self::Tcp(tcp) => tcp.reset(),
        }
    }
}

/// Queues one read for the session thread and wakes it. While the queue is
/// full, the reader waits for the session to catch up. Returns false once the
/// session has gone or the transport is stopping.
pub fn deliver<T>(
    queue: &SyncSender<T>,
    wake: &SyncSender<()>,
    stopped: &AtomicBool,
    mut item: T,
) -> bool {
    loop {
        match queue.try_send(item) {
            Ok(()) => {
                let _ = wake.try_send(());
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                if stopped.load(Ordering::Relaxed) {
                    return false;
                }
                item = back;
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
