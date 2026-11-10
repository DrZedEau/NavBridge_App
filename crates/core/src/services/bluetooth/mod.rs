//! Bluetooth for wireless Android Auto, through BlueZ over D-Bus. While
//! wireless mode is on, NavBridge offers the Android Auto wireless service and
//! a headset service, and calls paired phones as a headset: that makes Android
//! Auto on the phone connect to the wireless service, where the handshake sends
//! it to the hotspot. Phones pair only while the user has opened pairing, and
//! only once the user has confirmed on screen the code the phone shows.

mod agent;
mod bluez;
mod profiles;
mod rfcomm;
#[cfg(test)]
mod tests;
mod worker;

pub use bluez::{hands_free_address, paired};

use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::services::wifi;
use navbridge_common::{BluetoothPhone, BluetoothSnapshot, PairingRequest};
use tracing::info;
use worker::{default_of, Worker};
use zbus::zvariant::OwnedObjectPath;

/// How long phones can find NavBridge and pair once the user opens pairing.
const PAIRING_TIME: Duration = Duration::from_secs(120);

/// State shared with the D-Bus objects, which run on zbus's own thread.
#[derive(Default)]
struct Shared {
    status: String,
    /// The agent and profiles are registered while wireless mode is on.
    ready: bool,
    pairing_until: Option<Instant>,
    confirm: Option<Confirm>,
    /// A phone that got the hotspot's details, until it should have joined.
    joining: Option<(String, Instant)>,
    /// The paired phones, for the snapshot.
    phones: Vec<BluetoothPhone>,
    /// The one phone called, by address, as Settings chose it.
    default_phone: Option<String>,
    /// Disconnect phone ended the wireless session: no phone is called, and
    /// `turned_away` cannot set up, until Connect phone.
    paused: bool,
    turned_away: Option<OwnedObjectPath>,
    /// The phone that last got the hotspot's details.
    last_setup: Option<OwnedObjectPath>,
}

impl Shared {
    fn pairing(&self) -> bool {
        self.pairing_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// A phone's Android Auto connected over Bluetooth and is on its way to
    /// the hotspot.
    fn is_joining(&self) -> bool {
        self.joining
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() < *until)
    }
}

/// A pairing code waiting for the user.
struct Confirm {
    device: OwnedObjectPath,
    name: String,
    code: String,
    reply: async_channel::Sender<bool>,
    until: Instant,
}

enum Request {
    Update {
        wireless: bool,
        hotspot: bool,
        session: bool,
    },
    Pair,
    ClosePairing,
    Confirmed(OwnedObjectPath),
    /// Call the phones now, if no phone is connected.
    Call,
}

pub struct BluetoothService {
    requests: Option<Sender<Request>>,
    shared: Arc<Mutex<Shared>>,
    name: String,
    /// The last update sent, so that unchanged ones are not.
    last: Cell<Option<(bool, bool, bool)>>,
    worker: Option<JoinHandle<()>>,
}

impl BluetoothService {
    /// Nothing reaches BlueZ until wireless mode is first turned on. Only
    /// `default_phone` is called, while it is paired.
    pub fn start(hotspot: wifi::HotspotWatch, default_phone: Option<String>) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            default_phone,
            ..Shared::default()
        }));
        let (requests, received) = mpsc::channel();
        let name = hotspot.unit_name();
        let worker = Worker {
            shared: shared.clone(),
            connection: None,
            name: name.clone(),
            wireless: false,
            hotspot: false,
            watch: hotspot,
            early_until: None,
            session: false,
            registered: false,
            error: None,
            next_setup: Instant::now(),
            phones: Vec::new(),
            next_refresh: Instant::now(),
            next_call: Instant::now(),
            calling: Arc::new(AtomicBool::new(false)),
            answered: Arc::new(AtomicBool::new(false)),
            confirmed: None,
        };
        let worker = thread::Builder::new()
            .name("bluetooth".into())
            .spawn(move || worker.run(received))
            .expect("start the Bluetooth thread");
        Self {
            requests: Some(requests),
            shared,
            name,
            last: Cell::new(None),
            worker: Some(worker),
        }
    }

    /// Bluetooth is up while wireless mode is on; paired phones are called
    /// while the hotspot is up and no phone has a session.
    pub fn update(&self, wireless: bool, hotspot: bool, session: bool) {
        let state = (wireless, hotspot, session);
        if self.last.replace(Some(state)) != Some(state) {
            self.send(Request::Update {
                wireless,
                hotspot,
                session,
            });
        }
    }

    /// Opens pairing for PAIRING_TIME. The snapshot shows it open at once;
    /// the Bluetooth thread closes it again if it cannot open.
    pub fn start_pairing(&self) {
        self.shared.lock().unwrap().pairing_until = Some(Instant::now() + PAIRING_TIME);
        self.send(Request::Pair);
    }

    /// Closes pairing before its time is up, also shown at once.
    pub fn stop_pairing(&self) {
        self.shared.lock().unwrap().pairing_until = None;
        self.send(Request::ClosePairing);
    }

    /// The user's answer to the pairing code on screen.
    pub fn confirm(&self, accept: bool) {
        let Some(confirm) = self.shared.lock().unwrap().confirm.take() else {
            return;
        };
        info!(phone = %confirm.name, accept, "Bluetooth pairing code answered");
        let _ = confirm.reply.try_send(accept);
        if accept {
            self.send(Request::Confirmed(confirm.device));
        }
    }

    /// The one phone called from now on, or every paired phone; called at
    /// once if no phone is connected.
    pub fn set_default_phone(&self, address: Option<String>) {
        self.shared.lock().unwrap().default_phone = address;
        self.send(Request::Call);
    }

    /// The user ended the wireless session: no phone is called, and the one
    /// that set up last is turned away, until `connect`.
    pub fn disconnect(&self) {
        let mut shared = self.shared.lock().unwrap();
        shared.paused = true;
        shared.turned_away = shared.last_setup.clone();
        info!("wireless Android Auto paused: no phone is called");
    }

    /// Calls the phones again, at once.
    pub fn connect(&self) {
        {
            let mut shared = self.shared.lock().unwrap();
            shared.paused = false;
            shared.turned_away = None;
        }
        info!("wireless Android Auto resumed: phones are called");
        self.send(Request::Call);
    }

    pub fn snapshot(&self) -> BluetoothSnapshot {
        let shared = self.shared.lock().unwrap();
        BluetoothSnapshot {
            status: shared.status.clone(),
            ready: shared.ready,
            pairing: shared.pairing(),
            pairing_seconds: shared.pairing_until.map_or(0, |until| {
                until.saturating_duration_since(Instant::now()).as_secs() as u32
            }),
            name: self.name.clone(),
            confirm: shared.confirm.as_ref().map(|confirm| PairingRequest {
                device: confirm.name.clone(),
                code: confirm.code.clone(),
            }),
            phones: shared.phones.clone(),
            // As BlueZ writes the address, whichever case Settings kept.
            default_phone: default_of(&shared.phones, shared.default_phone.as_deref())
                .map(|phone| phone.address.clone()),
            paused: shared.paused,
        }
    }

    fn send(&self, request: Request) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }
}

impl Drop for BluetoothService {
    fn drop(&mut self) {
        // The worker then takes NavBridge's services off Bluetooth.
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A paired phone, as BlueZ lists it.
#[derive(Clone)]
struct Phone {
    path: OwnedObjectPath,
    /// As "64:9D:38:E1:B6:24".
    address: String,
    name: String,
}
