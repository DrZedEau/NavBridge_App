//! Events from a service's thread for the core's main loop, which sleeps in
//! poll(): each event also writes a byte that makes the receiver's descriptor
//! readable.
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixStream,
    },
    sync::mpsc::{self, Receiver, SendError, Sender},
};

pub struct EventSender<T> {
    events: Sender<T>,
    wake: UnixStream,
}

pub struct EventReceiver<T> {
    events: Receiver<T>,
    woken: UnixStream,
}

pub fn channel<T>() -> io::Result<(EventSender<T>, EventReceiver<T>)> {
    let (wake, woken) = UnixStream::pair()?;
    wake.set_nonblocking(true)?;
    woken.set_nonblocking(true)?;
    let (events, receiver) = mpsc::channel();
    Ok((
        EventSender { events, wake },
        EventReceiver {
            events: receiver,
            woken,
        },
    ))
}

impl<T> EventSender<T> {
    pub fn send(&self, event: T) -> Result<(), SendError<T>> {
        self.events.send(event)?;
        // A full socket is already readable, so a failed write loses nothing.
        let _ = (&self.wake).write(&[1]);
        Ok(())
    }
}

impl<T> EventReceiver<T> {
    pub fn take(&self) -> Vec<T> {
        // Drain first: an event sent meanwhile keeps its own wake byte.
        let mut bytes = [0u8; 64];
        while matches!((&self.woken).read(&mut bytes), Ok(n) if n > 0) {}
        self.events.try_iter().collect()
    }

    /// Readable whenever `take` has something to return.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.woken.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_wake_the_main_loop_until_taken() {
        let (sender, receiver) = channel().unwrap();
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!((&receiver.woken).read(&mut byte).unwrap(), 1);
        assert_eq!(receiver.take(), [1, 2]);
        // A leftover byte would keep the descriptor readable and make poll() spin.
        let leftover = (&receiver.woken).read(&mut byte).unwrap_err();
        assert_eq!(leftover.kind(), io::ErrorKind::WouldBlock);
    }
}
