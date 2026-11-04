//! Android Auto service: over USB accessory mode, or wirelessly over the
//! hotspot once the Bluetooth handshake has sent the phone there. For
//! development, the session can also run over TCP to the phone's head unit
//! server.
mod framing;
mod identity;
mod session;
mod tcp;
mod tls;
mod transport;
mod usb;
pub mod wireless;

mod listen;
mod runner;
mod service;
#[cfg(test)]
mod tests;

pub use service::{CarBluetooth, ProjectionService};

use std::sync::mpsc::{Receiver, SyncSender};

use crate::services::events;
use crate::services::gnss::Fix;
use navbridge_common::{
    NavigationKey, PhoneAction, ProjectionEvent, ProjectionSnapshot, TouchKind, UsbSnapshot,
};

#[derive(Clone)]
struct State {
    projection: ProjectionSnapshot,
    usb: UsbSnapshot,
    night: bool,
    /// The parking brake is on: the phone lifts its driving restrictions,
    /// unless the I-Bus reports the car moving.
    parked: bool,
    /// A GNSS receiver is connected: a phone connecting now is offered the
    /// car's location, and its speed from the fixes.
    gnss: bool,
    /// The I-Bus is read: a phone connecting now is offered the car's speed,
    /// which stands in while no fix gives one, and the cluster's handbrake is
    /// the parking brake.
    speedometer: bool,
    /// The car's speed in km/h, while the I-Bus reports it.
    speed: Option<u16>,
    /// The low beam as the I-Bus last reported it: only its changes set night
    /// mode, so the keys can still switch it in between.
    low_beam: Option<bool>,
    /// Why the worker stopped, if it could not go on.
    failure: Option<String>,
}

enum Command {
    Hide,
    Resume,
    Key(NavigationKey, bool),
    Touch(TouchKind, u32, u32),
    Phone(PhoneAction),
    Night(bool),
    ParkingBrake(bool),
    Location(Fix),
    Speed(Option<u16>),
    /// End a wireless session, as Settings' Disconnect phone asks.
    Disconnect,
}

/// Work for the session thread. Commands, USB transfers and microphone blocks
/// each signal `wake` once queued, so the thread sleeps on `woken` instead of
/// polling, and a key press reaches the phone without waiting for a timer.
struct Inbox {
    commands: Receiver<Command>,
    wake: SyncSender<()>,
    woken: Receiver<()>,
}

/// Projection events for the main loop.
type EventSender = events::EventSender<ProjectionEvent>;

type EventReceiver = events::EventReceiver<ProjectionEvent>;
