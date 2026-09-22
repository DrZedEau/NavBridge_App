use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use navbridge_common::SystemSnapshot;

pub struct SystemService {
    snapshot: Arc<RwLock<SystemSnapshot>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SystemService {
    pub fn start() -> Self {
        let snapshot = Arc::new(RwLock::new(SystemSnapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot_thread = Arc::clone(&snapshot);
        let stop_thread = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            let mut previous_cpu = None;
            while !stop_thread.load(Ordering::Relaxed) {
                let next = collect(&mut previous_cpu);
                if let Ok(mut guard) = snapshot_thread.write() {
                    *guard = next;
                }
                for _ in 0..10 {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });

        Self {
            snapshot,
            stop,
            thread: Some(thread),
        }
    }

    pub fn snapshot(&self) -> SystemSnapshot {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for SystemService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn collect(previous_cpu: &mut Option<(u64, u64)>) -> SystemSnapshot {
    let current = fs::read_to_string("/proc/stat").ok().and_then(|raw| cpu_times(&raw));
    let cpu_usage = match (previous_cpu.take(), current) {
        (Some(before), Some(now)) => cpu_percent(before, now)
            .map(|usage| format!("{usage:.1}%")).unwrap_or_else(|| "n/a".into()),
        _ => "n/a".into(),
    };
    *previous_cpu = current;
    SystemSnapshot { cpu_usage, memory: read_memory(), cpu_temp: read_cpu_temp(), audio_output_rate: read_audio_output_rate() }
}

// Aggregate across all cores; guest time is already included in user/nice.
fn cpu_times(raw: &str) -> Option<(u64, u64)> {
    let mut fields = raw.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" { return None; }
    let times = fields.take(8).map(str::parse::<u64>).collect::<Result<Vec<_>, _>>().ok()?;
    if times.len() < 4 { return None; }
    Some((times.iter().sum(), times[3] + times.get(4).copied().unwrap_or(0)))
}

fn cpu_percent(before: (u64, u64), now: (u64, u64)) -> Option<f64> {
    let total = now.0.checked_sub(before.0)?;
    let idle = now.1.checked_sub(before.1)?;
    if total == 0 || idle > total { return None; }
    Some(100.0 * (total - idle) as f64 / total as f64)
}

fn read_memory() -> String {
    fs::read_to_string("/proc/meminfo").ok()
        .and_then(|raw| memory_usage(&raw)).unwrap_or_else(|| "n/a".into())
}

fn memory_usage(raw: &str) -> Option<String> {
    let get = |name| raw.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == name).then(|| parts.next()?.parse::<u64>().ok()).flatten()
    });
    let total = get("MemTotal:")?;
    let available = get("MemAvailable:")?;
    if total == 0 || available > total { return None; }
    let used = total - available;
    Some(format!("{:.0}% ({:.1}/{:.1} GiB)", 100.0 * used as f64 / total as f64,
        used as f64 / 1_048_576.0, total as f64 / 1_048_576.0))
}

// Read the USB codec's hardware rate, after ALSA plug/dmix conversion.
fn read_audio_output_rate() -> String {
    fs::read_to_string("/proc/asound/CODEC/pcm0p/sub0/hw_params")
        .map(|raw| audio_output_rate(&raw)).unwrap_or_else(|_| "Unavailable".into())
}

fn audio_output_rate(raw: &str) -> String {
    if raw.trim() == "closed" { return "Idle".into(); }
    raw.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next()? != "rate:" { return None; }
        let rate = words.next()?.parse::<u32>().ok()?;
        (rate > 0).then(|| format!("{rate} Hz"))
    }).unwrap_or_else(|| "Unknown".into())
}

fn read_cpu_temp() -> String {
    fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok().and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli| format!("{:.1} °C", milli / 1000.0))
        .unwrap_or_else(|| "n/a".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_audio_rate_handles_active_closed_and_missing_rate() {
        assert_eq!(audio_output_rate("access: MMAP_INTERLEAVED\nrate: 48000 (48000/1)\n"), "48000 Hz");
        assert_eq!(audio_output_rate("closed\n"), "Idle");
        assert_eq!(audio_output_rate("channels: 2\n"), "Unknown");
    }
    #[test]
    fn cpu_usage_excludes_guest_double_counting_and_counts_iowait_as_idle() {
        assert_eq!(cpu_times("cpu 10 5 15 60 10 0 0 0 9 2\ncpu0 1"), Some((100,70)));
        assert_eq!(cpu_percent((100,70), (200,130)), Some(40.0));
        assert_eq!(cpu_percent((100,70), (100,70)), None);
        assert_eq!(cpu_percent((100,70), (90,65)), None);
    }
    #[test]
    fn ram_uses_available_memory_and_handles_missing_values() {
        assert_eq!(memory_usage("MemTotal: 8388608 kB\nMemAvailable: 6291456 kB"), Some("25% (2.0/8.0 GiB)".into()));
        assert_eq!(memory_usage("MemTotal: 8388608 kB"), None);
    }
}
