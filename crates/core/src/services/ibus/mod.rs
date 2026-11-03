//! The car's I-Bus, through the carrier board's ATmega32U4, which passes on
//! each frame it hears as a line on its USB serial port. Read only: nothing is
//! ever sent towards the car.
//!
//! The car's buttons become UI inputs, as from keys: the service asks the board
//! to stop typing keys for them ("KEYS OFF", which holds until the port
//! closes). The board passes frames on only while NavBridge's screen shows,
//! except the cluster's ignition-off, after which the Pi shuts down.
mod frame;
mod serial;
#[cfg(test)]
mod tests;

use crate::config::IbusConfig;
use crate::services::events::{self, EventReceiver, EventSender};
pub use frame::Button;
use frame::{Phase, Press, Reading};
use navbridge_common::{Ignition, InputAction, VehicleSnapshot};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Without a frame for this long, the bus is asleep or disconnected, and
/// nothing heard before is current.
const SILENCE: Duration = Duration::from_secs(10);
/// The cluster sends road and engine speed every 2 seconds with the ignition
/// on; older ones no longer describe the car.
const SPEED_EXPIRY: Duration = Duration::from_secs(10);
/// How often an interface that is missing or went away is opened again.
const RETRY: Duration = Duration::from_secs(2);
/// The longest the thread waits for bytes before checking whether to stop.
const READ_WAIT: Duration = Duration::from_millis(500);
/// A longer line is not one of the board's.
const MAX_LINE: usize = 512;

/// A car button's action for the UI, as a key's: pressed, then released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonInput {
    pub action: InputAction,
    pub pressed: bool,
}

/// What the I-Bus thread passes to the main loop.
enum Event {
    Input(ButtonInput),
    /// A reading changed.
    Changed,
}

/// What the main loop gets from the bus since it last looked.
#[derive(Debug, Default, PartialEq)]
pub struct Heard {
    /// The car's buttons, for the UI.
    pub inputs: Vec<ButtonInput>,
    /// A reading changed: the UI should see it now, not at the next snapshot.
    pub changed: bool,
}

/// The car's buttons, and the channel to the main loop.
struct Buttons {
    bindings: BTreeMap<Button, InputAction>,
    events: EventSender<Event>,
}

pub struct IbusService {
    state: Arc<Mutex<State>>,
    events: EventReceiver<Event>,
    /// How long after the ignition goes off the Pi shuts down, if it does.
    shutdown_delay: Option<Duration>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl IbusService {
    pub fn start(config: IbusConfig) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, events) = events::channel().expect("create the I-Bus event channel");
        let shutdown_delay = config
            .shutdown
            .then(|| Duration::from_secs(config.shutdown_delay_s));
        state.lock().unwrap().shutdown_delay = shutdown_delay;
        let worker = if config.enabled {
            state.lock().unwrap().problem = Some("Opening the interface".into());
            let (shared, stopped) = (state.clone(), stop.clone());
            let device = PathBuf::from(config.device);
            let buttons = Buttons {
                bindings: config.buttons,
                events: sender,
            };
            let worker = thread::Builder::new()
                .name("ibus".into())
                .spawn(move || run(&device, &shared, &stopped, &buttons))
                .expect("start the I-Bus thread");
            Some(worker)
        } else {
            state.lock().unwrap().problem = Some("Off in core.toml".into());
            None
        };
        Self {
            state,
            events,
            shutdown_delay,
            stop,
            worker,
        }
    }

    pub fn snapshot(&self) -> VehicleSnapshot {
        self.state.lock().unwrap().snapshot(Instant::now())
    }

    /// The ignition has been off for the shutdown delay, and stayed off: time
    /// for the Pi to shut down.
    pub fn shutdown_due(&self) -> bool {
        let Some(delay) = self.shutdown_delay else {
            return false;
        };
        self.state
            .lock()
            .unwrap()
            .ignition_off_at
            .is_some_and(|at| at.elapsed() >= delay)
    }

    /// The car's buttons, and whether a reading changed, since the last call.
    pub fn take(&self) -> Heard {
        let mut heard = Heard::default();
        for event in self.events.take() {
            match event {
                Event::Input(input) => heard.inputs.push(input),
                Event::Changed => heard.changed = true,
            }
        }
        heard
    }

    /// Readable whenever `take` has something to return.
    pub fn event_fd(&self) -> BorrowedFd<'_> {
        self.events.fd()
    }
}

impl Drop for IbusService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Keeps the interface open, opening it again whenever it is missing or goes
/// away, until the core stops.
fn run(device: &Path, state: &Mutex<State>, stop: &AtomicBool, buttons: &Buttons) {
    let mut reported: Option<String> = None;
    let mut undecoded = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        let problem = match serial::Port::open(device) {
            Ok(mut port) => {
                info!(device = %device.display(), "reading the I-Bus");
                state.lock().unwrap().opened();
                if let Err(e) = port.send_line("KEYS OFF") {
                    warn!(%e, "could not ask the board to stop typing keys");
                }
                let Some(e) = listen(&mut port, state, stop, &mut undecoded, buttons) else {
                    return;
                };
                warn!(%e, "I-Bus interface lost");
                "Interface lost".to_string()
            }
            Err(e) => {
                let problem = unavailable(&e);
                // Once, not at every retry.
                if reported.as_deref() != Some(problem.as_str()) {
                    info!(%e, device = %device.display(), "I-Bus interface unavailable");
                }
                problem
            }
        };
        reported = Some(problem.clone());
        state.lock().unwrap().closed(problem);
        let retry_at = Instant::now() + RETRY;
        while Instant::now() < retry_at && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn unavailable(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "Interface not connected".into(),
        io::ErrorKind::PermissionDenied => "No permission to read the interface".into(),
        _ => format!("Interface unavailable: {e}"),
    }
}

/// Reads lines until the interface goes away, which returns why, or the core
/// stops.
fn listen(
    port: &mut serial::Port,
    state: &Mutex<State>,
    stop: &AtomicBool,
    undecoded: &mut HashSet<(u8, u8, u8)>,
    buttons: &Buttons,
) -> Option<io::Error> {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        let n = match port.read(&mut chunk, READ_WAIT) {
            Ok(n) => n,
            Err(e) => return Some(e),
        };
        pending.extend_from_slice(&chunk[..n]);
        while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
            received(
                &String::from_utf8_lossy(&pending[..end]),
                state,
                undecoded,
                buttons,
            );
            pending.drain(..=end);
        }
        if pending.len() > MAX_LINE {
            state.lock().unwrap().bad_lines += 1;
            pending.clear();
        }
    }
    None
}

fn received(
    line: &str,
    state: &Mutex<State>,
    undecoded: &mut HashSet<(u8, u8, u8)>,
    buttons: &Buttons,
) {
    match frame::parse_line(line) {
        Ok(Some(frame)) => {
            let reading = frame::decode(&frame);
            let press = frame::button(&frame);
            if let Some(press) = press {
                debug!(?press, "I-Bus button");
                for input in inputs(press, &buttons.bindings) {
                    let _ = buttons.events.send(Event::Input(input));
                }
            }
            let kind = (frame.source, frame.destination, frame.message[0]);
            // Each kind once, to find out what else the car sends.
            if reading.is_none() && press.is_none() && undecoded.insert(kind) {
                debug!(
                    source = %format!("{:02X}", kind.0),
                    destination = %format!("{:02X}", kind.1),
                    message = %format!("{:02X}", kind.2),
                    "I-Bus message not decoded"
                );
            }
            if state.lock().unwrap().frame(reading, Instant::now()) {
                debug!(?reading, "I-Bus reading");
                let _ = buttons.events.send(Event::Changed);
            }
        }
        Ok(None) => {}
        Err(e) => {
            debug!(%e, line, "damaged I-Bus line");
            state.lock().unwrap().bad_lines += 1;
        }
    }
}

/// The UI's inputs for a button: a tap, pressed and released at once, for a
/// press and for each detent of the knob. Buttons act on their press alone:
/// holding one, and letting it go, do nothing more. A button with no action
/// gives none.
fn inputs(press: Press, bindings: &BTreeMap<Button, InputAction>) -> Vec<ButtonInput> {
    let Some(&action) = bindings.get(&press.button) else {
        return Vec::new();
    };
    if press.phase != Phase::Press {
        return Vec::new();
    }
    (0..press.steps)
        .flat_map(|_| {
            [
                ButtonInput {
                    action,
                    pressed: true,
                },
                ButtonInput {
                    action,
                    pressed: false,
                },
            ]
        })
        .collect()
}

#[derive(Default)]
struct State {
    /// Why the interface is not open; `None` while it is.
    problem: Option<String>,
    /// When the cluster said the ignition went off, until it says otherwise;
    /// it outlasts the bus falling asleep and the interface going away.
    ignition_off_at: Option<Instant>,
    shutdown_delay: Option<Duration>,
    frames: u64,
    bad_lines: u64,
    heard_at: Option<Instant>,
    ignition: Option<Ignition>,
    /// km/h, rpm, and when the cluster sent them.
    speed: Option<(u16, u16, Instant)>,
    /// Outside and coolant, in °C.
    temperatures: Option<(i8, i8)>,
    /// The on-board computer's range, in km, and consumption, in tenths of a
    /// litre per 100 km: they hold until it sends them again.
    range_km: Option<u16>,
    consumption_dl: Option<u16>,
    /// The cluster says so when they change: they hold until then.
    handbrake: Option<bool>,
    reverse: Option<bool>,
    /// The light module says so when it changes: it holds until then.
    low_beam: Option<bool>,
}

impl State {
    fn opened(&mut self) {
        self.problem = None;
    }

    /// Off starts the shutdown delay; any other position ends it.
    fn ignition_changed(&mut self, ignition: Ignition, now: Instant) {
        let off = ignition == Ignition::Off;
        if off && self.ignition_off_at.is_none() {
            self.ignition_off_at = Some(now);
            match self.shutdown_delay {
                Some(delay) if delay.is_zero() => info!("ignition off"),
                Some(delay) => info!(
                    seconds = delay.as_secs(),
                    "ignition off: the Pi shuts down unless it comes back on"
                ),
                None => {}
            }
        } else if !off && self.ignition_off_at.take().is_some() {
            info!(?ignition, "ignition back: no shutdown");
        }
    }

    /// Nothing the interface said before holds any more; the counts stay.
    fn closed(&mut self, problem: String) {
        *self = Self {
            problem: Some(problem),
            frames: self.frames,
            bad_lines: self.bad_lines,
            ignition_off_at: self.ignition_off_at,
            shutdown_delay: self.shutdown_delay,
            ..Self::default()
        };
    }

    /// A valid frame, and what it says if NavBridge knows the message.
    /// Returns whether a reading changed.
    fn frame(&mut self, reading: Option<Reading>, now: Instant) -> bool {
        self.frames += 1;
        self.heard_at = Some(now);
        match reading {
            Some(Reading::Ignition(ignition)) => {
                self.ignition_changed(ignition, now);
                self.ignition.replace(ignition) != Some(ignition)
            }
            Some(Reading::Speed { kmh, rpm }) => self
                .speed
                .replace((kmh, rpm, now))
                .is_none_or(|(old_kmh, old_rpm, _)| (old_kmh, old_rpm) != (kmh, rpm)),
            Some(Reading::Temperatures { outside, coolant }) => {
                self.temperatures.replace((outside, coolant)) != Some((outside, coolant))
            }
            Some(Reading::Range(km)) => std::mem::replace(&mut self.range_km, km) != km,
            Some(Reading::Consumption(dl)) => std::mem::replace(&mut self.consumption_dl, dl) != dl,
            Some(Reading::Sensors { handbrake, reverse }) => {
                let pulled = self.handbrake.replace(handbrake) != Some(handbrake);
                let shifted = self.reverse.replace(reverse) != Some(reverse);
                if shifted {
                    info!(reverse, "reverse gear");
                }
                pulled || shifted
            }
            Some(Reading::LowBeam(on)) => self.low_beam.replace(on) != Some(on),
            None => false,
        }
    }

    fn snapshot(&self, now: Instant) -> VehicleSnapshot {
        let mut vehicle = VehicleSnapshot {
            interface: self.problem.is_none(),
            frames: self.frames,
            bad_lines: self.bad_lines,
            ..Default::default()
        };
        let recent = |at: Instant, limit: Duration| now.saturating_duration_since(at) < limit;
        if let Some(problem) = &self.problem {
            vehicle.status = problem.clone();
            return vehicle;
        }
        if !self.heard_at.is_some_and(|at| recent(at, SILENCE)) {
            vehicle.status = "No I-Bus traffic".into();
            return vehicle;
        }
        vehicle.status = "Receiving".into();
        vehicle.ignition = self.ignition;
        // With the ignition off, the engine is not running, whatever was last heard.
        let running = !matches!(self.ignition, Some(Ignition::Off | Ignition::Accessory));
        if let Some((kmh, rpm, at)) = self.speed {
            if running && recent(at, SPEED_EXPIRY) {
                vehicle.speed_kmh = Some(kmh);
                vehicle.rpm = Some(rpm);
            }
        }
        if let Some((outside, coolant)) = self.temperatures {
            vehicle.outside_c = Some(outside);
            vehicle.coolant_c = Some(coolant);
        }
        vehicle.range_km = self.range_km;
        vehicle.consumption_dl = self.consumption_dl;
        vehicle.handbrake = self.handbrake;
        vehicle.reverse = self.reverse;
        vehicle.low_beam = self.low_beam;
        vehicle
    }
}
