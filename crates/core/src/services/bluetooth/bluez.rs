//! BlueZ over D-Bus: the adapter, devices, and the agent and profile managers,
//! the services NavBridge registers with them, and the phones BlueZ knows.

use std::collections::HashMap;
use std::time::Duration;

use super::rfcomm::{free_channel, listened};
use super::Phone;
use crate::services::android_auto::wireless;
use zbus::blocking::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

pub(super) const AGENT: &str = "/org/navbridge/agent";

pub(super) const WIRELESS_PROFILE: &str = "/org/navbridge/wireless";

pub(super) const HEADSET_PROFILE: &str = "/org/navbridge/headset";

/// A headset: the role NavBridge takes to call a phone.
pub(super) const HEADSET_UUID: &str = "00001108-0000-1000-8000-00805f9b34fb";

/// The phone's side of a headset or hands-free connection.
pub(super) const HEADSET_GATEWAY_UUID: &str = "00001112-0000-1000-8000-00805f9b34fb";

pub(super) const HANDS_FREE_GATEWAY_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";

/// bluez-alsa's name on the system bus: it takes calls as a hands-free device.
pub(super) const HANDS_FREE_SERVICE: &str = "org.bluealsa";

#[zbus::proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0",
    gen_async = false,
    blocking_name = "Adapter1ProxyBlocking"
)]
pub(super) trait Adapter1 {
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
pub(super) trait Device1 {
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
pub(super) trait AgentManager1 {
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
pub(super) trait ProfileManager1 {
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
pub(super) enum Refusal {
    Rejected(String),
}

/// The adapter's address while bluez-alsa runs to take calls on it, for
/// Android Auto to send the phone's calls there; none without BlueZ, an
/// adapter or bluez-alsa.
pub fn hands_free_address() -> Option<String> {
    let connection = Connection::system().ok()?;
    let bus = zbus::blocking::fdo::DBusProxy::new(&connection).ok()?;
    let name = zbus::names::BusName::try_from(HANDS_FREE_SERVICE).ok()?;
    if !bus.name_has_owner(name).ok()? {
        return None;
    }
    adapter(&connection).ok()?.address().ok()
}

/// Whether BlueZ has paired with the phone at `address`, as "64:9D:38:E1:B6:24".
pub fn paired(address: &str) -> bool {
    if bluetooth_address(address).is_none() {
        return false;
    }
    let path = format!(
        "/org/bluez/hci0/dev_{}",
        address.to_uppercase().replace(':', "_")
    );
    let device = Connection::system().and_then(|connection| {
        Device1ProxyBlocking::builder(&connection)
            .path(path)?
            .cache_properties(CacheProperties::No)
            .build()
    });
    device.and_then(|device| device.paired()).unwrap_or(false)
}

pub(super) fn adapter(connection: &Connection) -> zbus::Result<Adapter1ProxyBlocking<'static>> {
    Adapter1ProxyBlocking::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
}

/// Registers the agent and both profiles with BlueZ, replacing an earlier
/// registration. Returns the RFCOMM channel of the Android Auto wireless
/// service: one the kernel finds free, as BlueZ's own services hold some. Its
/// SIM access service holds 8, the channel wireless dongles use.
pub(super) fn register_services(connection: &Connection) -> zbus::Result<u8> {
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
pub(super) fn bluetooth_address(text: &str) -> Option<[u8; 6]> {
    let mut bytes = [0u8; 6];
    let mut parts = text.split(':');
    for byte in bytes.iter_mut().rev() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(bytes)
}

pub(super) fn unregister_services(connection: &Connection) -> zbus::Result<()> {
    let agents = AgentManager1ProxyBlocking::new(connection)?;
    let agent = agents.unregister_agent(&ObjectPath::try_from(AGENT)?);
    let profiles = ProfileManager1ProxyBlocking::new(connection)?;
    let wireless = profiles.unregister_profile(&ObjectPath::try_from(WIRELESS_PROFILE)?);
    let headset = profiles.unregister_profile(&ObjectPath::try_from(HEADSET_PROFILE)?);
    agent.and(wireless).and(headset)
}

/// Registering again what is still registered is fine.
pub(super) fn tolerate_existing(result: zbus::Result<()>) -> zbus::Result<()> {
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
/// connection, by name.
pub(super) fn paired_phones(connection: &Connection) -> zbus::Result<Vec<Phone>> {
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
            let address = property(&device, "Address").unwrap_or_default();
            is_phone(paired, &uuids).then_some(Phone {
                path,
                address,
                name,
            })
        })
        .collect();
    phones.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(phones)
}

pub(super) fn property<T: TryFrom<OwnedValue>>(
    properties: &HashMap<String, OwnedValue>,
    name: &str,
) -> Option<T> {
    T::try_from(properties.get(name)?.try_clone().ok()?).ok()
}

pub(super) fn is_phone(paired: bool, uuids: &[String]) -> bool {
    paired
        && uuids.iter().any(|uuid| {
            uuid.eq_ignore_ascii_case(HEADSET_GATEWAY_UUID)
                || uuid.eq_ignore_ascii_case(HANDS_FREE_GATEWAY_UUID)
        })
}

pub(super) fn describe(
    wireless: bool,
    error: Option<&str>,
    pairing_left: Option<Duration>,
    joining: Option<&str>,
    paused: bool,
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
    if paused {
        return "Disconnected: Connect phone in Settings calls the phones again".into();
    }
    match phones {
        [] => "No phone paired: enable pairing".into(),
        phones => format!("Waiting for {}", phones.join(", ")),
    }
}

pub(super) async fn device_alias(
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

/// Lets the phone connect its services without asking this agent.
pub(super) async fn trust(
    connection: &zbus::Connection,
    device: &OwnedObjectPath,
) -> zbus::Result<()> {
    Device1Proxy::builder(connection)
        .path(device.as_str())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .set_trusted(true)
        .await
}
