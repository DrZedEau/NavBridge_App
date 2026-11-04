//! The pairing agent BlueZ asks when a phone pairs: the user compares the
//! phone's code on screen and answers.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::bluez::{device_alias, trust, Refusal};
use super::{Confirm, Shared};
use tracing::{info, warn};
use zbus::zvariant::OwnedObjectPath;

/// A code not confirmed by then is refused; phones give up about then too.
pub(super) const CONFIRM_TIME: Duration = Duration::from_secs(30);

/// Answers BlueZ when a phone pairs, showing the user the code to compare.
pub(super) struct Agent(pub(super) Arc<Mutex<Shared>>);

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
                device: device.clone(),
                name: name.clone(),
                code: format!("{passkey:06}"),
                reply,
                until: Instant::now() + CONFIRM_TIME,
            });
        }
        // The user's answer; a dropped request counts as refused.
        match answer.recv().await {
            Ok(true) => {
                // Trusted before the pairing ends: a phone connects its
                // services the moment both sides have confirmed, at once
                // when the phone confirmed first, and BlueZ asks this agent
                // to authorize those of an untrusted phone, which it refuses.
                match trust(connection, &device).await {
                    Ok(()) => {
                        info!(phone = %name, "Bluetooth phone trusted as its code is confirmed")
                    }
                    Err(e) => {
                        warn!(%e, phone = %name, "Bluetooth phone not trusted before pairing ends")
                    }
                }
                Ok(())
            }
            _ => Err(Refusal::Rejected("refused on NavBridge".into())),
        }
    }

    fn request_authorization(&self, _device: OwnedObjectPath) -> Result<(), Refusal> {
        Err(Refusal::Rejected("pairing needs a code".into()))
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<(), Refusal> {
        let name = device_alias(connection, &device)
            .await
            .unwrap_or_else(|_| "A phone".into());
        info!(phone = %name, %uuid, "Bluetooth service refused: the phone is not trusted");
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
        if let Some(confirm) = self.0.lock().unwrap().confirm.take() {
            info!(phone = %confirm.name, "Bluetooth pairing cancelled before NavBridge answered");
        }
    }
}
