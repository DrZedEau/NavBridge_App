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
use crate::config::{AndroidAutoConfig, AudioConfig, DecoderChoice};
use crate::services::{
    audio::AudioPipeline,
    events,
    gnss::{Fix, GnssUpdate},
    media::{FrameSize, PushResult, VideoPipeline},
    wifi::HOTSPOT_IP,
};
use crate::settings::Settings;
use navbridge_common::{
    NavigationKey, PhoneAction, ProjectionEvent, ProjectionPhase, ProjectionSnapshot, TouchKind,
    UsbSnapshot,
};
use std::{
    io,
    net::{SocketAddr, TcpListener},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, RwLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};
use transport::Transport;
#[derive(Clone)]
struct State {
    projection: ProjectionSnapshot,
    usb: UsbSnapshot,
    night: bool,
    /// The parking brake is on: the phone lifts its driving restrictions,
    /// unless the I-Bus reports the car moving.
    parked: bool,
    /// A GNSS receiver is connected: a phone connecting now is offered the
    /// car's location.
    gnss: bool,
    /// The I-Bus is read: a phone connecting now is offered the car's speed,
    /// and the cluster's handbrake is the parking brake.
    speedometer: bool,
    /// The car's speed in km/h, while the I-Bus reports it.
    speed: Option<u16>,
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
}
/// Upper bound on the session thread's sleep, so pings and timeouts stay on time.
const SESSION_TICK: Duration = Duration::from_millis(100);
/// How often the head unit server is tried while it is not answering.
const SERVER_RETRY: Duration = Duration::from_secs(5);
/// How often listening on the hotspot's address is tried again after failing,
/// such as while another program holds the port.
const LISTEN_RETRY: Duration = Duration::from_secs(2);
const DECODER_STOPPED: &str = "the H.264 decoder stopped";
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
pub struct ProjectionService {
    state: Arc<RwLock<State>>,
    settings: Arc<Settings>,
    frame_size: FrameSize,
    tx: SyncSender<Command>,
    wake: SyncSender<()>,
    events: EventReceiver,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl ProjectionService {
    /// The DPI and frame rate chosen in Settings, kept in `settings`, replace
    /// core.toml's. Decoded video goes to `frame_path`, for the UI.
    pub fn start(
        config: AndroidAutoConfig,
        audio: AudioConfig,
        frame_path: PathBuf,
        settings: Arc<Settings>,
    ) -> io::Result<Self> {
        let saved = settings.load();
        let state = Arc::new(RwLock::new(State {
            projection: ProjectionSnapshot {
                dpi: saved.dpi.unwrap_or(config.dpi),
                fps: saved.fps.unwrap_or(config.fps),
                ..Default::default()
            },
            usb: Default::default(),
            night: false,
            parked: false,
            gnss: false,
            speedometer: false,
            speed: None,
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
            let frame = (frame_path, video_size);
            if let Err(e) = runner(config, audio, frame, s.clone(), inbox, event_tx, end) {
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
    /// For the next phone too: 30 or 60 frames a second.
    pub fn set_fps(&self, fps: u32) {
        self.state.write().unwrap().projection.fps = fps;
        self.settings.update(|saved| saved.fps = Some(fps));
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
    /// connected phone gets every time, as from a speedometer; and the
    /// handbrake, which while the I-Bus is read is the parking brake, off
    /// until the cluster has reported it.
    pub fn set_vehicle(&self, read: bool, speed: Option<u16>, handbrake: Option<bool>) {
        let (send_speed, brake) = {
            let mut s = self.state.write().unwrap();
            let changed = s.speed != speed;
            s.speedometer = read;
            s.speed = speed;
            let engaged = handbrake == Some(true);
            let brake = (read && s.parked != engaged).then(|| {
                s.parked = engaged;
                engaged
            });
            (
                s.projection.phone_connected && (changed || speed.is_some()),
                brake,
            )
        };
        if let Some(engaged) = brake {
            info!(engaged, "handbrake");
            self.command(Command::ParkingBrake(engaged));
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
fn clear(
    state: &Arc<RwLock<State>>,
    video: &VideoPipeline,
    audio: &AudioPipeline,
    events: &EventSender,
) {
    video.reset();
    audio.reset();
    let mut s = state.write().unwrap();
    s.projection.phone_connected = false;
    s.projection.streaming = false;
    s.projection.phone_name.clear();
    s.projection.phase = ProjectionPhase::WaitingForPhone;
    s.projection.status = "Connect an Android phone by USB".into();
    s.usb.selected = None;
    let _ = events.send(ProjectionEvent::PhoneDisconnected);
}
/// Ask the phone to end its Android Auto session and give it a moment to
/// reply, so it does not keep projecting to a head unit that has gone away.
fn close_session(
    session: &mut session::Session,
    transport: &Transport,
    woken: &Receiver<()>,
) -> Result<(), String> {
    session.shutdown()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut closed = false;
    loop {
        // Media acks keep flowing so the phone's reply is not held up.
        for out in session.outgoing.drain(..) {
            transport.write(&out)?;
        }
        let now = Instant::now();
        if closed || now >= deadline {
            info!(acknowledged = closed, "Android Auto session closed");
            return Ok(());
        }
        match transport.try_read() {
            Some(Ok(bytes)) => {
                closed = session
                    .receive(&bytes)?
                    .iter()
                    .any(|action| matches!(action, session::Action::Shutdown));
            }
            Some(Err(_)) => return Ok(()),
            None => {
                let _ = woken.recv_timeout(deadline - now);
            }
        }
    }
}
/// Development: the head unit server that Android Auto's developer settings
/// start on the phone, tried while no phone is plugged in by USB.
struct HeadUnitServer {
    address: Option<SocketAddr>,
    next_try: Instant,
}
impl HeadUnitServer {
    fn connect(&mut self, wake: &SyncSender<()>) -> Option<tcp::Transport> {
        let address = self.address?;
        let now = Instant::now();
        if now < self.next_try {
            return None;
        }
        // Also spaces out sessions that the server ends at once, such as an
        // `adb forward` whose phone has stopped its server.
        self.next_try = now + SERVER_RETRY;
        tcp::Transport::connect(address, wake)
            .inspect_err(|e| debug!(%e, %address, "head unit server not answering"))
            .ok()
    }
}
/// Phones that come over the hotspot after the Bluetooth handshake. The
/// address exists only while the hotspot is up, but the socket listens on it
/// from the start, so a phone is accepted the moment it can connect, and keeps
/// working as the hotspot comes and goes.
struct WirelessListener {
    address: SocketAddr,
    listener: Option<TcpListener>,
    next_try: Instant,
}
impl WirelessListener {
    fn new(address: SocketAddr) -> Self {
        Self {
            address,
            listener: None,
            next_try: Instant::now(),
        }
    }
    fn accept(&mut self, wake: &SyncSender<()>) -> Option<tcp::Transport> {
        if self.listener.is_none() && Instant::now() >= self.next_try {
            self.next_try = Instant::now() + LISTEN_RETRY;
            match listen_before_address(self.address) {
                Ok(listener) => {
                    info!(address = %self.address, "waiting for wireless Android Auto phones");
                    self.listener = Some(listener);
                }
                Err(e) => warn!(%e, address = %self.address, "cannot wait for wireless phones"),
            }
        }
        let (stream, _) = self.listener.as_ref()?.accept().ok()?;
        tcp::Transport::accepted(stream, wake)
            .inspect_err(|e| warn!(%e, "wireless phone connection failed"))
            .ok()
    }
    /// Sleeps for `pause`, or until a phone connects.
    fn wait(&self, pause: Duration) {
        let Some(listener) = &self.listener else {
            thread::sleep(pause);
            return;
        };
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for the listener's open descriptor.
        unsafe { libc::poll(&mut fd, 1, pause.as_millis() as libc::c_int) };
    }
}
/// A non-blocking listener on `address`, even while no interface has that
/// address (IP_FREEBIND), as the hotspot's while the hotspot starts.
fn listen_before_address(address: SocketAddr) -> io::Result<TcpListener> {
    let SocketAddr::V4(address) = address else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an IPv4 address is needed",
        ));
    };
    let raw = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: address.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(*address.ip()).to_be(),
        },
        sin_zero: [0; 8],
    };
    let on: libc::c_int = 1;
    let on_length = std::mem::size_of_val(&on) as libc::socklen_t;
    // SAFETY: a socket owned here, given an option and an address of the
    // lengths passed.
    unsafe {
        let fd = libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = OwnedFd::from_raw_fd(fd);
        let option = (&raw const on).cast();
        let pointer = (&raw const raw).cast::<libc::sockaddr>();
        let length = std::mem::size_of_val(&raw) as libc::socklen_t;
        if libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_FREEBIND, option, on_length) < 0
            || libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, option, on_length) < 0
            || libc::bind(fd, pointer, length) < 0
            || libc::listen(fd, 8) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(TcpListener::from(socket))
    }
}
fn runner(
    config: AndroidAutoConfig,
    audio: AudioConfig,
    (frame_path, frame_size): (PathBuf, FrameSize),
    state: Arc<RwLock<State>>,
    inbox: Inbox,
    events: EventSender,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let context = rusb::Context::new().map_err(|e| e.to_string())?;
    // A phone still in accessory mode was left by an earlier run that stopped
    // or crashed mid-session. Reset it so its first session here starts fresh.
    let reset = usb::reset_accessories(&context);
    if reset > 0 {
        info!(
            phones = reset,
            "reset Android accessory left by an earlier run"
        );
        thread::sleep(Duration::from_secs(1));
    }
    let video = VideoPipeline::start(
        frame_path,
        config.decoder == DecoderChoice::Auto,
        frame_size,
    )?;
    video.decoder_changed();
    state.write().unwrap().projection.decoder = video.decoder();
    let audio = AudioPipeline::start(&audio, inbox.wake.clone())
        .map_err(|e| format!("start audio workers: {e}"))?;
    let mut rejected = usb::Rejected::default();
    let mut server = HeadUnitServer {
        address: config.head_unit_server,
        next_try: Instant::now(),
    };
    let mut hotspot = WirelessListener::new(SocketAddr::from((HOTSPOT_IP, wireless::PORT)));
    while !stop.load(Ordering::Relaxed) {
        // Without a phone, queued keys must not replay into the next session.
        while inbox.commands.try_recv().is_ok() {}
        let (usb, pause) = match usb::Transport::discover(&context, &inbox.wake, &mut rejected) {
            Ok(found) => (found, Duration::from_millis(500)),
            Err(e) => {
                warn!(%e,"Android USB discovery failed");
                let mut s = state.write().unwrap();
                s.usb.error = Some(e.clone());
                s.projection.status = e;
                s.projection.phase = ProjectionPhase::Error;
                (None, Duration::from_secs(2))
            }
        };
        let Some(transport) = usb
            .map(Transport::Usb)
            .or_else(|| hotspot.accept(&inbox.wake).map(Transport::Wifi))
            .or_else(|| server.connect(&inbox.wake).map(Transport::Tcp))
        else {
            // A phone on the hotspot is accepted at once; USB is looked for
            // again after the pause.
            hotspot.wait(pause);
            continue;
        };
        match &transport {
            Transport::Usb(usb) => info!(
                vid = usb.info.vendor_id,
                pid = usb.info.product_id,
                "Android accessory connected"
            ),
            Transport::Wifi(tcp) => {
                info!(address = %tcp.peer, "wireless Android Auto phone connected")
            }
            Transport::Tcp(tcp) => {
                info!(address = %tcp.peer, "Android Auto head unit server connected")
            }
        }
        video.reset();
        audio.reset();
        let (night, parked, gnss, speedometer, speed, dpi, fps) = {
            let mut s = state.write().unwrap();
            s.usb.selected = transport.usb().cloned();
            s.usb.error = None;
            s.projection.phase = ProjectionPhase::Starting;
            s.projection.status = format!("Negotiating Android Auto over {}", transport.kind());
            (
                s.night,
                s.parked,
                s.gnss,
                s.speedometer,
                s.speed,
                s.projection.dpi,
                s.projection.fps,
            )
        };
        let result = (|| -> Result<(), String> {
            let config = AndroidAutoConfig {
                dpi,
                fps,
                ..config.clone()
            };
            let mut session = session::Session::new(config, night, parked, gnss)?
                .with_speedometer(speedometer, speed);
            let clock = Instant::now();
            let mut shown = false;
            while !stop.load(Ordering::Relaxed) {
                for c in inbox.commands.try_iter().take(128) {
                    match c {
                        Command::Hide => session.hide()?,
                        Command::Resume => session.resume()?,
                        Command::Key(k, p) => session.navigation_input(k, p)?,
                        Command::Touch(kind, x, y) => session.touch(kind, x, y)?,
                        Command::Phone(a) => {
                            let key = session::phone_key(a);
                            session.key(key, true)?;
                            session.key(key, false)?;
                        }
                        Command::Night(n) => session.night(n)?,
                        Command::ParkingBrake(engaged) => session.parking_brake(engaged)?,
                        Command::Location(fix) => session.location(fix)?,
                        Command::Speed(kmh) => session.vehicle_speed(kmh)?,
                    }
                }
                session.tick()?;
                if video.stopped() {
                    return Err(DECODER_STOPPED.into());
                }
                if video.decoder_changed() {
                    state.write().unwrap().projection.decoder = video.decoder();
                }
                for captured in audio.outgoing() {
                    session.microphone(&captured.bytes, clock.elapsed().as_micros() as u64)?;
                }
                for out in session.outgoing.drain(..) {
                    transport.write(&out)?;
                }
                let actions = match transport.try_read() {
                    Some(Ok(bytes)) => session.receive(&bytes)?,
                    Some(Err(e)) => return Err(e),
                    None => {
                        let _ = inbox.woken.recv_timeout(SESSION_TICK);
                        continue;
                    }
                };
                for action in actions {
                    use session::Action;
                    match action {
                        Action::Connected(name) => {
                            let mut s = state.write().unwrap();
                            s.projection.phone_connected = true;
                            s.projection.phone_name = name;
                            s.projection.status = "Android Auto connected".into();
                            let _ = events.send(ProjectionEvent::PhoneConnected);
                        }
                        Action::Video(data) => {
                            match video.push_h264(&data) {
                                PushResult::NeedKeyframe => {
                                    warn!(
                                        "video decoding fell behind; asking the phone for a keyframe"
                                    );
                                    session.request_keyframe()?;
                                }
                                PushResult::Stopped => return Err(DECODER_STOPPED.into()),
                                PushResult::Accepted | PushResult::Dropped => {}
                            }
                            // The state changes once per session, not per frame.
                            if !shown {
                                shown = true;
                                let mut s = state.write().unwrap();
                                s.projection.streaming = true;
                                s.projection.phase = ProjectionPhase::Running;
                                s.projection.status =
                                    format!("Android Auto streaming over {}", transport.kind());
                                let _ = events.send(ProjectionEvent::StreamingReady);
                            }
                        }
                        Action::Audio(stream, data) => {
                            audio.receive_pcm(stream, &data);
                        }
                        Action::StopAudio(stream) => audio.stop_stream(stream),
                        Action::Microphone(active) => audio.recording(active)?,
                        Action::Duck(active) => audio.duck(active),
                        Action::HostUi => {
                            let _ = events.send(ProjectionEvent::RequestHostUi);
                        }
                        Action::Shutdown => {
                            for out in session.outgoing.drain(..) {
                                transport.write(&out)?;
                            }
                            return Ok(());
                        }
                    }
                }
            }
            // The core is stopping.
            close_session(&mut session, &transport, &inbox.woken)
        })();
        clear(&state, &video, &audio, &events);
        if let Err(e) = result {
            transport.reset();
            warn!(%e, link = transport.kind(), "Android Auto session ended");
            let mut s = state.write().unwrap();
            if transport.usb().is_some() {
                s.usb.error = Some(e.clone());
            }
            s.projection.status = e;
        }
        // Video would stay black from now on: Android Auto stops, and the
        // core with it.
        if video.stopped() {
            return Err(DECODER_STOPPED.into());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phones_on_the_hotspot_are_accepted_as_soon_as_they_connect() {
        let (wake, _woken) = mpsc::sync_channel(1);
        // TEST-NET-1 is never assigned, like the hotspot's address while it is
        // off or starting: the listener is ready anyway.
        let mut off = WirelessListener::new(SocketAddr::from(([192, 0, 2, 1], 0)));
        assert!(off.accept(&wake).is_none());
        assert!(off.listener.is_some());
        let free = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut on = WirelessListener::new(free);
        assert!(on.accept(&wake).is_none(), "nobody has connected yet");
        // Without a phone, the wait lasts the whole pause...
        let started = Instant::now();
        on.wait(Duration::from_millis(100));
        assert!(started.elapsed() >= Duration::from_millis(100));
        // ...and a phone connecting ends it at once.
        let phone = std::net::TcpStream::connect(free).unwrap();
        let started = Instant::now();
        on.wait(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
        let transport = on.accept(&wake).expect("the phone was not accepted");
        assert_eq!(transport.peer, phone.local_addr().unwrap());
    }
}
