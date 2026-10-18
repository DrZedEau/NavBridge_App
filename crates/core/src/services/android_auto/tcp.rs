//! Android Auto over TCP: phones arriving over the hotspot for wireless Android
//! Auto, and the head unit server that Android Auto's developer settings start
//! on the phone, forwarded with `adb forward tcp:5277 tcp:5277`. The phone
//! speaks exactly the protocol it speaks over USB.
use super::transport::deliver;
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// A phone that stops reading for this long ends the session.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Transport {
    stream: TcpStream,
    received: Receiver<io::Result<Vec<u8>>>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    pub peer: SocketAddr,
}

impl Transport {
    /// `wake` is signalled after each read, as for USB transfers, so the
    /// session thread can sleep until the phone sends something.
    pub fn connect(address: SocketAddr, wake: &SyncSender<()>) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        Self::start(stream, address, wake)
    }

    /// A phone that connected to NavBridge.
    pub fn accepted(stream: TcpStream, wake: &SyncSender<()>) -> io::Result<Self> {
        let peer = stream.peer_addr()?;
        // Accepted from a listener that does not block; reads here must.
        stream.set_nonblocking(false)?;
        Self::start(stream, peer, wake)
    }

    fn start(stream: TcpStream, peer: SocketAddr, wake: &SyncSender<()>) -> io::Result<Self> {
        // Key presses and acks are small: send them at once.
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let mut reading = stream.try_clone()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let (tx, received) = mpsc::sync_channel(64);
        let (end, wake) = (stopped.clone(), wake.clone());
        let reader = thread::Builder::new()
            .name("aa-tcp-reader".into())
            .spawn(move || {
                let mut buffer = vec![0; 16384];
                loop {
                    let result = match reading.read(&mut buffer) {
                        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
                        Ok(n) => Ok(buffer[..n].to_vec()),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => Err(e),
                    };
                    let failed = result.is_err();
                    if !deliver(&tx, &wake, &end, result) || failed {
                        return;
                    }
                }
            })?;
        Ok(Self {
            stream,
            received,
            stopped,
            reader: Some(reader),
            peer,
        })
    }

    /// Closes the connection; the phone sees the session end.
    pub fn reset(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    /// The next read queued by the reader thread, if one has arrived.
    pub fn try_read(&self) -> Option<io::Result<Vec<u8>>> {
        match self.received.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(io::ErrorKind::NotConnected.into())),
        }
    }

    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        (&self.stream).write_all(data).map_err(|e| e.to_string())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // Ends the reader's blocking read.
        self.reset();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AndroidAutoConfig;
    use crate::services::android_auto::session::Session;
    use std::net::TcpListener;
    use std::time::Instant;

    fn connected() -> (Transport, TcpStream, Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (wake, woken) = mpsc::sync_channel(1);
        let transport = Transport::connect(listener.local_addr().unwrap(), &wake).unwrap();
        let (phone, _) = listener.accept().unwrap();
        (transport, phone, woken)
    }

    /// Waits for the next read as the session thread does.
    fn next(transport: &Transport, woken: &Receiver<()>) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = transport.try_read() {
                return result;
            }
            assert!(Instant::now() < deadline, "nothing arrived");
            let _ = woken.recv_timeout(Duration::from_millis(100));
        }
    }

    #[test]
    fn the_session_reaches_the_phone_and_its_replies_wake_the_session() {
        let (transport, mut phone, woken) = connected();
        // The head unit speaks first: the version request opens every session.
        let session = Session::new(AndroidAutoConfig::default(), false, false, false).unwrap();
        let request = &session.outgoing[0];
        transport.write(request).unwrap();
        let mut received = vec![0; request.len()];
        phone.read_exact(&mut received).unwrap();
        assert_eq!(&received, request);

        phone.write_all(b"version response").unwrap();
        woken
            .recv_timeout(Duration::from_secs(2))
            .expect("the reply did not wake the session");
        let mut reply = Vec::new();
        while reply.len() < 16 {
            reply.extend(next(&transport, &woken).unwrap());
        }
        assert_eq!(reply, b"version response");

        // The phone ending the connection ends the session.
        drop(phone);
        assert!(next(&transport, &woken).is_err());
    }

    #[test]
    fn dropping_the_transport_stops_its_reader_at_once() {
        let (transport, _phone, _woken) = connected();
        let started = Instant::now();
        drop(transport);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_server_that_is_not_running_fails_fast() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (wake, _woken) = mpsc::sync_channel(1);
        let started = Instant::now();
        assert!(Transport::connect(address, &wake).is_err());
        assert!(started.elapsed() < CONNECT_TIMEOUT);
    }
}
