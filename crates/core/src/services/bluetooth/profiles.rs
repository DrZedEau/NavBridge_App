//! The services phones connect to: the Android Auto wireless service, whose
//! handshake sends a phone to the hotspot, and the headset service.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::bluez::device_alias;
use super::Shared;
use crate::services::android_auto::wireless;
use crate::services::wifi;
use tracing::{debug, info, warn};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};

/// Once wireless mode is on, phones are called this long before the hotspot
/// is up, as the call and the hotspot both take seconds; a phone's setup waits
/// as long for the hotspot before sending it there.
pub(super) const HOTSPOT_WAIT: Duration = Duration::from_secs(15);

/// How long a phone setting up wireless Android Auto is shown joining the
/// hotspot. No phone is called meanwhile: a call could restart its setup.
pub(super) const JOINING_TIME: Duration = Duration::from_secs(30);

/// The Android Auto wireless service: a phone's Android Auto connects here.
pub(super) struct WirelessProfile(pub(super) Arc<Mutex<Shared>>, pub(super) wifi::HotspotWatch);

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
        {
            let mut shared = self.0.lock().unwrap();
            // Closing the link turns it away.
            if shared.paused && shared.turned_away.as_ref() == Some(&device) {
                info!(phone = %name, "the phone's Android Auto turned away: Settings disconnected it");
                return;
            }
            info!(phone = %name, "the phone's Android Auto connected over Bluetooth");
            shared.joining = Some((name.clone(), Instant::now() + JOINING_TIME));
        }
        let link = UnixStream::from(std::os::fd::OwnedFd::from(fd));
        let (shared, hotspot) = (self.0.clone(), self.1.clone());
        let spawned = thread::Builder::new()
            .name("aa-wireless-setup".into())
            .spawn(move || setup(link, device, name, shared, hotspot));
        if let Err(e) = spawned {
            warn!(%e, "could not start the wireless setup");
        }
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

/// Sends a phone to the hotspot: it then opens the session over Wi-Fi.
pub(super) fn setup(
    mut link: UnixStream,
    device: OwnedObjectPath,
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
            let mut shared = shared.lock().unwrap();
            shared.joining = Some((phone, Instant::now() + JOINING_TIME));
            shared.last_setup = Some(device);
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
pub(super) struct HeadsetProfile;

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
