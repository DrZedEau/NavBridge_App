//! Wakes the UI as soon as navbridge-core publishes a projection frame.
//!
//! The core renames each finished frame over the previous one, so inotify on
//! the directory reports every new frame the moment it is complete.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::thread;
use std::time::Duration;

use tracing::warn;

/// Only used when inotify is unavailable.
const POLL_INTERVAL: Duration = Duration::from_millis(8);
/// `struct inotify_event` before its name: wd, mask, cookie and name length.
const EVENT_HEADER: usize = 16;

/// Calls `changed` from a background thread whenever the file at `path` is
/// replaced or removed, or every 8 ms when inotify is unavailable.
pub fn spawn(path: &Path, changed: impl Fn() + Send + 'static) -> io::Result<()> {
    // Watch before returning, so no frame published after this call is missed.
    let watch = Watch::new(path);
    thread::Builder::new()
        .name("frame-watch".into())
        .spawn(move || {
            let err = match watch {
                Ok(watch) => watch.run(&changed),
                Err(err) => err,
            };
            warn!(%err, "no frame notifications, polling every 8 ms instead");
            loop {
                changed();
                thread::sleep(POLL_INTERVAL);
            }
        })?;
    Ok(())
}

struct Watch {
    events: File,
    name: Vec<u8>,
}

impl Watch {
    #[cfg(target_os = "linux")]
    fn new(path: &Path) -> io::Result<Self> {
        use std::ffi::CString;
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;

        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame path has no file name",
            ));
        };
        let dir = CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: returns a new descriptor, owned (and closed) by `events`.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let events = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        // New frames are renamed into place; the file is removed when the
        // decoder resets or the core starts.
        let mask = libc::IN_MOVED_TO | libc::IN_DELETE;
        // SAFETY: `dir` is a NUL-terminated path that outlives the call.
        if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            events,
            name: name.as_bytes().to_vec(),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn new(_: &Path) -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Blocks while the watch works and returns the error that ended it.
    fn run(mut self, changed: &impl Fn()) -> io::Error {
        let mut buffer = [0u8; 4096];
        loop {
            match self.events.read(&mut buffer) {
                Ok(0) => return io::ErrorKind::UnexpectedEof.into(),
                Ok(read) => {
                    if mentions(&buffer[..read], &self.name) {
                        changed();
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return err,
            }
        }
    }
}

/// Whether a read of inotify events names `name`, or reports that the queue
/// overflowed (an event without a watch), which may have dropped one.
fn mentions(mut events: &[u8], name: &[u8]) -> bool {
    while let Some(header) = events.get(..EVENT_HEADER) {
        let watch = i32::from_ne_bytes(header[..4].try_into().unwrap());
        let length = u32::from_ne_bytes(header[12..].try_into().unwrap()) as usize;
        let Some(padded) = events.get(EVENT_HEADER..EVENT_HEADER + length) else {
            break;
        };
        // Names are NUL-terminated and padded to keep the next event aligned.
        if watch == -1 || padded.split(|&b| b == 0).next() == Some(name) {
            return true;
        }
        events = &events[EVENT_HEADER + length..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    /// One inotify event as the kernel lays it out.
    fn event(watch: i32, name: &[u8]) -> Vec<u8> {
        let padded = if name.is_empty() {
            0
        } else {
            (name.len() + 1).next_multiple_of(16)
        };
        let mut event = [
            watch.to_ne_bytes(),
            0x80u32.to_ne_bytes(), // IN_MOVED_TO
            0u32.to_ne_bytes(),
            (padded as u32).to_ne_bytes(),
        ]
        .concat();
        event.extend_from_slice(name);
        event.resize(EVENT_HEADER + padded, 0);
        event
    }

    #[test]
    fn only_the_frame_or_a_lost_event_wakes_the_ui() {
        let frame = b"navbridge-video.rgba";
        let temporary = event(1, b"navbridge-video.tmp.42");
        assert!(!mentions(&temporary, frame));
        assert!(!mentions(&event(1, b"navbridge-video.rgba.old"), frame));
        assert!(mentions(&[temporary, event(1, frame)].concat(), frame));
        assert!(mentions(&event(-1, b""), frame), "overflowed queue");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_frame_renamed_into_place_or_removed_is_reported() {
        let dir =
            std::env::temp_dir().join(format!("navbridge-frame-watch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let frame = dir.join("frame.rgba");
        let watch = Watch::new(&frame).unwrap();
        let (tx, woken) = mpsc::channel();
        thread::spawn(move || {
            watch.run(&|| {
                let _ = tx.send(());
            })
        });
        let wait = || woken.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..2 {
            // As the core does: write the frame aside, then rename it over.
            let temporary = dir.join("frame.tmp");
            fs::write(&temporary, b"pixels").unwrap();
            fs::rename(&temporary, &frame).unwrap();
            wait();
        }
        fs::remove_file(&frame).unwrap();
        wait();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unwatchable_frame_is_polled_instead() {
        let (tx, woken) = mpsc::channel();
        spawn(Path::new("/nonexistent-navbridge/frame.rgba"), move || {
            let _ = tx.send(());
        })
        .unwrap();
        for _ in 0..3 {
            woken.recv_timeout(Duration::from_secs(1)).unwrap();
        }
    }
}
