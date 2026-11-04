//! The Bluetooth thread: it brings NavBridge's services up and down with
//! wireless mode, opens and closes pairing, and calls paired phones in turn.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::agent::Agent;
use super::bluez::{
    adapter, describe, paired_phones, register_services, unregister_services, Device1ProxyBlocking,
    AGENT, HEADSET_GATEWAY_UUID, HEADSET_PROFILE, WIRELESS_PROFILE,
};
use super::profiles::{HeadsetProfile, WirelessProfile, HOTSPOT_WAIT};
use super::{Phone, Request, Shared, PAIRING_TIME};
use crate::services::wifi;
use navbridge_common::BluetoothPhone;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

/// How long BlueZ may take to finish pairing once the code is confirmed.
pub(super) const PAIRED_TIME: Duration = Duration::from_secs(15);

/// How often paired phones are called while no phone is connected.
pub(super) const CALL_INTERVAL: Duration = Duration::from_secs(20);

/// How long a phone that answered the call has to connect its Android Auto,
/// usually under a second, before the next paired phone is called.
pub(super) const ANSWER_TO_SETUP: Duration = Duration::from_secs(3);

/// How often the paired phones are listed again, for the status.
pub(super) const PHONES_REFRESH: Duration = Duration::from_secs(5);

pub(super) const RETRY: Duration = Duration::from_secs(10);

pub(super) const TICK: Duration = Duration::from_secs(1);

pub(super) struct Worker {
    pub(super) shared: Arc<Mutex<Shared>>,
    pub(super) connection: Option<Connection>,
    /// The Bluetooth name, as the hotspot's: NavBridge-984B.
    pub(super) name: String,
    pub(super) wireless: bool,
    /// Phones are called once the hotspot they will join is up, or while it
    /// starts, until `early_until`.
    pub(super) hotspot: bool,
    /// For the phones' setup, which waits for the hotspot.
    pub(super) watch: wifi::HotspotWatch,
    pub(super) early_until: Option<Instant>,
    pub(super) session: bool,
    /// The agent and profiles are registered with BlueZ.
    pub(super) registered: bool,
    pub(super) error: Option<String>,
    pub(super) next_setup: Instant,
    pub(super) phones: Vec<Phone>,
    pub(super) next_refresh: Instant,
    pub(super) next_call: Instant,
    pub(super) calling: Arc<AtomicBool>,
    /// A phone answered the latest round of calls.
    pub(super) answered: Arc<AtomicBool>,
    /// A phone whose code the user confirmed, until BlueZ has paired it.
    pub(super) confirmed: Option<(OwnedObjectPath, Instant)>,
}

/// The phone among `phones` at `address`, if it is paired.
pub(super) fn default_of<'a>(
    phones: &'a [BluetoothPhone],
    address: Option<&str>,
) -> Option<&'a BluetoothPhone> {
    let address = address?;
    phones
        .iter()
        .find(|phone| phone.address.eq_ignore_ascii_case(address))
}

/// The phones to call: the default one while it is paired, else every one.
pub(super) fn to_call(phones: &[Phone], default: Option<&str>) -> Vec<Phone> {
    let chosen = default.and_then(|address| {
        phones
            .iter()
            .find(|phone| phone.address.eq_ignore_ascii_case(address))
    });
    match chosen {
        Some(phone) => vec![phone.clone()],
        None => phones.to_vec(),
    }
}

/// Calls the phones one after another until one's Android Auto connects: a
/// phone that answered gets `setup_time` for that before the next is called,
/// so that two phones do not set up at once.
pub(super) fn call_in_turn<P>(
    phones: &[(P, String)],
    shared: &Mutex<Shared>,
    answered: &AtomicBool,
    setup_time: Duration,
    mut call: impl FnMut(&P) -> zbus::Result<()>,
) {
    let joining = || shared.lock().unwrap().is_joining();
    let mut answered_at: Option<Instant> = None;
    for (path, name) in phones {
        if let Some(at) = answered_at {
            while at.elapsed() < setup_time && !joining() {
                thread::sleep(Duration::from_millis(50));
            }
        }
        if joining() {
            debug!(phone = %name, "a phone is setting up wireless Android Auto: not called");
            break;
        }
        let started = Instant::now();
        let result = call(path);
        let ms = started.elapsed().as_millis() as u64;
        // Fails while the phone is away; the next round tries again.
        match result {
            Ok(()) => {
                answered.store(true, Ordering::Relaxed);
                answered_at = Some(Instant::now());
                info!(phone = %name, ms, "called the phone over Bluetooth");
            }
            Err(e) => debug!(%e, phone = %name, ms, "the phone did not answer"),
        }
    }
}

impl Worker {
    pub(super) fn run(mut self, requests: Receiver<Request>) {
        loop {
            match requests.recv_timeout(TICK) {
                Ok(Request::Update {
                    wireless,
                    hotspot,
                    session,
                }) => {
                    self.session = session;
                    if hotspot && !self.hotspot && !self.answered.load(Ordering::Relaxed) {
                        // Call at once: phones can join now. A phone that
                        // answered an early call is already setting up.
                        self.next_call = Instant::now();
                    }
                    self.hotspot = hotspot;
                    if wireless != self.wireless {
                        self.wireless = wireless;
                        if wireless {
                            self.early_until = Some(Instant::now() + HOTSPOT_WAIT);
                            self.up();
                        } else {
                            self.down();
                        }
                    }
                }
                Ok(Request::Pair) => self.open_pairing(),
                Ok(Request::ClosePairing) => {
                    self.hide();
                    self.next_call = Instant::now();
                }
                Ok(Request::Confirmed(device)) => {
                    self.confirmed = Some((device, Instant::now() + PAIRED_TIME));
                }
                Ok(Request::Call) => self.next_call = Instant::now(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if self.wireless {
                        self.down();
                    }
                    return;
                }
            }
            self.tick();
        }
    }

    pub(super) fn tick(&mut self) {
        let now = Instant::now();
        if self.wireless && !self.registered && now >= self.next_setup {
            self.up();
        }
        let (expired, pairing_over) = {
            let mut shared = self.shared.lock().unwrap();
            let expired = shared.confirm.as_ref().is_some_and(|c| now >= c.until);
            let expired = if expired { shared.confirm.take() } else { None };
            let over = shared.pairing_until.is_some_and(|until| now >= until);
            (expired, over)
        };
        if let Some(confirm) = expired {
            info!(phone = %confirm.name, "Bluetooth pairing code not confirmed in time");
            let _ = confirm.reply.try_send(false);
        }
        if pairing_over {
            self.close_pairing();
            self.next_call = now;
        }
        if let Some((device, until)) = self.confirmed.clone() {
            if self.trust(&device) {
                self.confirmed = None;
                self.close_pairing();
                self.refresh_phones();
                self.next_call = now;
            } else if now >= until {
                self.confirmed = None;
            }
        }
        // While pairing is open, paired phones are not called: the user is
        // pairing another one. Nor after Disconnect phone.
        let (joining, pairing, paused) = {
            let shared = self.shared.lock().unwrap();
            (
                shared.is_joining(),
                shared.pairing_until.is_some(),
                shared.paused,
            )
        };
        let early = self.early_until.is_some_and(|until| now < until);
        if self.registered
            && (self.hotspot || early)
            && !self.session
            && !joining
            && !pairing
            && !paused
            && now >= self.next_call
        {
            self.next_call = now + CALL_INTERVAL;
            self.call_phones();
        } else if self.registered && now >= self.next_refresh {
            self.refresh_phones();
        }
        let mut shared = self.shared.lock().unwrap();
        let names: Vec<String> = to_call(&self.phones, shared.default_phone.as_deref())
            .into_iter()
            .map(|phone| phone.name)
            .collect();
        shared.phones = self
            .phones
            .iter()
            .map(|phone| BluetoothPhone {
                address: phone.address.clone(),
                name: phone.name.clone(),
            })
            .collect();
        let pairing_left = shared
            .pairing_until
            .map(|until| until.saturating_duration_since(now))
            .filter(|left| !left.is_zero());
        let joining = shared
            .joining
            .as_ref()
            .filter(|(_, until)| now < *until)
            .map(|(name, _)| name.as_str());
        shared.status = describe(
            self.wireless,
            self.error.as_deref(),
            pairing_left,
            joining,
            shared.paused,
            &names,
            &self.name,
        );
        shared.ready = self.wireless && self.registered;
    }

    pub(super) fn up(&mut self) {
        self.next_setup = Instant::now() + RETRY;
        match self.register() {
            Ok(channel) => {
                info!(name = %self.name, channel, "Bluetooth ready for wireless Android Auto");
                self.registered = true;
                self.error = None;
                self.refresh_phones();
                self.next_call = Instant::now();
            }
            Err(e) => {
                warn!(%e, "Bluetooth setup for wireless Android Auto failed");
                self.error = Some(e.to_string());
            }
        }
    }

    /// Returns the RFCOMM channel of the Android Auto wireless service.
    pub(super) fn register(&mut self) -> zbus::Result<u8> {
        let connection = self.connection()?;
        let adapter = adapter(&connection)?;
        adapter.set_powered(true)?;
        adapter.set_alias(&self.name)?;
        register_services(&connection)
    }

    pub(super) fn down(&mut self) {
        if let Some(connection) = &self.connection {
            if let Err(e) = unregister_services(connection) {
                debug!(%e, "Bluetooth services already gone");
            }
        }
        self.close_pairing();
        self.registered = false;
        self.error = None;
        self.confirmed = None;
        let mut shared = self.shared.lock().unwrap();
        if let Some(confirm) = shared.confirm.take() {
            let _ = confirm.reply.try_send(false);
        }
        shared.joining = None;
        // Turned on again, wireless mode calls the phones afresh.
        shared.paused = false;
        shared.turned_away = None;
        shared.status.clear();
        info!("Bluetooth off for wireless Android Auto");
    }

    /// The D-Bus connection, with NavBridge's agent and profiles served on it.
    pub(super) fn connection(&mut self) -> zbus::Result<Connection> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let connection = Connection::system()?;
        {
            let server = connection.object_server();
            server.at(AGENT, Agent(self.shared.clone()))?;
            let wireless = WirelessProfile(self.shared.clone(), self.watch.clone());
            server.at(WIRELESS_PROFILE, wireless)?;
            server.at(HEADSET_PROFILE, HeadsetProfile)?;
        }
        self.connection = Some(connection.clone());
        Ok(connection)
    }

    /// Makes NavBridge visible and pairable; start_pairing has already set
    /// when that ends.
    pub(super) fn open_pairing(&mut self) {
        let Some(connection) = self.connection.clone().filter(|_| self.registered) else {
            self.shared.lock().unwrap().pairing_until = None;
            return;
        };
        let open = || -> zbus::Result<()> {
            let adapter = adapter(&connection)?;
            let seconds = PAIRING_TIME.as_secs() as u32;
            adapter.set_pairable_timeout(seconds)?;
            adapter.set_discoverable_timeout(seconds)?;
            adapter.set_pairable(true)?;
            adapter.set_discoverable(true)
        };
        match open() {
            Ok(()) => info!(name = %self.name, "Bluetooth pairing open"),
            Err(e) => {
                warn!(%e, "could not open Bluetooth pairing");
                self.shared.lock().unwrap().pairing_until = None;
            }
        }
    }

    pub(super) fn close_pairing(&mut self) {
        let was_open = self.shared.lock().unwrap().pairing_until.take().is_some();
        if was_open {
            self.hide();
        }
    }

    /// Phones can no longer find NavBridge or pair with it.
    pub(super) fn hide(&self) {
        if let Some(connection) = &self.connection {
            if let Ok(adapter) = adapter(connection) {
                let _ = adapter.set_discoverable(false);
                let _ = adapter.set_pairable(false);
            }
            info!("Bluetooth pairing closed");
        }
    }

    /// Trusts a phone once BlueZ has paired it. Returns whether it is paired.
    pub(super) fn trust(&self, device: &OwnedObjectPath) -> bool {
        let Some(connection) = &self.connection else {
            return false;
        };
        let paired = || -> zbus::Result<bool> {
            let phone = Device1ProxyBlocking::builder(connection)
                .path(device.as_str())?
                .cache_properties(CacheProperties::No)
                .build()?;
            if !phone.paired()? {
                return Ok(false);
            }
            phone.set_trusted(true)?;
            info!(phone = %phone.alias().unwrap_or_default(), "Bluetooth phone paired");
            Ok(true)
        };
        paired().unwrap_or(false)
    }

    pub(super) fn refresh_phones(&mut self) {
        self.next_refresh = Instant::now() + PHONES_REFRESH;
        let Some(connection) = &self.connection else {
            return;
        };
        match paired_phones(connection) {
            Ok(phones) => self.phones = phones,
            Err(e) => debug!(%e, "could not list paired phones"),
        }
    }

    /// Calls the paired phones as a headset, or only the default one, on a
    /// thread of its own: a phone out of range takes seconds to fail.
    pub(super) fn call_phones(&mut self) {
        self.refresh_phones();
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let default = self.shared.lock().unwrap().default_phone.clone();
        let phones: Vec<(OwnedObjectPath, String)> = to_call(&self.phones, default.as_deref())
            .into_iter()
            .map(|phone| (phone.path, phone.name))
            .collect();
        if phones.is_empty() || self.calling.swap(true, Ordering::Relaxed) {
            return;
        }
        let (calling, answered) = (self.calling.clone(), self.answered.clone());
        let shared = self.shared.clone();
        answered.store(false, Ordering::Relaxed);
        let spawned = thread::Builder::new()
            .name("bluetooth-call".into())
            .spawn(move || {
                call_in_turn(&phones, &shared, &answered, ANSWER_TO_SETUP, |path| {
                    Device1ProxyBlocking::builder(&connection)
                        .path(path.as_str())?
                        .build()?
                        .connect_profile(HEADSET_GATEWAY_UUID)
                });
                calling.store(false, Ordering::Relaxed);
            });
        if spawned.is_err() {
            self.calling.store(false, Ordering::Relaxed);
        }
    }
}
