mod config;
mod ipc;
mod services;
mod settings;

use std::env;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use config::CoreConfig;
use ipc::IpcServer;
use navbridge_common::{AppSnapshot, ClientCommand, ServerEvent};
use services::android_auto::ProjectionService;
use services::codec::Gain;
use services::system::SystemService;
use settings::Settings;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .compact()
        .init();

    if let Err(err) = run() {
        eprintln!("navbridge-core: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let config = CoreConfig::load(&args.config)?;
    // Before any thread starts, so that every thread inherits the blocked signals.
    let stop = stop_requests()?;
    let mut server = IpcServer::bind(
        &config.socket_path,
        config.android_auto.dpi,
        config.android_auto.fps,
    )?;
    // Choices made in Settings, which survive a restart.
    let settings = Arc::new(Settings::new(settings::PATH));
    let system = SystemService::start(config.audio.clone(), settings.clone());
    // Started first, so it stops last: a phone's session ends before the
    // Wi-Fi goes back to the home network.
    let wifi = services::wifi::WifiService::start(args.wireless, settings.clone());
    // Sends phones to the hotspot; up while wireless mode is on.
    let bluetooth = services::bluetooth::BluetoothService::start(wifi.watch());
    let projection = ProjectionService::start(
        config.android_auto.clone(),
        config.audio.clone(),
        settings.clone(),
    )?;
    // While a receiver is connected, the phone gets the car's position.
    let gnss = services::gnss::GnssService::start(projection.gnss_input());
    if config.legacy_ibus.is_some() {
        info!("core.toml's [ibus] section is ignored: NavBridge no longer reads the I-Bus");
    }

    info!(
        ui = "400x240",
        media = "800x480",
        dpi = config.android_auto.dpi,
        transport = "Android USB AOA",
        "NavBridge core started"
    );
    if let Some(address) = config.android_auto.head_unit_server {
        info!(%address, "while no phone is on USB, the phone's head unit server is tried");
    }

    let interval = Duration::from_millis(config.snapshot_interval_ms.clamp(100, 5_000));
    let mut next_snapshot = Instant::now();
    loop {
        server.accept_pending();

        for command in server.read_commands() {
            match command {
                ClientCommand::Hello { client } => info!(%client, "IPC hello"),
                // The size the UI shows projection frames at.
                ClientCommand::SetFrameSize { width, height } => {
                    projection.set_frame_size(width, height)
                }
                ClientCommand::RequestSnapshot => next_snapshot = Instant::now(),
                ClientCommand::SetNightMode { enabled } => projection.set_night_mode(enabled),
                ClientCommand::SetParkingBrake { engaged } => {
                    info!(engaged, "parking brake");
                    projection.set_parking_brake(engaged);
                }
                ClientCommand::HideProjection => projection.hide_session(),
                ClientCommand::ResumeProjection => projection.resume_session(),
                ClientCommand::NavigationInput { key, pressed } => {
                    projection.navigation_input(key, pressed)
                }
                ClientCommand::Touch { kind, x, y } => projection.touch(kind, x, y),
                ClientCommand::PhoneAction { action } => projection.phone_action(action),
                ClientCommand::SetWireless { enabled } => {
                    wifi.set_enabled(enabled);
                    // Show the choice at once; the Wi-Fi follows.
                    next_snapshot = Instant::now();
                }
                ClientCommand::SetDpi { dpi } => {
                    if (1..=1000).contains(&dpi) {
                        info!(dpi, "projection DPI for the next phone");
                        projection.set_dpi(dpi);
                    }
                    next_snapshot = Instant::now();
                }
                ClientCommand::SetFps { fps } => {
                    if matches!(fps, 30 | 60) {
                        info!(fps, "projection frame rate for the next phone");
                        projection.set_fps(fps);
                    }
                    next_snapshot = Instant::now();
                }
                ClientCommand::SetVolume { percent } => system.set_gain(Gain::Speaker, percent),
                ClientCommand::SetMicrophone { percent } => {
                    system.set_gain(Gain::Microphone, percent)
                }
                ClientCommand::StartPairing => {
                    bluetooth.start_pairing();
                    next_snapshot = Instant::now();
                }
                ClientCommand::StopPairing => {
                    bluetooth.stop_pairing();
                    next_snapshot = Instant::now();
                }
                ClientCommand::ConfirmPairing { accept } => {
                    bluetooth.confirm(accept);
                    next_snapshot = Instant::now();
                }
            }
        }

        for event in projection.take_events() {
            server.broadcast_event(ServerEvent::ProjectionEvent { event });
        }

        if Instant::now() >= next_snapshot {
            let phone = projection.snapshot();
            let wireless = wifi.snapshot();
            let session = phone.phone_connected || phone.streaming;
            bluetooth.update(wireless.enabled, wireless.hotspot, session);
            let snapshot = AppSnapshot {
                gnss: gnss.snapshot(),
                usb: projection.usb_snapshot(),
                projection: phone,
                system: system.snapshot(),
                wireless,
                bluetooth: bluetooth.snapshot(),
            };
            server.broadcast_snapshot(snapshot);

            next_snapshot = Instant::now() + interval;
        }
        server.flush_pending();
        if (&stop).read(&mut [0]).is_ok() {
            break;
        }
        // Sleep until a client, the phone, a stop request or the next snapshot
        // needs attention, so a command is forwarded the moment it arrives.
        server.wait(
            &[projection.event_fd(), stop.as_fd()],
            next_snapshot.saturating_duration_since(Instant::now()),
        );
    }
    // Dropping the services closes the phone's session and stops their threads.
    Ok(())
}

/// SIGTERM or SIGINT makes the returned socket readable instead of killing the
/// process, so the phone's session can be closed first; a second signal exits
/// at once. Call before spawning threads, so that they inherit the blocked set.
fn stop_requests() -> io::Result<UnixStream> {
    let (requests, notify) = UnixStream::pair()?;
    requests.set_nonblocking(true)?;
    // SAFETY: builds a local signal set and blocks it for this thread.
    let signals = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        set
    };
    thread::Builder::new()
        .name("stop-signals".into())
        .spawn(move || {
            let mut signal = 0;
            // SAFETY: waits on the valid set built above.
            unsafe { libc::sigwait(&signals, &mut signal) };
            info!(signal, "stopping NavBridge core");
            let _ = (&notify).write_all(&[1]);
            // SAFETY: as above.
            unsafe { libc::sigwait(&signals, &mut signal) };
            std::process::exit(128 + signal);
        })?;
    Ok(requests)
}

struct Args {
    config: PathBuf,
    /// Start with wireless Android Auto on, whatever the saved choice.
    wireless: bool,
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut config = PathBuf::from("config/core.toml");
    let mut wireless = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = PathBuf::from(args.next().ok_or("--config needs a path")?),
            "--wireless" => wireless = true,
            "-h" | "--help" => {
                println!(
                    "NavBridge Core\n\nUsage: navbridge-core [--config PATH] [--wireless]\n\n  --wireless  start with wireless Android Auto on (the hotspot replaces the home Wi-Fi)\n\nMedia session: 800x480; DPI from android_auto.dpi (default 160)"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(Args { config, wireless })
}
