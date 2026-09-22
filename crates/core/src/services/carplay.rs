use super::gnss_forward::{GnssFix, Forwarder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusb::{Context, Direction, TransferType};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::{AudioConfig, CarplayConfig, UsbConfig};
use crate::services::audio::AudioPipeline;
use crate::services::media::{PushResult, VideoPipeline};
use crate::services::protocol::{
    android_auto_rearm_messages, initialization_messages, night_mode_messages, payload_u32, send_message, MessageParser,
    phone_action_message, NavigationButtons, ProtocolMessage, CMD_AUDIO, CMD_BLUETOOTH_CONNECTED, CMD_BLUETOOTH_CONNECT_START,
    CMD_BLUETOOTH_DISCONNECTED, CMD_BLUETOOTH_LISTEN, CMD_CONTROL, CMD_JSON_CONTROL,
    CMD_MEDIA_DATA, CMD_PHASE, CMD_PLUGGED, CMD_SOFTWARE_VERSION, CMD_UNPLUGGED, CMD_VIDEO,
    CONTROL_REQUEST_HOST_UI, CONTROL_REQUEST_KEYFRAME, CMD_BLUETOOTH_NAME, CMD_WIFI_NAME,
};
use crate::services::usb;
use navbridge_common::{
    CarplayEvent, CarplayPhase, CarplaySnapshot, PhoneAction, NavigationKey, UsbSnapshot, MEDIA_HEIGHT, MEDIA_WIDTH,
    VIDEO_FRAME_PATH,
};

#[derive(Clone)]
struct RuntimeState {
    gnss_fix: Option<GnssFix>,
    night_mode: bool,
    usb: UsbSnapshot,
    carplay: CarplaySnapshot,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            gnss_fix: None,
            night_mode: false,
            usb: UsbSnapshot::default(),
            carplay: CarplaySnapshot::default(),
        }
    }
}

enum ServiceCommand {
    AutoConnect,
    Resume,
    PairPhone,
    DisconnectPhone,
    NavigationInput { key: NavigationKey, pressed: bool },
    PhoneAction(PhoneAction),
}

pub struct CarPlayService {
    state: Arc<RwLock<RuntimeState>>,
    stop: Arc<AtomicBool>,
    command_tx: Sender<ServiceCommand>,
    event_rx: Receiver<CarplayEvent>,
    thread: Option<JoinHandle<()>>,
}

impl CarPlayService {
    pub fn start(usb_config: UsbConfig, carplay_config: CarplayConfig, audio_config: AudioConfig) -> Self {
        let state = Arc::new(RwLock::new(RuntimeState {
            carplay: CarplaySnapshot { dpi: carplay_config.dpi, ..Default::default() },
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            runner(
                usb_config,
                carplay_config,
                audio_config,
                thread_state,
                thread_stop,
                command_rx,
                event_tx,
            )
        });

        Self {
            state,
            stop,
            command_tx,
            event_rx,
            thread: Some(thread),
        }
    }

    /// Re-send the adapter auto-connect command. The normal path is automatic;
    /// this remains useful as a manual retry action from the UI.
    pub fn start_session(&self) {
        let _ = self.command_tx.send(ServiceCommand::AutoConnect);
    }

    /// Explicitly disconnect the phone from the adapter. Navigating back to the
    /// NavBridge menu does NOT call this; the phone session may continue in the
    /// background after RequestHostUI.
    pub fn stop_session(&self) {
        let _ = self.command_tx.send(ServiceCommand::DisconnectPhone);
    }

    pub fn pair_phone(&self) {
        let _ = self.command_tx.send(ServiceCommand::PairPhone);
    }

    pub fn resume_session(&self) {
        let _ = self.command_tx.send(ServiceCommand::Resume);
    }

    pub fn set_gnss_fix(&self, fix: Option<GnssFix>) {
        if let Ok(mut state) = self.state.write() { state.gnss_fix = fix; }
    }

    pub fn toggle_night_mode(&self) {
        if let Ok(mut state) = self.state.write() {
            state.night_mode = !state.night_mode;
            info!(enabled = state.night_mode, "night mode requested");
        }
    }

    pub fn phone_action(&self, action: PhoneAction) {
        let _ = self.command_tx.send(ServiceCommand::PhoneAction(action));
    }

    pub fn navigation_input(&self, key: NavigationKey, pressed: bool) {
        let _ = self.command_tx.send(ServiceCommand::NavigationInput { key, pressed });
    }

    pub fn usb_snapshot(&self) -> UsbSnapshot {
        self.state
            .read()
            .map(|state| state.usb.clone())
            .unwrap_or_else(|_| UsbSnapshot {
                error: Some("CarPlay state lock poisoned".into()),
                ..UsbSnapshot::default()
            })
    }

    pub fn snapshot(&self) -> CarplaySnapshot {
        self.state
            .read()
            .map(|state| state.carplay.clone())
            .unwrap_or_else(|_| CarplaySnapshot {
                phase: CarplayPhase::Error,
                status: "CarPlay state lock poisoned".into(),
                ..CarplaySnapshot::default()
            })
    }

    pub fn take_events(&self) -> Vec<CarplayEvent> {
        self.event_rx.try_iter().collect()
    }
}

impl Drop for CarPlayService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn runner(
    usb_config: UsbConfig,
    carplay_config: CarplayConfig,
    audio_config: AudioConfig,
    state: Arc<RwLock<RuntimeState>>,
    stop: Arc<AtomicBool>,
    command_rx: Receiver<ServiceCommand>,
    event_tx: Sender<CarplayEvent>,
) {
    let context = match Context::new() {
        Ok(context) => context,
        Err(err) => {
            set_error(&state, format!("libusb init failed: {err}"));
            return;
        }
    };

    while !stop.load(Ordering::Relaxed) {
        // Commands sent while no dongle is available are retry hints only.
        while command_rx.try_recv().is_ok() {}

        let selected = match usb::scan(&context) {
            Ok(selected) => selected,
            Err(err) => {
                set_error(&state, format!("USB scan failed: {err}"));
                sleep_interruptible(&stop, usb_config.scan_interval_ms);
                continue;
            }
        };

        let Some(selected) = selected else {
            set_waiting_for_dongle(&state);
            sleep_interruptible(&stop, usb_config.scan_interval_ms);
            continue;
        };

        if let Ok(mut shared) = state.write() {
            shared.usb = selected.snapshot.clone();
            shared.carplay.dongle_firmware.clear();
            shared.carplay.dongle_bluetooth_name.clear();
            shared.carplay.phase = CarplayPhase::DongleReady;
            shared.carplay.status = "Dongle detected - resetting USB transport".into();
            shared.carplay.negotiated_width = MEDIA_WIDTH;
            shared.carplay.negotiated_height = MEDIA_HEIGHT;
            shared.carplay.dpi = carplay_config.dpi;
        }

        info!(
            vendor = %format!("{:04x}", selected.info.vendor_id),
            product = %format!("{:04x}", selected.info.product_id),
            bus = selected.info.bus,
            address = selected.info.address,
            reset_delay_ms = usb_config.reset_delay_ms,
            "preparing Carlinkit USB transport"
        );

        // The adapter reset causes a USB re-enumeration. The old libusb handle
        // must not be used afterwards: reset, drop it, wait ~3 seconds, then
        // locate the newly enumerated device and open a fresh handle.
        let selected = match reset_and_rediscover(
            &context,
            selected,
            &usb_config,
            &state,
            &stop,
        ) {
            Ok(selected) => selected,
            Err(err) => {
                if !stop.load(Ordering::Relaxed) {
                    warn!(%err, "Carlinkit reset/rediscovery failed");
                    set_error(&state, err);
                    sleep_interruptible(&stop, usb_config.scan_interval_ms);
                }
                continue;
            }
        };

        if let Ok(mut shared) = state.write() {
            shared.usb = selected.snapshot.clone();
            shared.carplay.dongle_firmware.clear();
            shared.carplay.dongle_bluetooth_name.clear();
            shared.carplay.phase = CarplayPhase::DongleReady;
            shared.carplay.status = "Dongle re-enumerated - opening protocol session".into();
        }

        info!(
            vendor = %format!("{:04x}", selected.info.vendor_id),
            product = %format!("{:04x}", selected.info.product_id),
            bus = selected.info.bus,
            address = selected.info.address,
            "opening Carlinkit protocol session after reset"
        );

        if let Err(err) = run_usb_session(
            selected.device,
            (&usb_config, &carplay_config, &audio_config),
            &state,
            &stop,
            &command_rx,
            &event_tx,
        ) {
            if !stop.load(Ordering::Relaxed) {
                warn!(%err, "Carlinkit USB session ended");
                set_error(&state, err);
            }
        }

        if !stop.load(Ordering::Relaxed) {
            sleep_interruptible(&stop, usb_config.scan_interval_ms);
        }
    }
}

fn reset_and_rediscover(
    context: &Context,
    selected: usb::SelectedUsbDevice,
    usb_config: &UsbConfig,
    state: &Arc<RwLock<RuntimeState>>,
    stop: &Arc<AtomicBool>,
) -> Result<usb::SelectedUsbDevice, String> {
    let original_vendor = selected.info.vendor_id;
    let original_product = selected.info.product_id;
    let original_bus = selected.info.bus;
    let original_address = selected.info.address;

    let handle = selected
        .device
        .open()
        .map_err(|e| format!("open dongle for reset: {e}"))?;

    info!(
        bus = original_bus,
        address = original_address,
        "issuing Carlinkit USB reset"
    );

    // libusb may report NotFound/NoDevice when the reset itself already made
    // the gadget disappear. That is the expected transition to re-enumeration.
    match handle.reset() {
        Ok(()) => {}
        Err(rusb::Error::NotFound | rusb::Error::NoDevice) => {
            debug!("dongle disappeared during reset as expected");
        }
        Err(err) => return Err(format!("reset dongle: {err}")),
    }
    drop(handle);

    let reset_delay_ms = usb_config.reset_delay_ms.clamp(2_500, 10_000);
    if let Ok(mut shared) = state.write() {
        shared.carplay.status = format!(
            "Dongle reset - waiting {} ms for USB re-enumeration",
            reset_delay_ms
        );
    }
    sleep_interruptible(stop, reset_delay_ms);
    if stop.load(Ordering::Relaxed) {
        return Err("stopped while waiting for dongle re-enumeration".into());
    }

    // Give slow firmware a few more seconds beyond the mandatory reset delay.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match usb::scan(context) {
            Ok(Some(candidate)) => {
                let same_family = candidate.info.vendor_id == original_vendor
                    && (candidate.info.product_id == original_product
                        || candidate.info.vendor_id == 0x1314);
                if same_family {
                    info!(
                        old_bus = original_bus,
                        old_address = original_address,
                        new_bus = candidate.info.bus,
                        new_address = candidate.info.address,
                        vendor = %format!("{:04x}", candidate.info.vendor_id),
                        product = %format!("{:04x}", candidate.info.product_id),
                        "Carlinkit dongle re-enumerated"
                    );
                    return Ok(candidate);
                }
            }
            Ok(None) => {}
            Err(err) => debug!(%err, "USB scan during reset settle failed"),
        }

        if Instant::now() >= deadline {
            return Err("dongle did not re-enumerate after USB reset".into());
        }
        sleep_interruptible(stop, 250);
        if stop.load(Ordering::Relaxed) {
            return Err("stopped while rediscovering dongle".into());
        }
    }
}

fn run_usb_session(
    device: rusb::Device<Context>,
    configs: (&UsbConfig, &CarplayConfig, &AudioConfig),
    state: &Arc<RwLock<RuntimeState>>,
    stop: &Arc<AtomicBool>,
    command_rx: &Receiver<ServiceCommand>,
    event_tx: &Sender<CarplayEvent>,
) -> Result<(), String> {
    let (usb_config, carplay_config, audio_config) = configs;
    let handle = device.open().map_err(|e| format!("open dongle: {e}"))?;
    if let Err(err) = handle.set_auto_detach_kernel_driver(true) {
        debug!(%err, "libusb auto-detach unavailable; continuing");
    }

    // Configuration 1 is normally already active after re-enumeration. Avoid
    // forcing it unless necessary; needless set-configuration calls can tear
    // down the gadget endpoints on some firmware revisions.
    match handle.active_configuration() {
        Ok(1) => {}
        Ok(current) => {
            info!(current, "switching dongle to USB configuration 1");
            handle
                .set_active_configuration(1)
                .map_err(|e| format!("set USB configuration 1: {e}"))?;
        }
        Err(err) => debug!(%err, "could not query active USB configuration"),
    }

    handle
        .claim_interface(0)
        .map_err(|e| format!("claim USB interface 0: {e}"))?;

    let result = (|| {
        let descriptor = device
            .active_config_descriptor()
            .map_err(|e| format!("read active USB config: {e}"))?;
        let endpoints = find_bulk_endpoints(&descriptor)
            .ok_or_else(|| "bulk IN/OUT endpoints not found on interface 0".to_string())?;

        if endpoints.alternate_setting != 0 {
            handle
                .set_alternate_setting(0, endpoints.alternate_setting)
                .map_err(|e| {
                    format!(
                        "select USB interface 0 alt {}: {e}",
                        endpoints.alternate_setting
                    )
                })?;
        }

        if endpoints.endpoint_in != 0x83 || endpoints.endpoint_out != 0x02 {
            warn!(
                endpoint_in = %format!("0x{:02x}", endpoints.endpoint_in),
                endpoint_out = %format!("0x{:02x}", endpoints.endpoint_out),
                max_packet_in = endpoints.max_packet_in,
                max_packet_out = endpoints.max_packet_out,
                "dongle endpoints differ from reference 0x83/0x02; using descriptors"
            );
        }

        info!(
            endpoint_in = %format!("0x{:02x}", endpoints.endpoint_in),
            endpoint_out = %format!("0x{:02x}", endpoints.endpoint_out),
            alternate_setting = endpoints.alternate_setting,
            max_packet_in = endpoints.max_packet_in,
            max_packet_out = endpoints.max_packet_out,
            heartbeat_ms = carplay_config.heartbeat_ms,
            write_timeout_ms = usb_config.write_timeout_ms,
            "Carlinkit USB interface claimed"
        );

        let read_timeout = Duration::from_millis(usb_config.io_timeout_ms.clamp(20, 500));
        let write_timeout =
            Duration::from_millis(usb_config.write_timeout_ms.clamp(500, 5_000));
        let heartbeat_interval =
            Duration::from_millis(carplay_config.heartbeat_ms.clamp(1_000, 5_000));
        let init_delay =
            Duration::from_millis(usb_config.init_command_delay_ms.clamp(50, 500));

        // Protocol reference: start the heartbeat timer when the fresh USB
        // session opens, but DO NOT send a heartbeat at t=0. Initialization is
        // paced first; the initial heartbeat becomes due at t=2000 ms.
        let session_started = Instant::now();
        let mut next_heartbeat = session_started + heartbeat_interval;

        if let Ok(mut shared) = state.write() {
            shared.carplay.status = format!(
                "Initializing dongle - first heartbeat in {} ms",
                heartbeat_interval.as_millis()
            );
        }

        let mut applied_night_mode = state.read().map(|s| s.night_mode).unwrap_or(false);
        for message in initialization_messages(carplay_config, applied_night_mode) {
            send_message(
                &handle,
                endpoints.endpoint_out,
                &message,
                write_timeout,
            )
            .map_err(|e| format!("initialization command 0x{:02x}: {e}", message.kind))?;

            debug!(kind = message.kind, "Carlinkit initialization command sent");
            wait_with_heartbeat(
                &handle,
                endpoints.endpoint_out,
                state,
                &mut next_heartbeat,
                heartbeat_interval,
                write_timeout,
                init_delay,
            )?;
        }

        set_dongle_online(state, carplay_config.heartbeat_ms);

        let video = match VideoPipeline::start(VIDEO_FRAME_PATH) {
            Ok(video) => Some(video),
            Err(err) => {
                warn!(%err, "software video decoder unavailable; projection video cannot be displayed");
                let _ = update_carplay(state, |carplay| {
                    carplay.status = format!("Dongle online; video decoder unavailable: {err}");
                });
                None
            }
        };

        let audio = AudioPipeline::start(audio_config);
        let mut navigation = NavigationButtons::default();
        let mut parser = MessageParser::new();
        let mut read_buffer = vec![0_u8; usb_config.read_size.clamp(2048, 256 * 1024)];
        let mut last_android_rearm: Option<Instant> = None;
        let mut gnss_forwarder = Forwarder::default();
        let mut gnss_packets = 0u64;

        while !stop.load(Ordering::Relaxed) {
            let (fix, phone_connected) = state.read().map(|s| (s.gnss_fix.clone(), s.carplay.phone_connected)).unwrap_or_default();
            for message in gnss_forwarder.messages(fix.as_ref(), phone_connected, Instant::now()) {
                send_message(&handle, endpoints.endpoint_out, &message, write_timeout)
                    .map_err(|e| format!("forward external GNSS: {e}"))?;
                if message.kind == 0x29 {
                    gnss_packets += 1;
                    if gnss_packets == 1 {
                        info!(bytes = message.payload.len(), "first external GNSS fix sent to dongle");
                    }
                    debug!(gnss_packets, bytes = message.payload.len(), "external GNSS fix sent to dongle");
                } else {
                    info!(enabled = payload_u32(&message) == Some(18), "external GNSS forwarding changed");
                }
            }
            let desired = state.read().map(|s| s.night_mode).unwrap_or(applied_night_mode);
            if desired != applied_night_mode {
                for message in night_mode_messages(desired) {
                    send_message(&handle, endpoints.endpoint_out, &message, write_timeout)
                        .map_err(|e| format!("set night mode: {e}"))?;
                }
                applied_night_mode = desired;
                info!(enabled = desired, "night mode sent to dongle");
            }
            loop {
                match command_rx.try_recv() {
                    Ok(ServiceCommand::AutoConnect) => {
                        // Guard against stale UI snapshots and older clients:
                        // opening a view must never reconfigure a live phone.
                        let active = state.read()
                            .map(|s| s.carplay.phone_connected || s.carplay.streaming)
                            .unwrap_or(true);
                        if active {
                            debug!("connection retry ignored: phone session already active");
                            continue;
                        }
                        info!("manual AA/CarPlay retry: re-applying Android work mode and auto-connect");
                        for message in android_auto_rearm_messages(carplay_config) {
                            send_message(
                                &handle,
                                endpoints.endpoint_out,
                                &message,
                                write_timeout,
                            )
                            .map_err(|e| format!("send auto-connect retry: {e}"))?;
                            thread::sleep(Duration::from_millis(80));
                        }
                    }
                    Ok(ServiceCommand::PairPhone) => {
                        let active = state.read().map(|s| s.carplay.phone_connected || s.carplay.streaming).unwrap_or(true);
                        if active { continue; }
                        // Android pairs through the advertised Bluetooth device.
                        // Command 30 also requests BLE advertising for CarPlay.
                        // This does not erase bonds or bypass the phone's consent.
                        for message in android_auto_rearm_messages(carplay_config).into_iter()
                            .chain(std::iter::once(ProtocolMessage::control(30))) {
                            send_message(&handle, endpoints.endpoint_out, &message, write_timeout)
                                .map_err(|e| format!("request phone pairing: {e}"))?;
                            thread::sleep(Duration::from_millis(80));
                        }
                        let _ = update_carplay(state, |carplay| {
                            carplay.status = "Select the dongle in your phone's Bluetooth settings".into();
                        });
                        info!("phone pairing requested; awaiting phone-side Bluetooth approval");
                    }
                    Ok(ServiceCommand::Resume) => {
                        let active = state.read()
                            .map(|s| s.carplay.phone_connected || s.carplay.streaming)
                            .unwrap_or(false);
                        if active {
                            // AutoKit requests a fresh frame when its video surface
                            // returns. 500/501 are adapter-to-host focus notifications;
                            // sending 500 does not restart the paused AA stream.
                            send_message(&handle, endpoints.endpoint_out,
                                &ProtocolMessage::control(CONTROL_REQUEST_KEYFRAME), write_timeout)
                                .map_err(|e| format!("request frame when resuming projection: {e}"))?;
                            info!("requested fresh frame to resume existing phone session");
                        }
                    }
                    Ok(ServiceCommand::PhoneAction(action)) => {
                        let connected = state.read().map(|s| s.carplay.phone_connected).unwrap_or(false);
                        if connected {
                            send_message(&handle, endpoints.endpoint_out, &phone_action_message(action), write_timeout)
                                .map_err(|e| format!("send phone action: {e}"))?;
                            info!(?action, "phone shortcut sent");
                        }
                    }
                    Ok(ServiceCommand::NavigationInput { key, pressed }) => {
                        let connected = state.read().map(|s| s.carplay.phone_connected).unwrap_or(false);
                        if !connected {
                            navigation = NavigationButtons::default();
                            continue;
                        }
                        for message in navigation.update(key, pressed) {
                            send_message(&handle, endpoints.endpoint_out, &message, write_timeout)
                                .map_err(|e| format!("send navigation key: {e}"))?;
                            debug!(?key, pressed, command = ?payload_u32(&message), "phone navigation input sent");
                        }
                    }
                    Ok(ServiceCommand::DisconnectPhone) => {
                        audio.reset();
                        send_message(
                            &handle,
                            endpoints.endpoint_out,
                            &ProtocolMessage::disconnect_phone(),
                            write_timeout,
                        )
                        .map_err(|e| format!("disconnect phone: {e}"))?;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }

            send_due_heartbeat(
                &handle,
                endpoints.endpoint_out,
                state,
                &mut next_heartbeat,
                heartbeat_interval,
                write_timeout,
            )?;

            // The dongle expects separate reads for its transport header and payload.
            // Do not let a large read consume the next transfer out of phase.
            for message in audio.outgoing() {
                send_message(&handle, endpoints.endpoint_out, &message, write_timeout)
                    .map_err(|e| format!("send microphone PCM: {e}"))?;
            }
            let read_timeout = if audio.active() { read_timeout.min(Duration::from_millis(10)) } else { read_timeout };
            let read_len = parser.bytes_needed().min(read_buffer.len());
            match handle.read_bulk(endpoints.endpoint_in, &mut read_buffer[..read_len], read_timeout) {
                Ok(0) => {}
                Ok(count) => {
                    parser.push(&read_buffer[..count]);
                    while let Some(message) = parser.next() {
                        audio.receive(&message);
                        let actions = handle_protocol_message(message, state, event_tx, video.as_ref());

                        if actions.request_keyframe {
                            debug!("requesting fresh H.264 keyframe after decoder queue drop");
                            send_message(
                                &handle,
                                endpoints.endpoint_out,
                                &ProtocolMessage::control(CONTROL_REQUEST_KEYFRAME),
                                write_timeout,
                            )
                            .map_err(|e| format!("request video keyframe: {e}"))?;
                        }

                        if actions.rearm_android {
                            navigation = NavigationButtons::default();
                            if let Some(video) = video.as_ref() {
                                video.reset();
                            }
                            let should_rearm = last_android_rearm
                                .map(|last| last.elapsed() >= Duration::from_millis(500))
                                .unwrap_or(true);
                            if should_rearm {
                                info!("re-applying Android Auto work mode after session reset");
                                for rearm in android_auto_rearm_messages(carplay_config) {
                                    send_message(
                                        &handle,
                                        endpoints.endpoint_out,
                                        &rearm,
                                        write_timeout,
                                    )
                                    .map_err(|e| format!("re-arm Android Auto: {e}"))?;
                                    thread::sleep(Duration::from_millis(80));
                                }
                                last_android_rearm = Some(Instant::now());
                            }
                        }
                    }
                    if let Some(video) = video.as_ref() {
                        update_video_counters(state, video);
                    }
                }
                Err(rusb::Error::Timeout) => {}
                Err(err) => return Err(format!("USB bulk read: {err}")),
            }
        }

        Ok(())
    })();

    let _ = handle.release_interface(0);
    result
}

fn wait_with_heartbeat<T: rusb::UsbContext>(
    handle: &rusb::DeviceHandle<T>,
    endpoint_out: u8,
    state: &Arc<RwLock<RuntimeState>>,
    next_heartbeat: &mut Instant,
    heartbeat_interval: Duration,
    write_timeout: Duration,
    delay: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + delay;
    loop {
        send_due_heartbeat(
            handle,
            endpoint_out,
            state,
            next_heartbeat,
            heartbeat_interval,
            write_timeout,
        )?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

fn send_due_heartbeat<T: rusb::UsbContext>(
    handle: &rusb::DeviceHandle<T>,
    endpoint_out: u8,
    state: &Arc<RwLock<RuntimeState>>,
    next_heartbeat: &mut Instant,
    heartbeat_interval: Duration,
    write_timeout: Duration,
) -> Result<(), String> {
    let now = Instant::now();
    if now < *next_heartbeat {
        return Ok(());
    }

    send_message(
        handle,
        endpoint_out,
        &ProtocolMessage::heartbeat(),
        write_timeout,
    )
    .map_err(|e| format!("heartbeat: {e}"))?;
    let heartbeat_count = bump_heartbeat(state);
    debug!(heartbeat_count, "Carlinkit heartbeat sent");

    // Advance from the scheduled time so heartbeat cadence does not drift.
    // If the process was delayed for more than one interval, do not burst-send
    // stale heartbeats; schedule the next one from 'now'.
    *next_heartbeat += heartbeat_interval;
    if *next_heartbeat <= Instant::now() {
        *next_heartbeat = Instant::now() + heartbeat_interval;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct BulkEndpoints {
    endpoint_in: u8,
    endpoint_out: u8,
    alternate_setting: u8,
    max_packet_in: u16,
    max_packet_out: u16,
}

fn find_bulk_endpoints(descriptor: &rusb::ConfigDescriptor) -> Option<BulkEndpoints> {
    let mut fallback = None;

    for interface in descriptor.interfaces() {
        for interface_desc in interface.descriptors() {
            if interface_desc.interface_number() != 0 {
                continue;
            }

            let mut endpoint_in = None;
            let mut endpoint_out = None;
            let mut max_packet_in = 0;
            let mut max_packet_out = 0;

            for endpoint in interface_desc.endpoint_descriptors() {
                if endpoint.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match endpoint.direction() {
                    Direction::In => {
                        endpoint_in = Some(endpoint.address());
                        max_packet_in = endpoint.max_packet_size();
                    }
                    Direction::Out => {
                        endpoint_out = Some(endpoint.address());
                        max_packet_out = endpoint.max_packet_size();
                    }
                }
            }

            let (Some(endpoint_in), Some(endpoint_out)) = (endpoint_in, endpoint_out) else {
                continue;
            };

            let candidate = BulkEndpoints {
                endpoint_in,
                endpoint_out,
                alternate_setting: interface_desc.setting_number(),
                max_packet_in,
                max_packet_out,
            };

            // Prefer the capture-verified f_accessory endpoints. Fall back to
            // descriptors for older/custom firmware that exposes different
            // endpoint numbers.
            if endpoint_in == 0x83 && endpoint_out == 0x02 {
                return Some(candidate);
            }
            if fallback.is_none() {
                fallback = Some(candidate);
            }
        }
    }

    fallback
}

#[derive(Debug, Default, Clone, Copy)]
struct MessageActions {
    request_keyframe: bool,
    rearm_android: bool,
}

fn handle_protocol_message(
    message: ProtocolMessage,
    state: &Arc<RwLock<RuntimeState>>,
    event_tx: &Sender<CarplayEvent>,
    video: Option<&VideoPipeline>,
) -> MessageActions {
    let mut actions = MessageActions::default();

    if message.encrypted {
        warn!(kind = message.kind, "encrypted protocol message ignored; encryption is not enabled by NavBridge");
        return actions;
    }

    match message.kind {
        CMD_PLUGGED => {
            let phone_type = payload_u32(&message);
            let (method, transport) = phone_connection(&message.payload);
            let newly_connected = update_carplay(state, |carplay| {
                let newly_connected = !carplay.phone_connected;
                carplay.phone_connected = true;
                carplay.streaming = false;
                carplay.connection_method = method.into();
                carplay.connection_transport = transport.into();
                carplay.phase = CarplayPhase::Starting;
                carplay.status = match phone_type {
                    Some(5) => "Android Auto connected - waiting for video".into(),
                    Some(3) | Some(8) => "CarPlay connected - waiting for video".into(),
                    _ => "Phone connected - negotiating AA / CarPlay".into(),
                };
                newly_connected
            });
            info!(?phone_type, "Carlinkit phone Plugged event");
            if newly_connected.unwrap_or(false) {
                let _ = event_tx.send(CarplayEvent::PhoneConnected);
            }
        }
        CMD_UNPLUGGED => {
            end_phone_session(state, event_tx, "Phone disconnected");
            actions.rearm_android = true;
        }
        CMD_PHASE => {
            if let Some(phase) = payload_u32(&message) {
                debug!(phase, "Carlinkit projection phase");
                match phase {
                    0 => {
                        end_phone_session(state, event_tx, "AA / CarPlay session ended");
                        actions.rearm_android = true;
                    }
                    7 => {
                        let newly_connected = update_carplay(state, |carplay| {
                            let newly_connected = !carplay.phone_connected;
                            carplay.protocol_phase = Some(phase);
                            carplay.phone_connected = true;
                            carplay.streaming = false;
                            carplay.phase = CarplayPhase::Starting;
                            carplay.status = "AA / CarPlay connecting".into();
                            newly_connected
                        });
                        if newly_connected.unwrap_or(false) {
                            let _ = event_tx.send(CarplayEvent::PhoneConnected);
                        }
                    }
                    8 => {
                        mark_streaming(state, event_tx, "AA / CarPlay streaming");
                        let _ = update_carplay(state, |carplay| carplay.protocol_phase = Some(phase));
                    }
                    other => {
                        let _ = update_carplay(state, |carplay| {
                            carplay.protocol_phase = Some(other);
                        });
                    }
                }
            }
        }
        CMD_CONTROL => {
            if let Some(command) = payload_u32(&message) {
                match command {
                    CONTROL_REQUEST_HOST_UI => {
                        info!("CarPlay/AA requested host UI");
                        let _ = event_tx.send(CarplayEvent::RequestHostUi);
                    }
                    1002 => {
                        set_link_status(state, event_tx, "Phone auto-connect started", false);
                    }
                    1003 => {
                        set_link_status(state, event_tx, "Scanning for paired phone", false);
                    }
                    1004 => {
                        set_link_status(state, event_tx, "Paired phone found", true);
                    }
                    1005 => {
                        let _ = update_carplay(state, |carplay| {
                            if !carplay.phone_connected { carplay.status = "No saved phone found. Select Pair phone.".into(); }
                        });
                    }
                    1006 => {
                        set_link_status(state, event_tx, "Phone connection attempt failed", false);
                    }
                    1007 => {
                        set_link_status(state, event_tx, "Phone Bluetooth connected", true);
                    }
                    1008 => {
                        set_link_status(state, event_tx, "Phone Bluetooth disconnected", false);
                    }
                    1009 => {
                        set_link_status(state, event_tx, "Phone Wi-Fi connected", true);
                    }
                    1010 => {
                        set_link_status(state, event_tx, "Phone Wi-Fi not connected", false);
                    }
                    other if (1000..=1013).contains(&other) => {
                        debug!(command = other, "Carlinkit connection status command");
                    }
                    other => debug!(command = other, "Carlinkit control command"),
                }
            }
        }
        CMD_BLUETOOTH_CONNECT_START => {
            set_link_status(state, event_tx, "Phone Bluetooth pairing/connection started", false);
        }
        CMD_BLUETOOTH_CONNECTED => {
            set_link_status(state, event_tx, "Phone Bluetooth connected", true);
        }
        CMD_BLUETOOTH_DISCONNECTED => {
            let _ = update_carplay(state, |carplay| {
                carplay.status = "Phone Bluetooth link ended; projection may remain active".into();
            });
        }
        CMD_BLUETOOTH_LISTEN => {
            let _ = update_carplay(state, |carplay| {
                carplay.status = "Dongle waiting for phone".into();
            });
        }
        CMD_JSON_CONTROL => {
            let became_phone = update_json_status(&message.payload, state);
            if became_phone {
                let _ = event_tx.send(CarplayEvent::PhoneConnected);
            }
        }
        CMD_VIDEO => {
            if message.payload.len() < 20 {
                warn!(length = message.payload.len(), "short Carlinkit video packet");
                return actions;
            }
            let width = u32::from_le_bytes(message.payload[0..4].try_into().unwrap());
            let height = u32::from_le_bytes(message.payload[4..8].try_into().unwrap());
            let h264 = &message.payload[20..];

            let newly_streaming = mark_streaming(state, event_tx, "Projection video streaming");
            let _ = update_carplay(state, |carplay| {
                carplay.source_video_width = width;
                carplay.source_video_height = height;
                carplay.video_packets = carplay.video_packets.saturating_add(1);
            });
            if newly_streaming {
                info!(width, height, bytes = h264.len(), "first projection video packet");
            }

            if let Some(video) = video {
                if video.push_h264(h264) == PushResult::NeedKeyframe {
                    actions.request_keyframe = true;
                }
            }
        }
        CMD_AUDIO => {
            // PCM and microphone control are handled by AudioPipeline above.
        }
        CMD_MEDIA_DATA => {
            let subtype = message
                .payload
                .get(0..4)
                .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                .map(u32::from_le_bytes);
            debug!(?subtype, length = message.payload.len(), "Carlinkit media/dashboard metadata");
            // Receiving live metadata is strong evidence that a phone link is
            // established even on firmware that delays/omits Plugged. On the
            // first such indication, re-arm Android Auto as well: some firmware
            // clears AndroidWorkMode while the wireless phone link is changing.
            if set_link_status(
                state,
                event_tx,
                "Phone link active - waiting for projection video",
                true,
            ) {
                actions.rearm_android = true;
            }
        }
        CMD_BLUETOOTH_NAME => {
            let name = String::from_utf8_lossy(&message.payload).trim_matches(char::from(0)).trim().to_string();
            info!(%name, "dongle Bluetooth name");
            let _ = update_carplay(state, |carplay| carplay.dongle_bluetooth_name = name);
        }
        CMD_WIFI_NAME => {
            let name = String::from_utf8_lossy(&message.payload).trim_matches(char::from(0)).trim().to_string();
            info!(%name, "dongle Wi-Fi name");
        }
        CMD_SOFTWARE_VERSION => {
            let version = String::from_utf8_lossy(&message.payload)
                .trim_matches(char::from(0))
                .trim()
                .to_string();
            info!(%version, "Carlinkit firmware version");
            let _ = update_carplay(state, |carplay| carplay.dongle_firmware = version);
        }
        _ => debug!(kind = message.kind, length = message.payload.len(), "unhandled Carlinkit message"),
    }

    actions
}

fn set_link_status(
    state: &Arc<RwLock<RuntimeState>>,
    event_tx: &Sender<CarplayEvent>,
    status: &str,
    connected_hint: bool,
) -> bool {
    let newly_connected = update_carplay(state, |carplay| {
        let newly_connected = connected_hint && !carplay.phone_connected;
        if connected_hint {
            carplay.phone_connected = true;
            if !carplay.streaming {
                carplay.phase = CarplayPhase::Starting;
            }
        }
        if !carplay.streaming {
            carplay.status = status.into();
        }
        newly_connected
    });
    let newly_connected = newly_connected.unwrap_or(false);
    if newly_connected {
        let _ = event_tx.send(CarplayEvent::PhoneConnected);
    }
    newly_connected
}

fn mark_streaming(
    state: &Arc<RwLock<RuntimeState>>,
    event_tx: &Sender<CarplayEvent>,
    status: &str,
) -> bool {
    let (newly_connected, newly_streaming) = update_carplay(state, |carplay| {
        let newly_connected = !carplay.phone_connected;
        let newly_streaming = !carplay.streaming;
        carplay.phone_connected = true;
        carplay.streaming = true;
        carplay.phase = CarplayPhase::Running;
        carplay.status = status.into();
        (newly_connected, newly_streaming)
    })
    .unwrap_or((false, false));
    if newly_connected {
        let _ = event_tx.send(CarplayEvent::PhoneConnected);
    }
    if newly_streaming {
        let _ = event_tx.send(CarplayEvent::StreamingReady);
    }
    newly_streaming
}

fn update_video_counters(state: &Arc<RwLock<RuntimeState>>, video: &VideoPipeline) {
    let _ = update_carplay(state, |carplay| {
        carplay.video_packets = carplay.video_packets.max(video.received_count());
        carplay.video_frames = video.decoded_count();
        carplay.video_dropped = video.dropped_count();
    });
}

// Plugged payload: phoneType u32, optional Wi-Fi transport flag u32.
// Do not infer a wired connection when older firmware omits the flag.
fn phone_connection(payload: &[u8]) -> (&'static str, &'static str) {
    let word = |offset| payload.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
    let kind = word(0);
    let method = match kind { Some(3 | 8) => "CarPlay", Some(5) => "AndroidAuto", _ => "" };
    let transport = match word(4) {
        Some(0) => "Wired", Some(1) => "Wireless",
        None if kind == Some(8) => "Wireless", _ => "",
    };
    (method, transport)
}

fn update_json_status(payload: &[u8], state: &Arc<RwLock<RuntimeState>>) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else {
        return false;
    };

    let method = find_json_string(&value, "MDLinkType");
    let phone = find_json_string(&value, "btName");
    if method.is_none() && phone.is_none() {
        return false;
    }

    let became_phone = update_carplay(state, |carplay| {
        let became_phone = !carplay.phone_connected && method.as_deref().is_some_and(|m| m == "AndroidAuto" || m == "CarPlay");
        if became_phone {
            carplay.phone_connected = true;
            carplay.phase = CarplayPhase::Starting;
            carplay.status = "Phone link negotiated - waiting for video".into();
        }
        if let Some(method) = method {
            carplay.connection_method = method;
        }
        if let Some(phone) = phone {
            carplay.phone_name = phone;
        }
        became_phone
    })
    .unwrap_or(false);
    became_phone
}

fn find_json_string(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(value)) = map.get(key) {
                return Some(value.clone());
            }
            map.values().find_map(|value| find_json_string(value, key))
        }
        Value::Array(values) => values.iter().find_map(|value| find_json_string(value, key)),
        _ => None,
    }
}

fn end_phone_session(
    state: &Arc<RwLock<RuntimeState>>,
    event_tx: &Sender<CarplayEvent>,
    status: &str,
) {
    let was_connected = update_carplay(state, |carplay| {
        let was_connected = carplay.phone_connected || carplay.streaming;
        carplay.phone_connected = false;
        carplay.streaming = false;
        carplay.protocol_phase = Some(0);
        carplay.phase = CarplayPhase::DongleReady;
        carplay.status = status.into();
        carplay.phone_name.clear();
        carplay.connection_method.clear();
        carplay.connection_transport.clear();
        carplay.source_video_width = 0;
        carplay.source_video_height = 0;
        was_connected
    });
    if was_connected.unwrap_or(false) {
        let _ = event_tx.send(CarplayEvent::PhoneDisconnected);
    }
}

fn update_carplay<T>(
    state: &Arc<RwLock<RuntimeState>>,
    update: impl FnOnce(&mut CarplaySnapshot) -> T,
) -> Option<T> {
    state.write().ok().map(|mut state| update(&mut state.carplay))
}

fn bump_heartbeat(state: &Arc<RwLock<RuntimeState>>) -> u64 {
    update_carplay(state, |carplay| {
        carplay.heartbeat_count = carplay.heartbeat_count.saturating_add(1);
        carplay.heartbeat_count
    })
    .unwrap_or(0)
}

fn set_waiting_for_dongle(state: &Arc<RwLock<RuntimeState>>) {
    if let Ok(mut state) = state.write() {
        state.usb = UsbSnapshot::default();
        let heartbeats = state.carplay.heartbeat_count;
        state.carplay = CarplaySnapshot {
            dpi: state.carplay.dpi,
            heartbeat_count: heartbeats,
            ..CarplaySnapshot::default()
        };
    }
}

fn set_dongle_online(state: &Arc<RwLock<RuntimeState>>, heartbeat_ms: u64) {
    let _ = update_carplay(state, |carplay| {
        carplay.phase = CarplayPhase::DongleReady;
        carplay.status = format!("Dongle online - waiting for phone (heartbeat {heartbeat_ms} ms)");
        carplay.phone_connected = false;
        carplay.streaming = false;
        carplay.protocol_phase = None;
    });
}

fn set_error(state: &Arc<RwLock<RuntimeState>>, error: String) {
    if let Ok(mut state) = state.write() {
        state.usb.error = Some(error.clone());
        state.carplay.phase = CarplayPhase::Error;
        state.carplay.status = error;
        state.carplay.phone_connected = false;
        state.carplay.streaming = false;
    }
}

fn sleep_interruptible(stop: &AtomicBool, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis.max(50));
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    #[test]
    fn phone_transport_uses_reported_flag_and_handles_legacy_payloads() {
        assert_eq!(phone_connection(&[3,0,0,0,1,0,0,0]), ("CarPlay", "Wireless"));
        assert_eq!(phone_connection(&[5,0,0,0,0,0,0,0]), ("AndroidAuto", "Wired"));
        assert_eq!(phone_connection(&[5,0,0,0]), ("AndroidAuto", ""));
        assert_eq!(phone_connection(&[8,0,0,0]), ("CarPlay", "Wireless"));
        assert_eq!(phone_connection(&[3,0]), ("", ""));
    }
}
