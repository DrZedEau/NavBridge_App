//! Hands-free calls. bluez-alsa takes a phone's calls over Bluetooth as a
//! hands-free device (packaging/bluealsa-navbridge.conf); while a call's audio
//! runs, the caller plays on the car's speakers and the car's microphone goes
//! to the phone. bluez-alsa is asked four times a second whether a call's
//! audio runs, as its D-Bus service only announces its devices, not calls.
use crate::config::AudioConfig;
use crate::services::audio::{open_buffered_pcm, AudioFormat};
use alsa::Direction;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{info, warn};
use zbus::{
    blocking::{fdo::ObjectManagerProxy, Connection},
    zvariant::{OwnedObjectPath, OwnedValue},
};

/// bluez-alsa's name on the system bus, and where it lists its devices.
const SERVICE: &str = "org.bluealsa";
const DEVICES: &str = "/org/bluealsa";
const PCM_INTERFACE: &str = "org.bluealsa.PCM1";
/// How often bluez-alsa is asked about calls; a call's audio starts at most
/// this late.
const POLL: Duration = Duration::from_millis(250);
/// How often while bluez-alsa is not running.
const ABSENT_POLL: Duration = Duration::from_secs(2);
/// How long a call's audio thread waits for samples before checking whether
/// the call is over.
const WAIT_MS: u32 = 100;
/// Bluetooth delivers a call's audio unevenly: each way buffers this much,
/// and plays once START of it is queued, so a late block does not cut it.
const BUFFER: Duration = Duration::from_millis(240);
const START: Duration = Duration::from_millis(80);

/// A call's audio, as bluez-alsa runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    /// The phone's Bluetooth address, as "64:9D:38:E1:B6:24".
    phone: String,
    rate: u32,
    codec: String,
}

pub struct CallService {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CallService {
    /// Calls play on `audio`'s devices: the caller on its playback device, and
    /// its capture device to the caller.
    pub fn start(audio: AudioConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::Builder::new()
            .name("calls".into())
            .spawn(move || run(&audio, &stopped))
            .expect("start the calls thread");
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for CallService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run(audio: &AudioConfig, stop: &AtomicBool) {
    let mut connection = None;
    // Whether bluez-alsa answered last time, so that only changes are logged.
    let mut present = None;
    let mut active: Option<(Call, Relay)> = None;
    while !stop.load(Ordering::Relaxed) {
        if connection.is_none() {
            connection = Connection::system()
                .inspect_err(|e| warn!(%e, "no system D-Bus for calls"))
                .ok();
        }
        let answer = match &connection {
            Some(connection) => running_call(connection),
            None => Err(zbus::Error::Failure("no system D-Bus".into())),
        };
        let now_present = answer.is_ok();
        if present.replace(now_present) != Some(now_present) {
            if now_present {
                info!("bluez-alsa runs: phones' calls can play in the car");
            } else if let Err(e) = &answer {
                info!(%e, "bluez-alsa cannot be asked about calls: they stay on the phone");
            }
        }
        let call = answer.unwrap_or(None);
        if active.as_ref().map(|(call, _)| call) != call.as_ref() {
            if let Some((ended, relay)) = active.take() {
                drop(relay);
                info!(phone = %ended.phone, "the call's audio ended");
            }
            if let Some(call) = call {
                info!(
                    phone = %call.phone,
                    rate = call.rate,
                    codec = %call.codec,
                    "a call's audio plays in the car"
                );
                let relay = Relay::start(audio, &call);
                active = Some((call, relay));
            }
        }
        let wait = if now_present { POLL } else { ABSENT_POLL };
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The call whose audio bluez-alsa runs, if any; an error while bluez-alsa
/// cannot be asked.
fn running_call(connection: &Connection) -> zbus::Result<Option<Call>> {
    let objects = ObjectManagerProxy::builder(connection)
        .destination(SERVICE)?
        .path(DEVICES)?
        .build()?
        .get_managed_objects()?;
    Ok(objects.into_values().find_map(|interfaces| {
        let (_, pcm) = interfaces
            .into_iter()
            .find(|(name, _)| name.as_str() == PCM_INTERFACE)?;
        call(&pcm)
    }))
}

/// A hands-free device's source while it runs: the caller's side of a call.
fn call(pcm: &HashMap<String, OwnedValue>) -> Option<Call> {
    let text = |name: &str| String::try_from(pcm.get(name)?.try_clone().ok()?).ok();
    let transport = text("Transport")?;
    if !transport.starts_with("HFP") && !transport.starts_with("HSP") {
        return None;
    }
    if text("Mode")? != "source" || !bool::try_from(pcm.get("Running")?).ok()? {
        return None;
    }
    let device = OwnedObjectPath::try_from(pcm.get("Device")?.try_clone().ok()?).ok()?;
    Some(Call {
        phone: phone_address(device.as_str())?,
        rate: u32::try_from(pcm.get("Sampling")?).ok()?,
        codec: text("Codec").unwrap_or_default(),
    })
}

/// "64:9D:38:E1:B6:24" from BlueZ's "/org/bluez/hci0/dev_64_9D_38_E1_B6_24".
fn phone_address(device: &str) -> Option<String> {
    let address = device.rsplit_once("/dev_")?.1.replace('_', ":");
    (address.len() == 17).then_some(address)
}

/// A call's audio, each way on its own thread, until dropped.
struct Relay {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Relay {
    fn start(audio: &AudioConfig, call: &Call) -> Self {
        let sco = format!("bluealsa:DEV={},PROFILE=sco", call.phone);
        let format = AudioFormat {
            rate: call.rate,
            channels: 1,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let ways = [
            // The caller, to the car's speakers.
            ("call-downlink", sco.clone(), audio.playback_device.clone()),
            // The car's microphone, to the caller.
            ("call-uplink", audio.capture_device.clone(), sco),
        ];
        let threads = ways
            .into_iter()
            .filter_map(|(name, from, to)| {
                let stop = stop.clone();
                thread::Builder::new()
                    .name(name.into())
                    .spawn(move || match relay(&from, &to, format, &stop) {
                        Ok(gaps) => info!(
                            from,
                            to,
                            late = gaps.late,
                            overflowed = gaps.overflowed,
                            "call audio stopped"
                        ),
                        Err(e) => warn!(%e, from, to, "call audio stopped"),
                    })
                    .inspect_err(|e| warn!(%e, name, "could not start call audio"))
                    .ok()
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// How often a call's audio ran dry or overflowed: each is a gap heard.
#[derive(Default)]
struct Gaps {
    /// The receiving device ran out of samples.
    late: u32,
    /// The sending device had more than could be taken in time.
    overflowed: u32,
}

/// Copies 20 ms blocks from one device to the other until `stop`, waking at
/// least every WAIT_MS to check it.
fn relay(
    from: &str,
    to: &str,
    format: AudioFormat,
    stop: &AtomicBool,
) -> Result<Gaps, alsa::Error> {
    let input = open_buffered_pcm(from, Direction::Capture, format, BUFFER, START)?;
    let output = open_buffered_pcm(to, Direction::Playback, format, BUFFER, START)?;
    input.start()?;
    let mut gaps = Gaps::default();
    let (reader, writer) = (input.io_i16()?, output.io_i16()?);
    let mut block = vec![0i16; (format.rate / 50) as usize];
    while !stop.load(Ordering::Relaxed) {
        if !input.wait(Some(WAIT_MS))? {
            continue;
        }
        let read = match reader.readi(&mut block) {
            Ok(read) => read,
            Err(e) => {
                gaps.overflowed += 1;
                input.try_recover(e, true)?;
                continue;
            }
        };
        // The playback device does not block: when it has no room, the block
        // waits for some, unless the call ends first.
        let mut written = 0;
        while written < read && !stop.load(Ordering::Relaxed) {
            match writer.writei(&block[written..read]) {
                Ok(n) => written += n,
                Err(e) if e.errno() == libc::EAGAIN => {
                    output.wait(Some(WAIT_MS))?;
                }
                Err(e) => {
                    gaps.late += 1;
                    output.try_recover(e, true)?;
                }
            }
        }
    }
    Ok(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, Value};

    fn pcm(transport: &str, mode: &str, running: bool) -> HashMap<String, OwnedValue> {
        let owned = |value: Value| OwnedValue::try_from(value).unwrap();
        HashMap::from([
            ("Transport".into(), owned(Value::from(transport))),
            ("Mode".into(), owned(Value::from(mode))),
            ("Running".into(), owned(Value::from(running))),
            ("Sampling".into(), owned(Value::from(16000u32))),
            ("Codec".into(), owned(Value::from("mSBC"))),
            (
                "Device".into(),
                owned(Value::from(
                    ObjectPath::try_from("/org/bluez/hci0/dev_64_9D_38_E1_B6_24").unwrap(),
                )),
            ),
        ])
    }

    #[test]
    fn a_call_is_the_running_source_of_a_hands_free_device() {
        assert_eq!(
            call(&pcm("HFP-HF", "source", true)),
            Some(Call {
                phone: "64:9D:38:E1:B6:24".into(),
                rate: 16000,
                codec: "mSBC".into(),
            })
        );
        // Connected but between calls; the other direction; music.
        assert_eq!(call(&pcm("HFP-HF", "source", false)), None);
        assert_eq!(call(&pcm("HFP-HF", "sink", true)), None);
        assert_eq!(call(&pcm("A2DP-sink", "source", true)), None);
    }

    #[test]
    fn bluez_device_paths_give_the_phones_address() {
        assert_eq!(
            phone_address("/org/bluez/hci0/dev_64_9D_38_E1_B6_24").as_deref(),
            Some("64:9D:38:E1:B6:24")
        );
        assert_eq!(phone_address("/org/bluez/hci0"), None);
        assert_eq!(phone_address("/org/bluez/hci0/dev_64_9D"), None);
    }
}
