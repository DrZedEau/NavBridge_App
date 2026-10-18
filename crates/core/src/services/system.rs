use std::fs;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use navbridge_common::SystemSnapshot;
use tracing::{info, warn};

use crate::config::AudioConfig;
use crate::services::codec::{self, Gain};
use crate::settings::Settings;

/// How often the module's figures and the codec are read.
const REFRESH: Duration = Duration::from_secs(1);

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
            let mut codec_present = false;
            loop {
                let mut next = collect(&mut previous_cpu, &audio);
                let present =
                    next.audio.volume_percent.is_some() || next.audio.microphone_percent.is_some();
                if present && !codec_present {
                    let saved = settings.load();
                    let levels = [
                        (Gain::Speaker, saved.volume),
                        (Gain::Microphone, saved.microphone),
                    ];
                    for (gain, percent) in levels {
                        if let Some(percent) = percent {
                            apply(&audio, gain, percent);
                        }
                    }
                    next.audio = codec::status(&audio);
                }
                codec_present = present;
                if let Ok(mut guard) = snapshot_thread.write() {
                    *guard = next;
                }
                match requests.recv_timeout(REFRESH) {
                    Ok((gain, percent)) => {
                        apply(&audio, gain, percent);
                        settings.update(|saved| match gain {
                            Gain::Speaker => saved.volume = Some(percent),
                            Gain::Microphone => saved.microphone = Some(percent),
                        });
                    }
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

fn apply(audio: &AudioConfig, gain: Gain, percent: u8) {
    match codec::set(audio, gain, percent) {
        Ok(()) => info!(?gain, percent, "codec level set"),
        Err(e) => warn!(%e, ?gain, percent, "could not set the codec level"),
    }
}

fn collect(previous_cpu: &mut Option<(u64, u64)>, audio: &AudioConfig) -> SystemSnapshot {
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
        audio: codec::status(audio),
    }
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
}
