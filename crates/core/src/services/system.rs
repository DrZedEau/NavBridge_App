use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use navbridge_common::SystemSnapshot;

use crate::config::AudioConfig;
use crate::services::audio::mixer::{ChosenLevels, Gain};
use crate::settings::Settings;

/// How often the module's figures, the codec and the USB devices are read.
const REFRESH: Duration = Duration::from_secs(1);
/// Where the kernel lists USB devices, and where udev keeps what it looked up
/// for each, such as its names in the hardware database.
const USB_DEVICES: &str = "/sys/bus/usb/devices";
const UDEV_DATA: &str = "/run/udev/data";

pub struct SystemService {
    snapshot: Arc<RwLock<SystemSnapshot>>,
    /// Levels for the codec, from Settings.
    gains: Option<Sender<(Gain, u8)>>,
    thread: Option<JoinHandle<()>>,
}

impl SystemService {
    /// `audio` names the codec whose rates and mixer levels are reported and
    /// set. Levels chosen in Settings are kept in `settings`, and set again
    /// whenever the codec appears, as a power cut can lose ALSA's own copy.
    pub fn start(audio: AudioConfig, settings: Arc<Settings>) -> Self {
        let snapshot = Arc::new(RwLock::new(SystemSnapshot::default()));
        let (gains, requests) = mpsc::channel();
        let snapshot_thread = Arc::clone(&snapshot);

        let thread = thread::spawn(move || {
            let mut previous_cpu = None;
            let mut levels = ChosenLevels::new(audio, settings);
            loop {
                let next = collect(&mut previous_cpu, &mut levels);
                if let Ok(mut guard) = snapshot_thread.write() {
                    *guard = next;
                }
                match requests.recv_timeout(REFRESH) {
                    Ok((gain, percent)) => levels.set(gain, percent),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });

        Self {
            snapshot,
            gains: Some(gains),
            thread: Some(thread),
        }
    }

    pub fn snapshot(&self) -> SystemSnapshot {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Sets the codec's output volume or microphone gain, in percent, and
    /// keeps it; the snapshot shows it at once.
    pub fn set_gain(&self, gain: Gain, percent: u8) {
        if let Some(gains) = &self.gains {
            let _ = gains.send((gain, percent.min(100)));
        }
    }
}

impl Drop for SystemService {
    fn drop(&mut self) {
        // The thread stops at its next wait.
        self.gains.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn collect(previous_cpu: &mut Option<(u64, u64)>, levels: &mut ChosenLevels) -> SystemSnapshot {
    let current = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|raw| cpu_times(&raw));
    let cpu_usage = match (previous_cpu.take(), current) {
        (Some(before), Some(now)) => cpu_percent(before, now)
            .map(|usage| format!("{usage:.1}%"))
            .unwrap_or_else(|| "n/a".into()),
        _ => "n/a".into(),
    };
    *previous_cpu = current;
    SystemSnapshot {
        cpu_usage,
        memory: read_memory(),
        cpu_temp: read_cpu_temp(),
        audio: levels.status(),
        usb_devices: usb_devices(Path::new(USB_DEVICES), Path::new(UDEV_DATA)),
    }
}

/// The USB devices plugged in, hubs left out, in the order of their ports,
/// named as lsusb names them: the maker, then the product, each from udev's
/// hardware database, else as the device gives it; else its IDs.
fn usb_devices(root: &Path, udev: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices: Vec<(Vec<u32>, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let port = entry.file_name().into_string().ok()?;
            // Root hubs are "usb1" and so on; interfaces, "2-1:1.0".
            if port.starts_with("usb") || port.contains(':') {
                return None;
            }
            let read = |attribute: &str| {
                fs::read_to_string(entry.path().join(attribute))
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
            };
            if read("bDeviceClass").as_deref() == Some("09") {
                return None;
            }
            // Lines such as "E:ID_VENDOR_FROM_DATABASE=Texas Instruments",
            // in the file named after the device's numbers ("c189:129").
            let looked_up = read("dev")
                .and_then(|numbers| fs::read_to_string(udev.join(format!("c{numbers}"))).ok())
                .unwrap_or_default();
            let database = |key: &str| {
                looked_up
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("E:")?
                            .strip_prefix(key)?
                            .strip_prefix('=')
                    })
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(String::from)
            };
            let maker = database("ID_VENDOR_FROM_DATABASE").or_else(|| read("manufacturer"));
            let product = database("ID_MODEL_FROM_DATABASE").or_else(|| read("product"));
            let name = match (maker, product) {
                (None, None) => format!("USB {}:{}", read("idVendor")?, read("idProduct")?),
                (maker, product) => [maker, product]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            // "2-1.4": bus 2, port 1, then port 4 of a hub there.
            let order = port
                .split(['-', '.'])
                .map(|n| n.parse().unwrap_or(0))
                .collect();
            Some((order, name))
        })
        .collect();
    devices.sort();
    devices.into_iter().map(|(_, name)| name).collect()
}

// Aggregate across all cores; guest time is already included in user/nice.
fn cpu_times(raw: &str) -> Option<(u64, u64)> {
    let mut fields = raw.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let times = fields
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if times.len() < 4 {
        return None;
    }
    Some((
        times.iter().sum(),
        times[3] + times.get(4).copied().unwrap_or(0),
    ))
}

fn cpu_percent(before: (u64, u64), now: (u64, u64)) -> Option<f64> {
    let total = now.0.checked_sub(before.0)?;
    let idle = now.1.checked_sub(before.1)?;
    if total == 0 || idle > total {
        return None;
    }
    Some(100.0 * (total - idle) as f64 / total as f64)
}

fn read_memory() -> String {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|raw| memory_usage(&raw))
        .unwrap_or_else(|| "n/a".into())
}

fn memory_usage(raw: &str) -> Option<String> {
    let get = |name| {
        raw.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == name)
                .then(|| parts.next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    let total = get("MemTotal:")?;
    let available = get("MemAvailable:")?;
    if total == 0 || available > total {
        return None;
    }
    let used = total - available;
    Some(format!(
        "{:.0}% ({:.1}/{:.1} GiB)",
        100.0 * used as f64 / total as f64,
        used as f64 / 1_048_576.0,
        total as f64 / 1_048_576.0
    ))
}

fn read_cpu_temp() -> String {
    fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli| format!("{:.1} °C", milli / 1000.0))
        .unwrap_or_else(|| "n/a".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_usage_excludes_guest_double_counting_and_counts_iowait_as_idle() {
        assert_eq!(
            cpu_times("cpu 10 5 15 60 10 0 0 0 9 2\ncpu0 1"),
            Some((100, 70))
        );
        assert_eq!(cpu_percent((100, 70), (200, 130)), Some(40.0));
        assert_eq!(cpu_percent((100, 70), (100, 70)), None);
        assert_eq!(cpu_percent((100, 70), (90, 65)), None);
    }
    #[test]
    fn ram_uses_available_memory_and_handles_missing_values() {
        assert_eq!(
            memory_usage("MemTotal: 8388608 kB\nMemAvailable: 6291456 kB"),
            Some("25% (2.0/8.0 GiB)".into())
        );
        assert_eq!(memory_usage("MemTotal: 8388608 kB"), None);
    }
    #[test]
    fn usb_devices_are_named_as_lsusb_does_in_the_order_of_their_ports() {
        let root = std::env::temp_dir().join(format!("navbridge-usb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let udev = root.join("udev");
        fs::create_dir_all(&udev).unwrap();
        // What udev found in its hardware database: both names for the
        // codec, only the maker for the Arduino.
        fs::write(
            udev.join("c189:129"),
            "E:ID_VENDOR=Burr-Brown_from_TI\nE:ID_VENDOR_FROM_DATABASE=Texas Instruments\n\
             E:ID_MODEL_FROM_DATABASE=PCM2912A Audio Codec\n",
        )
        .unwrap();
        fs::write(
            udev.join("c189:385"),
            "E:ID_VENDOR_FROM_DATABASE=Arduino SA\n",
        )
        .unwrap();
        let root = root.join("devices");
        let device = |port: &str, attributes: &[(&str, &str)]| {
            let dir = root.join(port);
            fs::create_dir_all(&dir).unwrap();
            for (name, value) in attributes {
                fs::write(dir.join(name), format!("{value}\n")).unwrap();
            }
        };
        device(
            "usb2",
            &[("bDeviceClass", "09"), ("product", "xHCI Host Controller")],
        );
        device(
            "4-1",
            &[
                ("bDeviceClass", "ef"),
                ("dev", "189:385"),
                ("manufacturer", "Arduino LLC"),
                ("product", "Arduino Micro"),
            ],
        );
        device(
            "2-1",
            &[
                ("bDeviceClass", "00"),
                ("dev", "189:129"),
                ("manufacturer", "Burr-Brown from TI"),
                ("product", "USB audio CODEC"),
            ],
        );
        device("2-1:1.0", &[("bInterfaceClass", "01")]);
        device("1-1", &[("bDeviceClass", "09"), ("product", "USB2.0 Hub")]);
        device("1-1.10", &[("manufacturer", "u-blox AG")]);
        device("1-1.2", &[("idVendor", "18d1"), ("idProduct", "2d00")]);
        assert_eq!(
            usb_devices(&root, &udev),
            [
                "USB 18d1:2d00",
                "u-blox AG",
                "Texas Instruments PCM2912A Audio Codec",
                "Arduino SA Arduino Micro"
            ]
        );
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
        assert!(usb_devices(&root, &udev).is_empty());
    }
}
