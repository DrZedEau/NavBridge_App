//! The Android Auto service as the core's main loop sees it: commands for the
//! session thread, and the projection's state, snapshot and events.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use super::runner::{runner, Setup};
use super::{Command, EventReceiver, Inbox, State};
use crate::config::{AndroidAutoConfig, AudioConfig};
use crate::services::events;
use crate::services::gnss::GnssUpdate;
use crate::services::video::FrameSize;
use crate::settings::Settings;
use navbridge_common::{
    NavigationKey, PhoneAction, ProjectionEvent, ProjectionPhase, ProjectionSnapshot, TouchKind,
    UsbSnapshot, VehicleSnapshot, VideoCodec,
};
use tracing::{debug, error, info, warn};

/// What the service asks of the car's Bluetooth. The core's main loop gives
/// it, so that this service does not depend on the Bluetooth one, which uses
/// this one's wireless handshake.
#[derive(Clone, Copy)]
pub struct CarBluetooth {
    /// The car's address while something can take calls on it.
    pub hands_free_address: fn() -> Option<String>,
    /// Whether the car has paired with the phone at this address.
    pub paired: fn(&str) -> bool,
}

pub struct ProjectionService {
    pub(super) state: Arc<RwLock<State>>,
    pub(super) settings: Arc<Settings>,
    pub(super) frame_size: FrameSize,
    pub(super) tx: SyncSender<Command>,
    pub(super) wake: SyncSender<()>,
    pub(super) events: EventReceiver,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) worker: Option<JoinHandle<()>>,
}

impl ProjectionService {
    /// The DPI and codec chosen in Settings, kept in `settings`, replace
    /// core.toml's. Decoded video goes to `frame_path`, for the UI.
    pub fn start(
        config: AndroidAutoConfig,
        audio: AudioConfig,
        frame_path: PathBuf,
        settings: Arc<Settings>,
        bluetooth: CarBluetooth,
    ) -> io::Result<Self> {
        let saved = settings.load();
        let state = Arc::new(RwLock::new(State {
            projection: ProjectionSnapshot {
                dpi: saved.dpi.unwrap_or(config.dpi),
                codec: saved.video_codec.unwrap_or(config.codec),
                ..Default::default()
            },
            usb: Default::default(),
            night: false,
            parked: false,
            gnss: false,
            speedometer: false,
            speed: None,
            low_beam: None,
            failure: None,
        }));
        let (tx, commands) = mpsc::sync_channel(128);
        // One pending wakeup covers everything queued before the thread runs.
        let (wake, woken) = mpsc::sync_channel(1);
        let (event_tx, events) = events::channel()?;
        let stop = Arc::new(AtomicBool::new(false));
        let inbox = Inbox {
            commands,
            wake: wake.clone(),
            woken,
        };
        let (s, end) = (state.clone(), stop.clone());
        let frame_size = FrameSize::default();
        let video_size = frame_size.clone();
        let worker = thread::spawn(move || {
            let setup = Setup {
                config,
                audio,
                frame_path,
                frame_size: video_size,
                bluetooth,
            };
            if let Err(e) = runner(setup, s.clone(), inbox, event_tx, end) {
                error!(%e, "Android Auto stopped");
                let mut state = s.write().unwrap();
                state.projection.phase = ProjectionPhase::Error;
                state.projection.status = e.clone();
                state.failure = Some(e);
            }
        });
        Ok(Self {
            state,
            settings,
            frame_size,
            tx,
            wake,
            events,
            stop,
            worker: Some(worker),
        })
    }
    fn command(&self, c: Command) {
        if self.tx.try_send(c).is_err() {
            warn!("projection command queue unavailable");
            return;
        }
        let _ = self.wake.try_send(());
    }
    pub fn hide_session(&self) {
        self.command(Command::Hide)
    }
    pub fn resume_session(&self) {
        self.command(Command::Resume)
    }
    /// For the next phone that connects: the DPI is part of its session setup.
    pub fn set_dpi(&self, dpi: u32) {
        self.state.write().unwrap().projection.dpi = dpi;
        self.settings.update(|saved| saved.dpi = Some(dpi));
    }
    /// For the next phone that connects, as the DPI is.
    pub fn set_codec(&self, codec: VideoCodec) {
        self.state.write().unwrap().projection.codec = codec;
        self.settings
            .update(|saved| saved.video_codec = Some(codec));
    }
    /// The size the UI shows projection frames at: 400x240, or 800x480 for a
    /// UI drawn twice as large.
    pub fn set_frame_size(&self, width: u32, height: u32) {
        if !FrameSize::CHOICES.contains(&(width, height)) {
            warn!(
                width,
                height, "projection frame size not offered; keeping it"
            );
            return;
        }
        if self.frame_size.set(width, height) {
            info!(width, height, "projection frames at the UI's size");
        }
    }
    pub fn set_night_mode(&self, night: bool) {
        self.state.write().unwrap().night = night;
        self.command(Command::Night(night));
    }
    /// Kept for the next phone too, as night mode is. While the I-Bus is
    /// read, the car's handbrake is the parking brake instead.
    pub fn set_parking_brake(&self, engaged: bool) {
        {
            let mut s = self.state.write().unwrap();
            if s.speedometer {
                debug!(
                    engaged,
                    "parking brake ignored: the I-Bus reports the handbrake"
                );
                return;
            }
            s.parked = engaged;
        }
        info!(engaged, "parking brake");
        self.command(Command::ParkingBrake(engaged));
    }
    /// For the GNSS service: whether a receiver is connected, and its fixes.
    pub fn gnss_input(&self) -> impl Fn(GnssUpdate) + Send + 'static {
        let (state, tx, wake) = (self.state.clone(), self.tx.clone(), self.wake.clone());
        move |update| match update {
            GnssUpdate::Receiver(connected) => state.write().unwrap().gnss = connected,
            GnssUpdate::Fix(fix) => {
                if tx.try_send(Command::Location(fix)).is_ok() {
                    let _ = wake.try_send(());
                }
            }
        }
    }
    /// The car's I-Bus, at each snapshot: whether it is read, so a phone
    /// connecting later is offered the speed; the speed in km/h, which a
    /// connected phone gets every time while no fresh fix gives one; the
    /// handbrake, which while the I-Bus is read is the parking brake, off
    /// until the cluster has reported it; and the low beam, whose changes set
    /// night mode: night with the headlights on, day with them off.
    pub fn set_vehicle(&self, vehicle: &VehicleSnapshot) {
        let (read, speed) = (vehicle.interface, vehicle.speed_kmh);
        let (send_speed, brake, night) = {
            let mut s = self.state.write().unwrap();
            let changed = s.speed != speed;
            s.speedometer = read;
            s.speed = speed;
            let engaged = vehicle.handbrake == Some(true);
            let brake = (read && s.parked != engaged).then(|| {
                s.parked = engaged;
                engaged
            });
            let night = match vehicle.low_beam {
                Some(on) if s.low_beam != Some(on) => {
                    s.low_beam = Some(on);
                    s.night = on;
                    Some(on)
                }
                _ => None,
            };
            (
                s.projection.phone_connected && (changed || speed.is_some()),
                brake,
                night,
            )
        };
        if let Some(engaged) = brake {
            info!(engaged, "handbrake");
            self.command(Command::ParkingBrake(engaged));
        }
        if let Some(on) = night {
            info!(low_beam = on, "night mode follows the headlights");
            self.command(Command::Night(on));
        }
        if send_speed {
            self.command(Command::Speed(speed));
        }
    }
    pub fn navigation_input(&self, k: NavigationKey, p: bool) {
        self.command(Command::Key(k, p))
    }
    pub fn touch(&self, kind: TouchKind, x: u32, y: u32) {
        self.command(Command::Touch(kind, x, y))
    }
    pub fn phone_action(&self, a: PhoneAction) {
        self.command(Command::Phone(a))
    }
    /// Asks a phone on the hotspot to end its session; a phone on USB stays.
    pub fn disconnect_wireless(&self) {
        self.command(Command::Disconnect)
    }
    pub fn usb_snapshot(&self) -> UsbSnapshot {
        self.state.read().unwrap().usb.clone()
    }
    pub fn snapshot(&self) -> ProjectionSnapshot {
        self.state.read().unwrap().projection.clone()
    }
    /// Why Android Auto stopped for good, as when its video decoder or its
    /// USB access failed: the core then exits, for systemd to start it again.
    pub fn failure(&self) -> Option<String> {
        let stopped = self.worker.as_ref().is_some_and(JoinHandle::is_finished);
        stopped.then(|| {
            let failure = self.state.read().unwrap().failure.clone();
            failure.unwrap_or_else(|| "the Android Auto worker stopped".into())
        })
    }
    pub fn take_events(&self) -> Vec<ProjectionEvent> {
        self.events.take()
    }
    /// Readable whenever `take_events` has something to return.
    pub fn event_fd(&self) -> BorrowedFd<'_> {
        self.events.fd()
    }
}

impl Drop for ProjectionService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
