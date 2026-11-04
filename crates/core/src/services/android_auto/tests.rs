use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::listen::WirelessListener;

#[test]
fn phones_on_the_hotspot_are_accepted_as_soon_as_they_connect() {
    let (wake, _woken) = mpsc::sync_channel(1);
    // TEST-NET-1 is never assigned, like the hotspot's address while it is
    // off or starting: the listener is ready anyway.
    let mut off = WirelessListener::new(SocketAddr::from(([192, 0, 2, 1], 0)));
    assert!(off.accept(&wake).is_none());
    assert!(off.listener.is_some());
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut on = WirelessListener::new(free);
    assert!(on.accept(&wake).is_none(), "nobody has connected yet");
    // Without a phone, the wait lasts the whole pause...
    let started = Instant::now();
    on.wait(Duration::from_millis(100));
    assert!(started.elapsed() >= Duration::from_millis(100));
    // ...and a phone connecting ends it at once.
    let phone = std::net::TcpStream::connect(free).unwrap();
    let started = Instant::now();
    on.wait(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(1));
    let transport = on.accept(&wake).expect("the phone was not accepted");
    assert_eq!(transport.peer, phone.local_addr().unwrap());
}
