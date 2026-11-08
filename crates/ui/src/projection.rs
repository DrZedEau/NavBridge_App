//! Android Auto's projection: touches passed on to the phone, and the frames
//! the core publishes, read and shown.

use std::io::{self, Read};
use std::time::Instant;

use crate::actions::release_navigation;
use crate::state::{Commands, UiState};
use crate::AppWindow;
use navbridge_common::{
    ClientCommand, TouchKind, MEDIA_HEIGHT, UI_HEIGHT, VIDEO_FRAME_HEADER_SIZE, VIDEO_FRAME_MAGIC,
};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Moves on the projection closer together than this are skipped; the last
/// point still goes out with the release.
pub(crate) const TOUCH_MOVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(15);

/// A finger or the mouse on the projection, at a point of the window's layout
/// (whatever its scale), passed to the phone in Android Auto's pixels: the
/// 800x480 it declares as its touchscreen, or 640x480 on a 4:3 panel.
pub(crate) fn touch_projection(
    state: &mut UiState,
    kind: i32,
    x: f32,
    y: f32,
    commands: &Commands,
) {
    if !(state.show_projection() && state.snapshot.projection.phone_connected) {
        return;
    }
    let scale = |at: f32, ui: u32, media: u32| -> u32 {
        (at.max(0.0) * media as f32 / ui as f32)
            .round()
            .min((media - 1) as f32) as u32
    };
    let ui_width = state.ui_width();
    let point = (
        scale(x, ui_width, ui_width * MEDIA_HEIGHT / UI_HEIGHT),
        scale(y, UI_HEIGHT, MEDIA_HEIGHT),
    );
    let kind = match kind {
        0 => TouchKind::Down,
        1 => TouchKind::Move,
        _ => TouchKind::Up,
    };
    match kind {
        TouchKind::Down => {
            release_navigation(state, commands);
            state.touch = Some(point);
            state.touch_moved_at = Some(Instant::now());
        }
        TouchKind::Move => {
            let recent = state
                .touch_moved_at
                .is_some_and(|at| at.elapsed() < TOUCH_MOVE_INTERVAL);
            if state.touch.is_none() || state.touch == Some(point) || recent {
                return;
            }
            state.touch = Some(point);
            state.touch_moved_at = Some(Instant::now());
        }
        TouchKind::Up => {
            if state.touch.take().is_none() {
                return;
            }
        }
    }
    let _ = commands.send(ClientCommand::Touch {
        kind,
        x: point.0,
        y: point.1,
    });
}

/// Shows the newest projection frame if the UI has not shown it yet. Returns
/// whether a new frame went on screen.
pub(crate) fn refresh_projection(ui: &AppWindow, state: &UiState) -> bool {
    if !state.show_projection() {
        ui.set_video_active(false);
        return false;
    }
    let previous = state.video_sequence.get();
    // Open once: atomic replacement cannot mix a header from one frame
    // with pixels from another. Unchanged frames cost only 20 bytes.
    let result = std::fs::File::open(&state.frame_path)
        .and_then(|mut file| read_projection(&mut file, previous, state.frame_size));
    match result {
        Ok(Some((sequence, pixels))) => {
            state.video_sequence.set(Some(sequence));
            ui.set_projection_frame(Image::from_rgba8(pixels));
            ui.set_video_active(true);
            true
        }
        Ok(None) => false,
        Err(_) => {
            ui.set_video_active(false);
            ui.set_projection_frame(Image::default());
            false
        }
    }
}

/// A frame's sequence number, if it is a projection frame of `size`.
pub(crate) fn read_frame_header(reader: &mut impl Read, size: (u32, u32)) -> io::Result<u64> {
    let mut header = [0u8; VIDEO_FRAME_HEADER_SIZE];
    reader.read_exact(&mut header)?;
    let width = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if header[..4] != VIDEO_FRAME_MAGIC || (width, height) != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid video header",
        ));
    }
    Ok(u64::from_le_bytes(header[12..20].try_into().unwrap()))
}

pub(crate) fn read_projection(
    reader: &mut impl Read,
    previous: Option<u64>,
    size: (u32, u32),
) -> io::Result<Option<(u64, SharedPixelBuffer<Rgba8Pixel>)>> {
    let sequence = read_frame_header(reader, size)?;
    if previous == Some(sequence) {
        return Ok(None);
    }
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(size.0, size.1);
    reader.read_exact(pixels.make_mut_bytes())?;
    if reader.read(&mut [0u8; 1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized video frame",
        ));
    }
    Ok(Some((sequence, pixels)))
}
