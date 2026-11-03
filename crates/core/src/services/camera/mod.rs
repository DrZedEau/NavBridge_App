//! The reverse camera: a V4L2 capture device, streamed while the car is in
//! reverse and published beside the projection frame for the UI's reverse
//! page.
//!
//! Two kinds of camera give the same V4L2 device:
//!
//! - a USB (UVC) camera, such as one on the board's own USB-C connector,
//!   giving YUYV; its `/dev/v4l/by-id/` name keeps when it moves to another
//!   port;
//! - an analog (CVBS) camera through an ADV7280-M decoder on the CSI-2 port,
//!   giving UYVY fields of 720x240 (NTSC) or 720x288 (PAL); the kernel's
//!   `adv7180` driver serves it, set up by a device-tree overlay and, on the
//!   CM5's `rp1-cfe`, a `media-ctl` link set at boot.
//!
//! The device is opened, and its format agreed, as soon as it is there, and
//! looked for again every 2 seconds while it is missing; it streams only while
//! the I-Bus reports reverse, so that it starts within a picture or two. Each
//! picture is scaled, through the projection's scaler, to the size the UI
//! asked for and replaces `camera.rgba` (the projection frame's format, beside
//! it); the file is removed when reverse ends, so that the next reverse never
//! shows an old picture. Of an interlaced picture only the top field is shown:
//! it holds the 240 lines the UI has.
//!
//! The core does not start this service yet: `main` is to start it with
//! `[camera]` from core.toml, hand it each I-Bus snapshot's reverse state and
//! the UI's frame size, and the UI's reverse page is to show `camera.rgba`.

// Until the core starts the camera.
#![allow(dead_code)]

mod device;
mod format;
mod picture;
#[cfg(test)]
mod tests;
mod v4l2;

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use device::{fourcc_text, Device};
use format::{colors, field_name, picture_lines};
use picture::Pictures;
use serde::Deserialize;
use tracing::{info, warn};

use crate::services::frames::FrameSize;

/// How long a missing or failed camera waits before it is looked for again.
const REOPEN_INTERVAL: Duration = Duration::from_secs(2);

/// The longest the thread waits for a picture, or idles out of reverse,
/// before it checks whether it should stop.
const TICK: Duration = Duration::from_millis(100);

/// `[camera]` in core.toml.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    pub enabled: bool,
    /// The capture device: a `/dev/v4l/by-id/` name for a USB camera, the
    /// decoder's `/dev/videoN` for an analog one.
    pub device: PathBuf,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: PathBuf::from("/dev/video0"),
        }
    }
}

/// What the camera is doing, for the System page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CameraState {
    /// No camera at the configured path.
    #[default]
    Missing,
    /// Open with its format agreed, waiting for reverse.
    Ready { name: String, format: String },
    /// Streaming to the frame file.
    Showing { name: String, format: String },
    /// There, but it cannot be used: what went wrong.
    Failed(String),
}

pub struct CameraService {
    state: Arc<RwLock<CameraState>>,
    reverse: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CameraService {
    /// Looks after the camera at `config.device`, publishing its pictures at
    /// `frame_path`, at `size`, while in reverse.
    pub fn start(config: CameraConfig, frame_path: PathBuf, size: FrameSize) -> Self {
        let state = Arc::new(RwLock::new(CameraState::Missing));
        let reverse = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (state, reverse, stop) = (state.clone(), reverse.clone(), stop.clone());
            thread::Builder::new()
                .name("camera".into())
                .spawn(move || {
                    run(config, frame_path, size, &state, &reverse, &stop);
                })
                .expect("spawn the camera thread")
        };
        Self {
            state,
            reverse,
            stop,
            worker: Some(worker),
        }
    }

    /// Streams while `on`, from the I-Bus's reverse gear.
    pub fn set_reverse(&self, on: bool) {
        if self.reverse.swap(on, Ordering::Relaxed) != on {
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
        }
    }

    pub fn state(&self) -> CameraState {
        self.state.read().unwrap().clone()
    }
}

impl Drop for CameraService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn run(
    config: CameraConfig,
    frame_path: PathBuf,
    size: FrameSize,
    state: &RwLock<CameraState>,
    reverse: &AtomicBool,
    stop: &AtomicBool,
) {
    let set = |next: CameraState| *state.write().unwrap() = next;
    let mut device: Option<Device> = None;
    let mut pictures = Pictures::default();
    let mut reopen_at = Instant::now();
    // The last failure logged, so that one that repeats is logged once.
    let mut logged: Option<String> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(camera) = device.as_mut() else {
            if Instant::now() < reopen_at {
                thread::park_timeout(TICK);
                continue;
            }
            match Device::open(&config.device) {
                Ok(camera) => {
                    let format = describe(&camera);
                    info!(device = %config.device.display(), name = %camera.name, %format, "reverse camera found");
                    logged = None;
                    set(CameraState::Ready {
                        name: camera.name.clone(),
                        format,
                    });
                    device = Some(camera);
                }
                Err(e) => {
                    let next = if e.kind() == io::ErrorKind::NotFound {
                        CameraState::Missing
                    } else {
                        CameraState::Failed(e.to_string())
                    };
                    let text = e.to_string();
                    if logged.as_ref() != Some(&text) {
                        warn!(device = %config.device.display(), error = %text, "reverse camera unavailable; looking again every 2 seconds");
                        logged = Some(text);
                    }
                    set(next);
                    reopen_at = Instant::now() + REOPEN_INTERVAL;
                }
            }
            continue;
        };
        let (name, format) = (camera.name.clone(), describe(camera));
        let shown = if reverse.load(Ordering::Relaxed) {
            set(CameraState::Showing { name, format });
            camera
                .start()
                .and_then(|()| show_next(camera, &mut pictures, &frame_path, &size))
        } else {
            set(CameraState::Ready { name, format });
            let stopped = camera.stop();
            // Nothing is left for the next reverse to show before its first
            // picture.
            let _ = fs::remove_file(&frame_path);
            thread::park_timeout(TICK);
            stopped
        };
        if let Err(e) = shown {
            warn!(error = %e, "reverse camera lost; looking again every 2 seconds");
            logged = Some(e.to_string());
            device = None;
            let _ = fs::remove_file(&frame_path);
            set(CameraState::Failed(e.to_string()));
            reopen_at = Instant::now() + REOPEN_INTERVAL;
        }
    }
    let _ = fs::remove_file(&frame_path);
}

/// Waits for the camera's next picture and publishes it.
fn show_next(
    camera: &mut Device,
    pictures: &mut Pictures,
    frame_path: &std::path::Path,
    size: &FrameSize,
) -> io::Result<()> {
    let (format, packing) = (camera.format, camera.packing);
    let shown = camera.next(TICK, |data, field| {
        let Some(lines) = picture_lines(&format, field) else {
            return Ok(());
        };
        let rgba = pictures
            .convert(packing, lines, colors(&format), data, size.get())
            .map_err(io::Error::other)?;
        pictures.publish(frame_path, &rgba)
    })?;
    shown.unwrap_or(Ok(()))
}

/// The agreed format, as the log and the System page show it.
fn describe(camera: &Device) -> String {
    let format = &camera.format;
    format!(
        "{}x{} {}, {}",
        format.width,
        format.height,
        fourcc_text(format.pixelformat),
        field_name(format.field)
    )
}
