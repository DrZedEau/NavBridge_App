mod config;
mod ipc;
mod services;

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use config::CoreConfig;
use ipc::IpcServer;
use navbridge_common::{AppSnapshot, ClientCommand, ServerEvent};
use services::carplay::CarPlayService;
use services::system::SystemService;
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
    let config_path = parse_args()?;
    let config = CoreConfig::load(&config_path)?;
    let mut server = IpcServer::bind(&config.socket_path, config.carplay.dpi)?;
    let system = SystemService::start();
    let gnss = services::gnss::GnssService::start();
    let carplay = CarPlayService::start(
        config.usb.clone(),
        config.carplay.clone(),
        config.audio.clone(),
    );

    info!(
        ui = "400x240",
        media = "800x480",
        dpi = config.carplay.dpi,
        heartbeat_ms = config.carplay.heartbeat_ms,
        "NavBridge core started"
    );

    let interval = Duration::from_millis(config.snapshot_interval_ms.clamp(100, 5_000));
    let mut next_snapshot = Instant::now();
    loop {
        server.accept_pending();
        carplay.set_gnss_fix(gnss.forwarding_fix());

        for command in server.read_commands() {
            match command {
                ClientCommand::Hello { client } => info!(%client, "IPC hello"),
                ClientCommand::RequestSnapshot => next_snapshot = Instant::now(),
                ClientCommand::ToggleNightMode => carplay.toggle_night_mode(),
                ClientCommand::StartCarplay => carplay.start_session(),
                ClientCommand::ResumeCarplay => carplay.resume_session(),
                ClientCommand::PairPhone => carplay.pair_phone(),
                ClientCommand::StopCarplay => carplay.stop_session(),
                ClientCommand::NavigationInput { key, pressed } => {
                    carplay.navigation_input(key, pressed)
                }
                ClientCommand::PhoneAction { action } => carplay.phone_action(action),
            }
        }

        for event in carplay.take_events() {
            server.broadcast_event(ServerEvent::CarplayEvent { event });
        }

        if Instant::now() >= next_snapshot {
            let snapshot = AppSnapshot {
                gnss: gnss.snapshot(),
                usb: carplay.usb_snapshot(),
                carplay: carplay.snapshot(),
                system: system.snapshot(),
                ibus_status: "IBUS service placeholder - implementation planned".into(),
            };
            server.broadcast_snapshot(snapshot);

            next_snapshot = Instant::now() + interval;
        }
        server.flush_pending();
        // Input must not wait for the relatively slow status snapshot interval.
        thread::sleep(Duration::from_millis(10));
    }
}

fn parse_args() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut config = PathBuf::from("config/core.toml");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = PathBuf::from(args.next().ok_or("--config needs a path")?),
            "-h" | "--help" => {
                println!(
                    "NavBridge Core\n\nUsage: navbridge-core [--config PATH]\n\nMedia session: 800x480; DPI from carplay.dpi (default 160)"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(config)
}
