//! Bluetooth for wireless Android Auto, through BlueZ over D-Bus. While
//! wireless mode is on, NavBridge offers the Android Auto wireless service and
//! a headset service, and calls paired phones as a headset: that makes Android
//! Auto on the phone connect to the wireless service, where the handshake sends
//! it to the hotspot. Phones pair only while the user has opened pairing, and
//! only once the user has confirmed on screen the code the phone shows.
use crate::services::{android_auto::wireless, wifi};
use navbridge_common::{BluetoothSnapshot, PairingRequest};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};
use zbus::{
    blocking::Connection,
    proxy::CacheProperties,
    zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue, Value},
};

const AGENT: &str = "/org/navbridge/agent";
const WIRELESS_PROFILE: &str = "/org/navbridge/wireless";
const HEADSET_PROFILE: &str = "/org/navbridge/headset";
/// A headset: the role NavBridge takes to call a phone.
const HEADSET_UUID: &str = "00001108-0000-1000-8000-00805f9b34fb";
/// The phone's side of a headset or hands-free connection.
const HEADSET_GATEWAY_UUID: &str = "00001112-0000-1000-8000-00805f9b34fb";
const HANDS_FREE_GATEWAY_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";
/// How long phones can find NavBridge and pair once the user opens pairing.
const PAIRING_TIME: Duration = Duration::from_secs(120);
/// A code not confirmed by then is refused; phones give up about then too.
const CONFIRM_TIME: Duration = Duration::from_secs(30);
/// How long BlueZ may take to finish pairing once the code is confirmed.
const PAIRED_TIME: Duration = Duration::from_secs(15);
/// How often paired phones are called while no phone is connected.
const CALL_INTERVAL: Duration = Duration::from_secs(20);
/// How long a phone that answered the call has to connect its Android Auto,
/// usually under a second, before the next paired phone is called.
const ANSWER_TO_SETUP: Duration = Duration::from_secs(3);
/// Once wireless mode is on, phones are called this long before the hotspot
/// is up, as the call and the hotspot both take seconds; a phone's setup waits
/// as long for the hotspot before sending it there.
const HOTSPOT_WAIT: Duration = Duration::from_secs(15);
/// How long a phone setting up wireless Android Auto is shown joining the
/// hotspot. No phone is called meanwhile: a call could restart its setup.
const JOINING_TIME: Duration = Duration::from_secs(30);
/// How often the paired phones are listed again, for the status.
const PHONES_REFRESH: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_secs(1);
/// RFCOMM sockets, as the kernel's Bluetooth stack takes them.
const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_RFCOMM: libc::c_int = 3;

#[zbus::proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0",
    gen_async = false,
    blocking_name = "Adapter1ProxyBlocking"
)]
trait Adapter1 {
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_alias(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_discoverable(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable_timeout(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_discoverable_timeout(&self, value: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn pairable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_pairable(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn pairable_timeout(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_pairable_timeout(&self, value: u32) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device1 {
    fn connect_profile(&self, uuid: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn trusted(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, value: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez",
    gen_async = false,
    blocking_name = "AgentManager1ProxyBlocking"
)]
trait AgentManager1 {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.bluez.ProfileManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez",
    gen_async = false,
    blocking_name = "ProfileManager1ProxyBlocking"
)]
trait ProfileManager1 {
    fn register_profile(
        &self,
        profile: &ObjectPath<'_>,
        uuid: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// The answer BlueZ passes on to a phone whose request NavBridge refuses.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum Refusal {
    Rejected(String),
}

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
    /// Nothing reaches BlueZ until wireless mode is first turned on.
    pub fn start(hotspot: wifi::HotspotWatch) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (requests, received) = mpsc::channel();
        let name = wifi::unit_name();
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

struct Worker {
    shared: Arc<Mutex<Shared>>,
    connection: Option<Connection>,
    /// The Bluetooth name, as the hotspot's: NavBridge-984B.
    name: String,
    wireless: bool,
    /// Phones are called once the hotspot they will join is up, or while it
    /// starts, until `early_until`.
    hotspot: bool,
    /// For the phones' setup, which waits for the hotspot.
    watch: wifi::HotspotWatch,
    early_until: Option<Instant>,
    session: bool,
    /// The agent and profiles are registered with BlueZ.
    registered: bool,
    error: Option<String>,
    next_setup: Instant,
    /// Paired phones: their object paths and names.
    phones: Vec<(OwnedObjectPath, String)>,
    next_refresh: Instant,
    next_call: Instant,
    calling: Arc<AtomicBool>,
    /// A phone answered the latest round of calls.
    answered: Arc<AtomicBool>,
    /// A phone whose code the user confirmed, until BlueZ has paired it.
    confirmed: Option<(OwnedObjectPath, Instant)>,
}

impl Worker {
    fn run(mut self, requests: Receiver<Request>) {
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

    fn tick(&mut self) {
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
        // pairing another one.
        let (joining, pairing) = {
            let shared = self.shared.lock().unwrap();
            (shared.is_joining(), shared.pairing_until.is_some())
        };
        let early = self.early_until.is_some_and(|until| now < until);
        if self.registered
            && (self.hotspot || early)
            && !self.session
            && !joining
            && !pairing
            && now >= self.next_call
        {
            self.next_call = now + CALL_INTERVAL;
            self.call_phones();
        } else if self.registered && now >= self.next_refresh {
            self.refresh_phones();
        }
        let names: Vec<String> = self.phones.iter().map(|(_, name)| name.clone()).collect();
        let mut shared = self.shared.lock().unwrap();
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
            &names,
            &self.name,
        );
        shared.ready = self.wireless && self.registered;
    }

    fn up(&mut self) {
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
    fn register(&mut self) -> zbus::Result<u8> {
        let connection = self.connection()?;
        let adapter = adapter(&connection)?;
        adapter.set_powered(true)?;
        adapter.set_alias(&self.name)?;
        register_services(&connection)
    }

    fn down(&mut self) {
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
        shared.status.clear();
        info!("Bluetooth off for wireless Android Auto");
    }

    /// The D-Bus connection, with NavBridge's agent and profiles served on it.
    fn connection(&mut self) -> zbus::Result<Connection> {
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
    fn open_pairing(&mut self) {
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

    fn close_pairing(&mut self) {
        let was_open = self.shared.lock().unwrap().pairing_until.take().is_some();
        if was_open {
            self.hide();
        }
    }

    /// Phones can no longer find NavBridge or pair with it.
    fn hide(&self) {
        if let Some(connection) = &self.connection {
            if let Ok(adapter) = adapter(connection) {
                let _ = adapter.set_discoverable(false);
                let _ = adapter.set_pairable(false);
            }
            info!("Bluetooth pairing closed");
        }
    }

    /// Trusts a phone once BlueZ has paired it. Returns whether it is paired.
    fn trust(&self, device: &OwnedObjectPath) -> bool {
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

    fn refresh_phones(&mut self) {
        self.next_refresh = Instant::now() + PHONES_REFRESH;
        let Some(connection) = &self.connection else {
            return;
        };
        match paired_phones(connection) {
            Ok(phones) => self.phones = phones,
            Err(e) => debug!(%e, "could not list paired phones"),
        }
    }

    /// Calls the paired phones as a headset, on a thread of its own: a phone
    /// out of range takes seconds to fail.
    fn call_phones(&mut self) {
        self.refresh_phones();
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let phones = self.phones.clone();
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

/// Calls the phones one after another until one's Android Auto connects: a
/// phone that answered gets `setup_time` for that before the next is called,
/// so that two phones do not set up at once.
fn call_in_turn<P>(
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

fn adapter(connection: &Connection) -> zbus::Result<Adapter1ProxyBlocking<'static>> {
    Adapter1ProxyBlocking::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
}

/// Registers the agent and both profiles with BlueZ, replacing an earlier
/// registration. Returns the RFCOMM channel of the Android Auto wireless
/// service: one the kernel finds free, as BlueZ's own services hold some. Its
/// SIM access service holds 8, the channel wireless dongles use.
fn register_services(connection: &Connection) -> zbus::Result<u8> {
    let _ = unregister_services(connection);
    let address = adapter(connection)?.address()?;
    let device = bluetooth_address(&address)
        .ok_or_else(|| zbus::Error::Failure(format!("unexpected adapter address {address}")))?;
    let channel = free_channel(device)
        .map_err(|e| zbus::Error::Failure(format!("no free RFCOMM channel: {e}")))?;
    let profiles = ProfileManager1ProxyBlocking::new(connection)?;
    let options = |name: &'static str| {
        HashMap::from([
            ("Name", Value::from(name)),
            // Only paired phones, over an encrypted link.
            ("RequireAuthentication", Value::from(true)),
            ("RequireAuthorization", Value::from(false)),
        ])
    };
    let mut android_auto = options("Android Auto Wireless");
    android_auto.insert("Role", Value::from("server"));
    android_auto.insert("Channel", Value::from(u16::from(channel)));
    profiles.register_profile(
        &ObjectPath::try_from(WIRELESS_PROFILE)?,
        wireless::SERVICE_UUID,
        android_auto,
    )?;
    // BlueZ only logs a channel it cannot listen on, and phones would then
    // reach whatever holds it.
    if !listened(device, channel) {
        return Err(zbus::Error::Failure(format!(
            "BlueZ is not listening on RFCOMM channel {channel}"
        )));
    }
    // Only used to call phones, so its own listening channel does not matter.
    tolerate_existing(profiles.register_profile(
        &ObjectPath::try_from(HEADSET_PROFILE)?,
        HEADSET_UUID,
        options("Headset"),
    ))?;
    let agents = AgentManager1ProxyBlocking::new(connection)?;
    let agent = ObjectPath::try_from(AGENT)?;
    // The phone shows a code; the user compares it and confirms here.
    agents.register_agent(&agent, "DisplayYesNo")?;
    agents.request_default_agent(&agent)?;
    Ok(channel)
}

/// The adapter's address as RFCOMM sockets take it, least significant byte
/// first: 2C:CF:67:E1:98:4D becomes 4D 98 E1 67 CF 2C.
fn bluetooth_address(text: &str) -> Option<[u8; 6]> {
    let mut bytes = [0u8; 6];
    let mut parts = text.split(':');
    for byte in bytes.iter_mut().rev() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(bytes)
}

#[repr(C)]
struct RfcommAddress {
    family: libc::sa_family_t,
    device: [u8; 6],
    channel: u8,
}

/// Listens on `channel` of the adapter, or on one the kernel finds free for 0,
/// and returns the channel. The socket closes again on return.
fn rfcomm_listen(device: [u8; 6], channel: u8) -> io::Result<u8> {
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

fn free_channel(device: [u8; 6]) -> io::Result<u8> {
    rfcomm_listen(device, 0)
}

/// Whether something listens on the channel; after registering, BlueZ.
fn listened(device: [u8; 6], channel: u8) -> bool {
    matches!(
        rfcomm_listen(device, channel),
        Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE)
    )
}

fn unregister_services(connection: &Connection) -> zbus::Result<()> {
    let agents = AgentManager1ProxyBlocking::new(connection)?;
    let agent = agents.unregister_agent(&ObjectPath::try_from(AGENT)?);
    let profiles = ProfileManager1ProxyBlocking::new(connection)?;
    let wireless = profiles.unregister_profile(&ObjectPath::try_from(WIRELESS_PROFILE)?);
    let headset = profiles.unregister_profile(&ObjectPath::try_from(HEADSET_PROFILE)?);
    agent.and(wireless).and(headset)
}

/// Registering again what is still registered is fine.
fn tolerate_existing(result: zbus::Result<()>) -> zbus::Result<()> {
    match result {
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyExists" =>
        {
            Ok(())
        }
        other => other,
    }
}

/// Paired devices that have a phone's side of a headset or hands-free
/// connection, as (object path, name).
fn paired_phones(connection: &Connection) -> zbus::Result<Vec<(OwnedObjectPath, String)>> {
    let objects = zbus::blocking::fdo::ObjectManagerProxy::builder(connection)
        .destination("org.bluez")?
        .path("/")?
        .build()?
        .get_managed_objects()?;
    let mut phones: Vec<_> = objects
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let (_, device) = interfaces
                .into_iter()
                .find(|(name, _)| name.as_str() == "org.bluez.Device1")?;
            let paired = property(&device, "Paired").unwrap_or(false);
            let uuids: Vec<String> = property(&device, "UUIDs").unwrap_or_default();
            let name = property(&device, "Alias").unwrap_or_else(|| "phone".to_string());
            is_phone(paired, &uuids).then_some((path, name))
        })
        .collect();
    phones.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(phones)
}

fn property<T: TryFrom<OwnedValue>>(
    properties: &HashMap<String, OwnedValue>,
    name: &str,
) -> Option<T> {
    T::try_from(properties.get(name)?.try_clone().ok()?).ok()
}

fn is_phone(paired: bool, uuids: &[String]) -> bool {
    paired
        && uuids.iter().any(|uuid| {
            uuid.eq_ignore_ascii_case(HEADSET_GATEWAY_UUID)
                || uuid.eq_ignore_ascii_case(HANDS_FREE_GATEWAY_UUID)
        })
}

fn describe(
    wireless: bool,
    error: Option<&str>,
    pairing_left: Option<Duration>,
    joining: Option<&str>,
    phones: &[String],
    name: &str,
) -> String {
    if !wireless {
        return String::new();
    }
    if let Some(e) = error {
        return format!("Bluetooth unavailable: {e}");
    }
    if let Some(left) = pairing_left {
        let seconds = left.as_secs();
        return format!(
            "Pairing open ({}:{:02}): on your phone, pair with {name}",
            seconds / 60,
            seconds % 60
        );
    }
    if let Some(phone) = joining {
        return format!("{phone} is joining the hotspot");
    }
    match phones {
        [] => "No phone paired: enable pairing".into(),
        phones => format!("Waiting for {}", phones.join(", ")),
    }
}

async fn device_alias(
    connection: &zbus::Connection,
    device: &OwnedObjectPath,
) -> zbus::Result<String> {
    Device1Proxy::builder(connection)
        .path(device.as_str())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .alias()
        .await
}

/// Answers BlueZ when a phone pairs, showing the user the code to compare.
struct Agent(Arc<Mutex<Shared>>);

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<(), Refusal> {
        let name = device_alias(connection, &device)
            .await
            .unwrap_or_else(|_| "A phone".into());
        let (reply, answer) = async_channel::bounded(1);
        {
            let mut shared = self.0.lock().unwrap();
            if !shared.pairing() {
                info!(phone = %name, "Bluetooth pairing refused: pairing is not open");
                return Err(Refusal::Rejected("pairing is not open on NavBridge".into()));
            }
            info!(phone = %name, "Bluetooth pairing request");
            shared.confirm = Some(Confirm {
                device,
                name,
                code: format!("{passkey:06}"),
                reply,
                until: Instant::now() + CONFIRM_TIME,
            });
        }
        // The user's answer; a dropped request counts as refused.
        match answer.recv().await {
            Ok(true) => Ok(()),
            _ => Err(Refusal::Rejected("refused on NavBridge".into())),
        }
    }

    fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), Refusal> {
        Err(Refusal::Rejected("pairing needs a code".into()))
    }

    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> Result<(), Refusal> {
        Err(Refusal::Rejected("not a NavBridge service".into()))
    }

    fn request_pin_code(&self, _device: OwnedObjectPath) -> Result<String, Refusal> {
        Err(Refusal::Rejected("pairing needs a code".into()))
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) -> Result<(), Refusal> {
        Err(Refusal::Rejected("pairing needs a code".into()))
    }

    fn request_passkey(&self, _device: OwnedObjectPath) -> Result<u32, Refusal> {
        Err(Refusal::Rejected("pairing needs a code".into()))
    }

    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    fn cancel(&self) {
        // Dropping the request ends it as refused.
        self.0.lock().unwrap().confirm = None;
    }
}

/// The Android Auto wireless service: a phone's Android Auto connects here.
struct WirelessProfile(Arc<Mutex<Shared>>, wifi::HotspotWatch);

#[zbus::interface(name = "org.bluez.Profile1")]
impl WirelessProfile {
    fn release(&self) {}

    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: OwnedFd,
        _properties: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) {
        let name = device_alias(connection, &device)
            .await
            .unwrap_or_else(|_| device.to_string());
        info!(phone = %name, "the phone's Android Auto connected over Bluetooth");
        self.0.lock().unwrap().joining = Some((name.clone(), Instant::now() + JOINING_TIME));
        let link = UnixStream::from(std::os::fd::OwnedFd::from(fd));
        let (shared, hotspot) = (self.0.clone(), self.1.clone());
        let spawned = thread::Builder::new()
            .name("aa-wireless-setup".into())
            .spawn(move || setup(link, name, shared, hotspot));
        if let Err(e) = spawned {
            warn!(%e, "could not start the wireless setup");
        }
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

/// Sends a phone to the hotspot: it then opens the session over Wi-Fi.
fn setup(
    mut link: UnixStream,
    phone: String,
    shared: Arc<Mutex<Shared>>,
    hotspot: wifi::HotspotWatch,
) {
    // A phone called while the hotspot was starting waits for it here.
    let started = Instant::now();
    let up = hotspot.wait(HOTSPOT_WAIT);
    let waited_ms = started.elapsed().as_millis() as u64;
    let result = if up {
        wifi::hotspot()
    } else {
        Err(format!(
            "the hotspot was not up within {} s",
            HOTSPOT_WAIT.as_secs()
        ))
    };
    let result = result
        .and_then(|hotspot| wireless::handshake(&mut link, &hotspot).map_err(|e| e.to_string()));
    match result {
        Ok(()) => {
            info!(
                %phone,
                waited_ms,
                "wireless Android Auto: the phone has the hotspot's details"
            );
            shared.lock().unwrap().joining = Some((phone, Instant::now() + JOINING_TIME));
        }
        Err(e) => {
            warn!(%e, %phone, "wireless Android Auto setup failed");
            // Calls resume, unless another phone is setting up.
            let mut shared = shared.lock().unwrap();
            shared.joining.take_if(|(name, _)| *name == phone);
        }
    }
}

/// The headset service. Being called as a headset makes a phone's Android
/// Auto look for the wireless service; the connection itself is not needed,
/// and closing it keeps calls on the phone.
struct HeadsetProfile;

#[zbus::interface(name = "org.bluez.Profile1")]
impl HeadsetProfile {
    fn release(&self) {}

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) {
        debug!(%device, "headset connection with a phone");
        drop(fd);
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_addresses_reach_sockets_least_significant_byte_first() {
        assert_eq!(
            bluetooth_address("2C:CF:67:E1:98:4D"),
            Some([0x4d, 0x98, 0xe1, 0x67, 0xcf, 0x2c])
        );
        assert_eq!(bluetooth_address("2C:CF:67:E1:98"), None);
        assert_eq!(bluetooth_address("2C:CF:67:E1:98:4D:00"), None);
        assert_eq!(bluetooth_address("2C:CF:67:E1:98:XX"), None);
    }

    #[test]
    fn only_paired_phones_are_called() {
        let gateway = vec![HEADSET_GATEWAY_UUID.to_uppercase()];
        let hands_free = vec![HANDS_FREE_GATEWAY_UUID.into()];
        let keyboard = vec!["00001124-0000-1000-8000-00805f9b34fb".to_string()];
        assert!(is_phone(true, &gateway));
        assert!(is_phone(true, &hands_free));
        assert!(!is_phone(false, &gateway));
        assert!(!is_phone(true, &keyboard));
    }

    #[test]
    fn calls_stop_once_a_phone_sets_up() {
        let phones: Vec<(String, String)> = ["Pixel 10", "Pixel 8"]
            .map(|name| (name.to_string(), name.to_string()))
            .to_vec();
        let setup_time = Duration::from_millis(500);
        let joining = |name: &str| Some((name.to_string(), Instant::now() + JOINING_TIME));

        // The first phone answers, and its Android Auto connects a moment
        // later: the second is not called.
        let shared = Arc::new(Mutex::new(Shared::default()));
        let answered = AtomicBool::new(false);
        let mut called = Vec::new();
        call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
            called.push(phone.clone());
            let (shared, phone) = (shared.clone(), phone.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                shared.lock().unwrap().joining = Some((phone, Instant::now() + JOINING_TIME));
            });
            Ok(())
        });
        assert_eq!(called, ["Pixel 10"]);
        assert!(answered.load(Ordering::Relaxed));

        // A phone that answers without setting up holds up the next call by
        // `setup_time`...
        let (shared, answered) = (Mutex::new(Shared::default()), AtomicBool::new(false));
        let (mut called, started) = (Vec::new(), Instant::now());
        call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
            called.push(phone.clone());
            Ok(())
        });
        assert_eq!(called, ["Pixel 10", "Pixel 8"]);
        assert!(started.elapsed() >= setup_time);

        // ...and one that is away does not.
        let (mut called, started) = (Vec::new(), Instant::now());
        call_in_turn(&phones, &shared, &answered, setup_time, |phone: &String| {
            called.push(phone.clone());
            match phone.as_str() {
                "Pixel 10" => Err(zbus::Error::Failure("br-connection-page-timeout".into())),
                _ => Ok(()),
            }
        });
        assert_eq!(called, ["Pixel 10", "Pixel 8"]);
        assert!(started.elapsed() < setup_time);

        // While a phone sets up, nobody is called.
        let shared = Mutex::new(Shared {
            joining: joining("Pixel 8"),
            ..Shared::default()
        });
        call_in_turn(&phones, &shared, &answered, setup_time, |_: &String| {
            panic!("a phone was called during another's setup")
        });
    }

    #[test]
    fn the_status_says_what_bluetooth_does() {
        let name = "NavBridge-984B";
        let phones = ["Pixel 7 Pro".to_string()];
        assert_eq!(describe(false, None, None, None, &phones, name), "");
        assert_eq!(
            describe(true, Some("no adapter"), None, None, &phones, name),
            "Bluetooth unavailable: no adapter"
        );
        assert_eq!(
            describe(
                true,
                None,
                Some(Duration::from_secs(95)),
                None,
                &phones,
                name
            ),
            "Pairing open (1:35): on your phone, pair with NavBridge-984B"
        );
        assert_eq!(
            describe(true, None, None, Some("Pixel 7 Pro"), &phones, name),
            "Pixel 7 Pro is joining the hotspot"
        );
        assert_eq!(
            describe(true, None, None, None, &phones, name),
            "Waiting for Pixel 7 Pro"
        );
        assert_eq!(
            describe(true, None, None, None, &[], name),
            "No phone paired: enable pairing"
        );
    }

    fn waiting_code(service: &BluetoothService, until: Instant) -> async_channel::Receiver<bool> {
        let (reply, answer) = async_channel::bounded(1);
        service.shared.lock().unwrap().confirm = Some(Confirm {
            device: OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").unwrap(),
            name: "Pixel 7 Pro".into(),
            code: "042137".into(),
            reply,
            until,
        });
        answer
    }

    #[test]
    fn the_users_answer_or_its_absence_reaches_the_phone() {
        // Wireless mode stays off, so BlueZ is never contacted.
        let service = BluetoothService::start(wifi::HotspotWatch::default());
        let answer = waiting_code(&service, Instant::now() + CONFIRM_TIME);
        assert_eq!(
            service.snapshot().confirm,
            Some(PairingRequest {
                device: "Pixel 7 Pro".into(),
                code: "042137".into()
            })
        );
        service.confirm(true);
        assert_eq!(answer.try_recv(), Ok(true));
        assert_eq!(service.snapshot().confirm, None);
        // A code left unanswered is refused.
        let answer = waiting_code(&service, Instant::now());
        let deadline = Instant::now() + Duration::from_secs(3);
        while answer.is_empty() {
            assert!(Instant::now() < deadline, "the code was never refused");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(answer.try_recv(), Ok(false));
        assert_eq!(service.snapshot().confirm, None);
    }

    /// Registers NavBridge's services with the real BlueZ, then removes them.
    /// The adapter's name and visibility are left alone.
    #[test]
    #[ignore = "requires BlueZ"]
    fn live_bluez_registration() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let connection = Connection::system().unwrap();
        {
            let server = connection.object_server();
            server.at(AGENT, Agent(shared.clone())).unwrap();
            server
                .at(
                    WIRELESS_PROFILE,
                    WirelessProfile(shared, wifi::HotspotWatch::default()),
                )
                .unwrap();
            server.at(HEADSET_PROFILE, HeadsetProfile).unwrap();
        }
        let adapter = adapter(&connection).unwrap();
        println!(
            "adapter: powered {}, pairable {}, discoverable {}",
            adapter.powered().unwrap(),
            adapter.pairable().unwrap(),
            adapter.discoverable().unwrap()
        );
        register_services(&connection).unwrap();
        // Twice is fine, as after a restart of NavBridge.
        let channel = register_services(&connection).unwrap();
        let device = bluetooth_address(&adapter.address().unwrap()).unwrap();
        assert!(
            listened(device, channel),
            "BlueZ is not on channel {channel}"
        );
        println!("Android Auto wireless service on RFCOMM channel {channel}");
        println!(
            "paired phones: {}",
            paired_phones(&connection).unwrap().len()
        );
        unregister_services(&connection).unwrap();
        assert!(!listened(device, channel), "channel {channel} still taken");
    }
}
