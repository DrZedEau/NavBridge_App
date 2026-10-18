//! The car's I-Bus, through the carrier board's ATmega32U4, which passes on
//! each frame it hears as a line on its USB serial port. Read only: nothing is
//! ever sent towards the car.
mod frame;
mod serial;

use crate::config::IbusConfig;
use frame::Reading;
use navbridge_common::{Ignition, VehicleSnapshot};
use std::{
    collections::HashSet,
    io,
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

pub struct IbusService {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl IbusService {
    pub fn start(config: IbusConfig) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = if config.enabled {
            state.lock().unwrap().problem = Some("Opening the interface".into());
            let (shared, stopped) = (state.clone(), stop.clone());
            let device = PathBuf::from(config.device);
            let worker = thread::Builder::new()
                .name("ibus".into())
                .spawn(move || run(&device, &shared, &stopped))
                .expect("start the I-Bus thread");
            Some(worker)
        } else {
            state.lock().unwrap().problem = Some("Off in core.toml".into());
            None
        };
        Self {
            state,
            stop,
            worker,
        }
    }

    pub fn snapshot(&self) -> VehicleSnapshot {
        self.state.lock().unwrap().snapshot(Instant::now())
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
fn run(device: &Path, state: &Mutex<State>, stop: &AtomicBool) {
    let mut reported: Option<String> = None;
    let mut undecoded = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        let problem = match serial::Port::open(device) {
            Ok(mut port) => {
                info!(device = %device.display(), "reading the I-Bus");
                state.lock().unwrap().opened();
                let Some(e) = listen(&mut port, state, stop, &mut undecoded) else {
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
            received(&String::from_utf8_lossy(&pending[..end]), state, undecoded);
            pending.drain(..=end);
        }
        if pending.len() > MAX_LINE {
            state.lock().unwrap().bad_lines += 1;
            pending.clear();
        }
    }
    None
}

fn received(line: &str, state: &Mutex<State>, undecoded: &mut HashSet<(u8, u8, u8)>) {
    match frame::parse_line(line) {
        Ok(Some(frame)) => {
            let reading = frame::decode(&frame);
            let kind = (frame.source, frame.destination, frame.message[0]);
            // Each kind once, to find out what else the car sends.
            if reading.is_none() && undecoded.insert(kind) {
                debug!(
                    source = %format!("{:02X}", kind.0),
                    destination = %format!("{:02X}", kind.1),
                    message = %format!("{:02X}", kind.2),
                    "I-Bus message not decoded"
                );
            }
            if state.lock().unwrap().frame(reading, Instant::now()) {
                debug!(?reading, "I-Bus reading");
            }
        }
        Ok(None) => {}
        Err(e) => {
            debug!(%e, line, "damaged I-Bus line");
            state.lock().unwrap().bad_lines += 1;
        }
    }
}

#[derive(Default)]
struct State {
    /// Why the interface is not open; `None` while it is.
    problem: Option<String>,
    frames: u64,
    bad_lines: u64,
    heard_at: Option<Instant>,
    ignition: Option<Ignition>,
    /// km/h, rpm, and when the cluster sent them.
    speed: Option<(u16, u16, Instant)>,
    /// Outside and coolant, in °C.
    temperatures: Option<(i8, i8)>,
    odometer: Option<u32>,
}

impl State {
    fn opened(&mut self) {
        self.problem = None;
    }

    /// Nothing the interface said before holds any more; the counts stay.
    fn closed(&mut self, problem: String) {
        *self = Self {
            problem: Some(problem),
            frames: self.frames,
            bad_lines: self.bad_lines,
            ..Self::default()
        };
    }

    /// A valid frame, and what it says if NavBridge knows the message.
    /// Returns whether a reading changed.
    fn frame(&mut self, reading: Option<Reading>, now: Instant) -> bool {
        self.frames += 1;
        self.heard_at = Some(now);
        match reading {
            Some(Reading::Ignition(ignition)) => self.ignition.replace(ignition) != Some(ignition),
            Some(Reading::Speed { kmh, rpm }) => self
                .speed
                .replace((kmh, rpm, now))
                .is_none_or(|(old_kmh, old_rpm, _)| (old_kmh, old_rpm) != (kmh, rpm)),
            Some(Reading::Temperatures { outside, coolant }) => {
                self.temperatures.replace((outside, coolant)) != Some((outside, coolant))
            }
            Some(Reading::Odometer { km }) => self.odometer.replace(km) != Some(km),
            None => false,
        }
    }

    fn snapshot(&self, now: Instant) -> VehicleSnapshot {
        let mut vehicle = VehicleSnapshot {
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
        vehicle.odometer_km = self.odometer;
        vehicle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        io::Write,
        os::fd::FromRawFd,
    };

    #[test]
    fn readings_follow_the_bus_and_expire() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut state = State::default();
        assert_eq!(state.snapshot(start).status, "No I-Bus traffic");
        state.frame(Some(Reading::Speed { kmh: 50, rpm: 1500 }), start);
        let temperatures = Reading::Temperatures {
            outside: 12,
            coolant: 85,
        };
        assert!(state.frame(Some(temperatures), start));
        assert!(!state.frame(Some(temperatures), start), "unchanged");
        state.frame(Some(Reading::Odometer { km: 123_456 }), start);
        let v = state.snapshot(at(1));
        assert_eq!(v.status, "Receiving");
        assert_eq!((v.speed_kmh, v.rpm), (Some(50), Some(1500)));
        assert_eq!((v.outside_c, v.coolant_c), (Some(12), Some(85)));
        assert_eq!(
            (v.odometer_km, v.ignition, v.frames),
            (Some(123_456), None, 4)
        );

        // Other frames keep the bus alive: a speed not sent again goes stale,
        // while temperatures and distance, which change slowly, stay.
        state.frame(None, at(9));
        state.frame(Some(Reading::Ignition(Ignition::On)), at(11));
        let v = state.snapshot(at(11));
        assert_eq!((v.speed_kmh, v.rpm), (None, None));
        assert_eq!((v.coolant_c, v.odometer_km), (Some(85), Some(123_456)));
        assert_eq!(v.ignition, Some(Ignition::On));
        // A fresh speed shows while the ignition is on, not once it is off.
        state.frame(Some(Reading::Speed { kmh: 0, rpm: 800 }), at(12));
        assert_eq!(state.snapshot(at(12)).rpm, Some(800));
        state.frame(Some(Reading::Ignition(Ignition::Off)), at(13));
        assert_eq!(state.snapshot(at(13)).rpm, None);

        // A quiet bus: the car is asleep, and nothing is current.
        let v = state.snapshot(at(24));
        assert_eq!(v.status, "No I-Bus traffic");
        assert_eq!((v.ignition, v.coolant_c, v.frames), (None, None, 8));
        // Losing the interface forgets the car at once, but not the counts.
        state.bad_lines += 1;
        state.closed("Interface not connected".into());
        let v = state.snapshot(at(13));
        assert_eq!(v.status, "Interface not connected");
        assert_eq!((v.coolant_c, v.frames, v.bad_lines), (None, 8, 1));
    }

    /// A pseudo-terminal, standing in for the board: its controlling side,
    /// and the device the service opens.
    fn pseudo_terminal() -> (File, PathBuf) {
        let (mut board, mut port) = (0, 0);
        // SAFETY: two out-pointers for the new descriptors; no name, settings
        // or window size.
        let opened = unsafe {
            libc::openpty(
                &mut board,
                &mut port,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(opened, 0, "{}", io::Error::last_os_error());
        let device = fs::read_link(format!("/proc/self/fd/{port}")).unwrap();
        // SAFETY: both descriptors were just opened, and are owned here.
        let (board, port) = unsafe { (File::from_raw_fd(board), File::from_raw_fd(port)) };
        // The service opens the device by its name, as it opens the board's.
        drop(port);
        (board, device)
    }

    fn wait_for(ibus: &IbusService, done: impl Fn(&VehicleSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&ibus.snapshot()) {
            assert!(Instant::now() < deadline, "stuck at {:?}", ibus.snapshot());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_interface_is_read_line_by_line_and_its_loss_noticed() {
        let (mut board, device) = pseudo_terminal();
        let ibus = IbusService::start(IbusConfig {
            enabled: true,
            device: device.display().to_string(),
        });
        // Open, and set to pass bytes through as they come.
        wait_for(&ibus, |v| v.status == "No I-Bus traffic");
        // The board's own message, a damaged frame, and frames, one of them
        // split across writes.
        board
            .write_all(b"IBUS ready\r\nIBUS1 RX 1 8004BF110328\r\nIBUS1 RX 2 8004BF110329\r\nIBUS1 RX 3 8005BF18")
            .unwrap();
        board
            .write_all(b"3C1E00\r\nIBUS1 RX 4 8005BF19F65A8F\r\n")
            .unwrap();
        wait_for(&ibus, |v| v.coolant_c.is_some());
        let v = ibus.snapshot();
        assert_eq!(
            (v.status.as_str(), v.frames, v.bad_lines),
            ("Receiving", 3, 1)
        );
        assert_eq!(v.ignition, Some(Ignition::On));
        assert_eq!((v.speed_kmh, v.rpm), (Some(120), Some(3000)));
        assert_eq!((v.outside_c, v.coolant_c), (Some(-10), Some(90)));
        // Unplugged: what the car said no longer holds.
        drop(board);
        wait_for(&ibus, |v| v.status != "Receiving" && v.ignition.is_none());
        assert_eq!(ibus.snapshot().frames, 3);
    }

    #[test]
    fn a_missing_or_disabled_interface_says_so() {
        let missing = IbusService::start(IbusConfig {
            enabled: true,
            device: "/dev/serial/by-id/no-such-board".into(),
        });
        wait_for(&missing, |v| v.status == "Interface not connected");
        let off = IbusService::start(IbusConfig {
            enabled: false,
            ..IbusConfig::default()
        });
        assert_eq!(off.snapshot().status, "Off in core.toml");
    }

    /// Reads the real interface for a few seconds and prints what it heard.
    #[test]
    #[ignore = "requires the I-Bus interface"]
    fn live_ibus_interface() {
        let ibus = IbusService::start(IbusConfig::default());
        thread::sleep(Duration::from_secs(6));
        let vehicle = ibus.snapshot();
        println!("{vehicle:#?}");
        assert!(vehicle.frames > 0, "no I-Bus frames: {}", vehicle.status);
    }
}
