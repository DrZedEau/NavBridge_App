//! The session thread: it looks for a phone, runs its session until it ends,
//! and passes video, sound and events on.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use super::listen::{HeadUnitServer, WirelessListener};
use super::service::CarBluetooth;
use super::transport::Transport;
use super::{session, usb, wireless, Command, EventSender, Inbox, State};
use crate::config::{AndroidAutoConfig, AudioConfig, DecoderChoice};
use crate::services::audio::AudioPipeline;
use crate::services::video::{FrameSize, PushResult, VideoPipeline};
use crate::services::wifi::HOTSPOT_IP;
use navbridge_common::{ProjectionEvent, ProjectionPhase, VideoCodec};
use tracing::{info, warn};

/// Upper bound on the session thread's sleep, so pings and timeouts stay on time.
pub(super) const SESSION_TICK: Duration = Duration::from_millis(100);

pub(super) const DECODER_STOPPED: &str = "the video decoder stopped";

/// What the session thread runs with, fixed for the core's run.
pub(super) struct Setup {
    pub(super) config: AndroidAutoConfig,
    pub(super) audio: AudioConfig,
    pub(super) frame_path: PathBuf,
    pub(super) frame_size: FrameSize,
    pub(super) bluetooth: CarBluetooth,
}

pub(super) fn clear(
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
    s.projection.wireless = false;
    s.projection.phone_name.clear();
    s.projection.phase = ProjectionPhase::WaitingForPhone;
    s.projection.status = "Connect an Android phone by USB".into();
    s.usb.selected = None;
    let _ = events.send(ProjectionEvent::PhoneDisconnected);
}

/// Ask the phone to end its Android Auto session and give it a moment to
/// reply, so it does not keep projecting to a head unit that has gone away.
pub(super) fn close_session(
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

pub(super) fn runner(
    setup: Setup,
    state: Arc<RwLock<State>>,
    inbox: Inbox,
    events: EventSender,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let Setup {
        config,
        audio,
        frame_path,
        frame_size,
        bluetooth,
    } = setup;
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
        let (night, parked, gnss, speedometer, speed, dpi, codec) = {
            let mut s = state.write().unwrap();
            s.usb.selected = transport.usb().cloned();
            s.usb.error = None;
            s.projection.wireless = matches!(transport, Transport::Wifi(_));
            s.projection.phase = ProjectionPhase::Starting;
            s.projection.status = format!("Negotiating Android Auto over {}", transport.kind());
            (
                s.night,
                s.parked,
                s.gnss,
                s.speedometer,
                s.speed,
                s.projection.dpi,
                s.projection.codec,
            )
        };
        // Without FFmpeg's H.265 decoder, the phone is asked for H.264.
        let codec = if video.decodes(codec) {
            codec
        } else {
            warn!(
                ?codec,
                "FFmpeg cannot decode this codec; asking the phone for H.264"
            );
            VideoCodec::H264
        };
        let result = (|| -> Result<(), String> {
            let config = {
                let mut config = config.clone();
                config.dpi = dpi;
                config.codec = codec;
                config
            };
            // Asked of BlueZ at each phone, as bluez-alsa may have started.
            let hands_free = (bluetooth.hands_free_address)();
            if let Some(address) = &hands_free {
                info!(%address, "Android Auto is offered the car's Bluetooth for calls");
            }
            let mut session = session::Session::new(config, night, parked, gnss)?
                .with_speedometer(speedometer, speed)
                .with_bluetooth(hands_free);
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
                        Command::Disconnect => {
                            if let Transport::Wifi(_) = &transport {
                                info!("ending the wireless Android Auto session: Settings disconnected the phone");
                                return close_session(&mut session, &transport, &inbox.woken);
                            }
                        }
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
                        Action::VideoCodec(codec) => video.set_codec(codec),
                        Action::Video(data) => {
                            match video.push(&data) {
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
                        Action::BluetoothPairing(phone) => {
                            let paired = (bluetooth.paired)(&phone);
                            info!(%phone, paired, "the phone asks whether the car's Bluetooth knows it");
                            session.bluetooth_paired(paired)?;
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
