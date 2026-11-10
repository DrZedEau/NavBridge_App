//! Choices made in Settings that survive a restart, kept together in one
//! file. Each service changes only its own, and the others stay as they were.
use navbridge_common::{AspectRatio, Theme, VideoCodec};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::warn;

pub const PATH: &str = "/var/lib/navbridge/settings.toml";

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Saved {
    pub wireless_android_auto: bool,
    /// The projection's DPI and codec, instead of core.toml's.
    pub dpi: Option<u32>,
    pub video_codec: Option<VideoCodec>,
    /// The codec's output volume and microphone gain, in percent of their range.
    pub volume: Option<u8>,
    pub microphone: Option<u8>,
    /// How much louder than the codec's volume calls play, in dB.
    pub call_volume_db: i8,
    /// The UI's colours.
    pub theme: Theme,
    /// The UI's shape, instead of frontend.toml's width.
    pub aspect_ratio: Option<AspectRatio>,
    /// The one phone called for wireless Android Auto, by its Bluetooth
    /// address; without one, every paired phone is called.
    pub default_phone: Option<String>,
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

/// How long a level stays unchanged before it is kept.
pub const SETTLE: Duration = Duration::from_millis(500);

/// A choice that a held key changes many times a second, such as a volume:
/// it is kept once it has stayed the same for `SETTLE`, in one write for all
/// its steps, and when its service stops.
#[derive(Debug, Default)]
pub struct Unsaved<T>(Option<(T, Instant)>);

impl<T> Unsaved<T> {
    /// The latest choice, due to be kept `SETTLE` from `now`.
    pub fn set(&mut self, value: T, now: Instant) {
        self.0 = Some((value, now + SETTLE));
    }

    /// When the choice is due to be kept, if one waits.
    pub fn due(&self) -> Option<Instant> {
        self.0.as_ref().map(|(_, due)| *due)
    }

    /// The choice to keep, once it has settled.
    pub fn settled(&mut self, now: Instant) -> Option<T> {
        self.due()
            .filter(|due| now >= *due)
            .and_then(|_| self.take())
    }

    /// The choice to keep, settled or not, as when the service stops.
    pub fn take(&mut self) -> Option<T> {
        self.0.take().map(|(value, _)| value)
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
    fn a_level_is_kept_once_it_has_settled() {
        let start = Instant::now();
        let step = Duration::from_millis(40);
        let mut level = Unsaved::default();
        assert_eq!((level.due(), level.settled(start)), (None, None));
        // A held key: each step puts the write off.
        level.set(70u8, start);
        level.set(71, start + step);
        assert_eq!(level.due(), Some(start + step + SETTLE));
        assert_eq!(level.settled(start + SETTLE), None);
        // Settled: the last step is kept, once.
        assert_eq!(level.settled(start + step + SETTLE), Some(71));
        assert_eq!(level.settled(start + step + SETTLE), None);
        // The service stops before it has settled: kept all the same.
        level.set(72, start);
        assert_eq!((level.take(), level.take()), (Some(72), None));
    }

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
        settings.update(|saved| saved.video_codec = Some(VideoCodec::H265));
        settings.update(|saved| saved.volume = Some(70));
        settings.update(|saved| saved.theme = Theme::Blue);
        settings.update(|saved| saved.aspect_ratio = Some(AspectRatio::FourThree));
        settings.update(|saved| saved.call_volume_db = 6);
        settings.update(|saved| saved.default_phone = Some("64:9D:38:E1:B6:24".into()));
        assert_eq!(
            settings.load(),
            Saved {
                wireless_android_auto: true,
                dpi: Some(140),
                video_codec: Some(VideoCodec::H265),
                volume: Some(70),
                microphone: None,
                call_volume_db: 6,
                theme: Theme::Blue,
                aspect_ratio: Some(AspectRatio::FourThree),
                default_phone: Some("64:9D:38:E1:B6:24".into()),
            }
        );
        let file = fs::read_to_string(&path).unwrap();
        assert!(file.contains("theme = \"blue\""));
        assert!(file.contains("video_codec = \"h265\""));
        assert!(file.contains("aspect_ratio = \"4:3\""));
        // A file from before, with wireless mode and the frame rate it no
        // longer keeps, still reads.
        fs::write(&path, "wireless_android_auto = true\nfps = 30\n").unwrap();
        let saved = settings.load();
        assert!(saved.wireless_android_auto);
        assert_eq!(
            (
                saved.dpi,
                saved.video_codec,
                saved.volume,
                saved.call_volume_db
            ),
            (None, None, None, 0)
        );
        assert_eq!(saved.theme, Theme::Amber);
        assert_eq!(saved.aspect_ratio, None);
        assert_eq!(saved.default_phone, None);
        fs::remove_dir_all(dir).unwrap();
    }
}
