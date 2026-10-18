//! Choices made in Settings that survive a restart, kept together in one
//! file. Each service changes only its own, and the others stay as they were.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tracing::warn;

pub const PATH: &str = "/var/lib/navbridge/settings.toml";

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Saved {
    pub wireless_android_auto: bool,
    /// The projection's DPI, instead of core.toml's.
    pub dpi: Option<u32>,
    /// The codec's output volume and microphone gain, in percent of their range.
    pub volume: Option<u8>,
    pub microphone: Option<u8>,
}

pub struct Settings {
    path: PathBuf,
    /// One change at a time, so that none is lost.
    lock: Mutex<()>,
}

impl Settings {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// What is saved; without a readable file, the defaults.
    pub fn load(&self) -> Saved {
        let _one = self.lock.lock().unwrap();
        read(&self.path)
    }

    /// Changes what is saved, writing the whole file again.
    pub fn update(&self, change: impl FnOnce(&mut Saved)) {
        let _one = self.lock.lock().unwrap();
        let mut saved = read(&self.path);
        change(&mut saved);
        if let Err(e) = write(&self.path, &saved) {
            warn!(%e, path = %self.path.display(), "could not keep the settings");
        }
    }
}

fn read(path: &Path) -> Saved {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Written beside, then renamed, so that a power cut leaves the old file or
/// the new one.
fn write(path: &Path, saved: &Saved) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    let partial = path.with_extension("tmp");
    fs::write(&partial, toml::to_string(saved).map_err(io::Error::other)?)?;
    fs::rename(partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_change_keeps_the_others() {
        let dir =
            std::env::temp_dir().join(format!("navbridge-settings-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("state/settings.toml");
        let settings = Settings::new(&path);
        assert_eq!(settings.load(), Saved::default());
        settings.update(|saved| saved.wireless_android_auto = true);
        settings.update(|saved| saved.dpi = Some(140));
        settings.update(|saved| saved.volume = Some(70));
        assert_eq!(
            settings.load(),
            Saved {
                wireless_android_auto: true,
                dpi: Some(140),
                volume: Some(70),
                microphone: None,
            }
        );
        // A file from before, with wireless mode only, still reads.
        fs::write(&path, "wireless_android_auto = true\n").unwrap();
        let saved = settings.load();
        assert!(saved.wireless_android_auto);
        assert_eq!((saved.dpi, saved.volume), (None, None));
        fs::remove_dir_all(dir).unwrap();
    }
}
