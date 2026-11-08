mod actions;
mod config;
mod frame_watch;
mod input;
mod ipc;
mod projection;
mod render;
mod settings_page;
mod state;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::env;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use crate::actions::{
    answer_pairing, core_hello, handle_key_event, keyboard_active, swipe_system, tap_back,
    tap_pair, tap_setting, tap_tile,
};
use crate::config::{KeyboardConfig, UiConfig};
use crate::input::KeyMap;
use crate::ipc::IpcEvent;
use crate::projection::{refresh_projection, touch_projection};
use crate::render::{clock_text, render_state};
use crate::state::{Commands, Screen, UiState, VideoSettings};
use navbridge_common::{
    video_frame_path, ProjectionEvent, ServerEvent, MEDIA_HEIGHT, MEDIA_WIDTH, UI_HEIGHT, UI_WIDTH,
};
use slint::Image;
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .compact()
        .init();

    if let Err(err) = run() {
        eprintln!("navbridge-ui: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (ui_config_path, keyboard_path) = parse_args()?;
    let ui_config = UiConfig::load(&ui_config_path)?;
    let keyboard_config = KeyboardConfig::load(&keyboard_path)?;
    let keymap = Rc::new(KeyMap::new(&keyboard_config)?);

    // Twice as large: the same layout drawn at twice the panel's size. The
    // window reads this as it opens, before any other thread runs.
    if ui_config.scale > 1 {
        env::set_var("SLINT_SCALE_FACTOR", ui_config.scale.to_string());
    }
    let frame_size = (
        ui_config.width * ui_config.scale,
        UI_HEIGHT * ui_config.scale,
    );

    let ui = AppWindow::new()?;
    ui.window().set_fullscreen(ui_config.fullscreen);
    ui.set_double_size(ui_config.scale > 1);

    let frame_path = video_frame_path(&ui_config.socket_path);
    let state = Rc::new(RefCell::new(UiState {
        frame_size,
        frame_path: frame_path.clone(),
        ..UiState::default()
    }));
    render_state(&ui, &state.borrow());

    // The menu's clock, checked every second and redrawn when the minute changes.
    ui.set_clock(clock_text().into());
    let clock = slint::Timer::default();
    {
        let weak = ui.as_weak();
        clock.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let now = clock_text();
                    if ui.get_clock() != now.as_str() {
                        ui.set_clock(now.into());
                    }
                }
            },
        );
    }

    let weak = ui.as_weak();
    let (command_tx, event_rx) = ipc::spawn(
        ui_config.socket_path.clone(),
        ui_config.reconnect_ms,
        move || {
            let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_core_message());
        },
    );

    {
        let state = Rc::clone(&state);
        let keymap = Rc::clone(&keymap);
        let command_tx = command_tx.clone();
        let weak = ui.as_weak();
        ui.on_key_event(move |text, pressed, repeat| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let text = text.to_string();
            let Some(action) = keymap.action_for(&text) else {
                return;
            };

            let mut state = state.borrow_mut();
            if !keyboard_active(&state) {
                return;
            }
            handle_key_event(&mut state, action, pressed, repeat, &command_tx);
            render_state(&ui, &state);
        });
    }

    // Taps, from a touchscreen or the mouse: each changes the state much as
    // its key would, then the window is redrawn.
    {
        let tapped = {
            let state = Rc::clone(&state);
            let commands = command_tx.clone();
            let weak = ui.as_weak();
            Rc::new(move |act: &dyn Fn(&mut UiState, &Commands)| {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let mut state = state.borrow_mut();
                act(&mut state, &commands);
                render_state(&ui, &state);
            })
        };
        let t = Rc::clone(&tapped);
        ui.on_tile_tapped(move |index| {
            t(&|state, commands| tap_tile(state, index as usize, commands))
        });
        let t = Rc::clone(&tapped);
        ui.on_back_tapped(move || t(&|state, commands| tap_back(state, commands)));
        let t = Rc::clone(&tapped);
        ui.on_setting_tapped(move |index, half| {
            t(&|state, commands| tap_setting(state, index as usize, half, commands))
        });
        let t = Rc::clone(&tapped);
        ui.on_pair_tapped(move || t(&|state, commands| tap_pair(state, commands)));
        let t = Rc::clone(&tapped);
        ui.on_system_swiped(move |delta| t(&|state, _| swipe_system(state, delta)));
        ui.on_pairing_answered(move |accept| {
            tapped(&|state, commands| answer_pairing(state, accept, commands))
        });
    }
    // Touches on the projection go to the phone; the window stays as it is.
    {
        let state = Rc::clone(&state);
        let commands = command_tx.clone();
        ui.on_projection_touch(move |kind, x, y| {
            touch_projection(&mut state.borrow_mut(), kind, x, y, &commands);
        });
    }

    // IPC runs on a worker thread, which wakes this handler for each message
    // from the core to apply it on Slint's main/UI thread.
    {
        let state = Rc::clone(&state);
        let commands = command_tx.clone();
        let weak = ui.as_weak();
        ui.on_core_message(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut changed = false;
            let mut state = state.borrow_mut();
            while let Ok(event) = event_rx.try_recv() {
                changed = true;
                match event {
                    IpcEvent::Connected => state.core_connected = true,
                    IpcEvent::Disconnected(reason) => {
                        tracing::debug!(%reason, "core offline");
                        state.disconnect_projection();
                        ui.set_video_active(false);
                        ui.set_projection_frame(Image::default());
                    }
                    IpcEvent::Server(ServerEvent::Hello {
                        ui_width,
                        ui_height,
                        media_width,
                        media_height,
                        media_dpi,
                        aspect_ratio,
                        ..
                    }) => {
                        // Reject an incompatible backend early: whatever panel this
                        // UI has, the core names the same sizes, and media stays
                        // 800x480 with configurable density.
                        if (ui_width, ui_height) != (UI_WIDTH, UI_HEIGHT)
                            || (media_width, media_height) != (MEDIA_WIDTH, MEDIA_HEIGHT)
                            || media_dpi == 0
                        {
                            tracing::warn!("core media geometry mismatch");
                            state.core_connected = false;
                        } else {
                            state.video = Some(VideoSettings {
                                width: media_width,
                                height: media_height,
                            });
                            core_hello(&mut state, aspect_ratio, &commands);
                        }
                    }
                    IpcEvent::Server(ServerEvent::Snapshot { snapshot }) => {
                        let was_connected = state.snapshot.projection.phone_connected;
                        let now_connected = snapshot.projection.phone_connected;
                        state.snapshot = snapshot;

                        // Snapshot transition is a safety net for a UI that was
                        // started/reconnected after the real-time event occurred.
                        if now_connected && !was_connected && !state.projection_hidden_by_phone {
                            state.screen = Screen::Phone;
                        } else if was_connected && !now_connected {
                            state.projection_hidden_by_phone = false;
                            if state.screen == Screen::Phone {
                                state.screen = Screen::Menu;
                            }
                        }
                    }
                    IpcEvent::Server(ServerEvent::ProjectionEvent { event }) => match event {
                        ProjectionEvent::PhoneConnected => {
                            state.projection_hidden_by_phone = false;
                            state.screen = Screen::Phone;
                        }
                        ProjectionEvent::StreamingReady => {
                            if !state.projection_hidden_by_phone {
                                state.screen = Screen::Phone;
                            }
                        }
                        ProjectionEvent::PhoneDisconnected => {
                            state.projection_hidden_by_phone = false;
                            state.screen = Screen::Menu;
                        }
                        ProjectionEvent::RequestHostUi => {
                            state.projection_hidden_by_phone = true;
                            state.screen = Screen::Menu;
                        }
                    },
                    // A car button, as its key would act.
                    IpcEvent::Server(ServerEvent::Input { action, pressed }) => {
                        handle_key_event(&mut state, action.into(), pressed, false, &commands)
                    }
                }
            }
            if changed {
                render_state(&ui, &state);
            }
        });
    }

    // Projection frames are exchanged out-of-band through tmpfs. The USB/backend
    // session remains 800x480; FFmpeg publishes an RGBA view at the panel's exact
    // size for Slint so video never travels through the JSON control socket. Each frame
    // is loaded as soon as the core publishes it; LinuxKMS shows it at the
    // next vblank.
    {
        let state = Rc::clone(&state);
        let weak = ui.as_weak();
        let frame_stats = env::var_os("NAVBRIDGE_FRAME_STATS").is_some();
        let mut sample_start = Instant::now();
        let mut sample_frames = 0u64;
        ui.on_frame_ready(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !refresh_projection(&ui, &state.borrow()) || !frame_stats {
                return;
            }
            sample_frames += 1;
            let seconds = sample_start.elapsed().as_secs_f64();
            if seconds >= 5.0 {
                tracing::info!(
                    fps = sample_frames as f64 / seconds,
                    frames = sample_frames,
                    seconds,
                    "projection UI frame updates"
                );
                sample_start = Instant::now();
                sample_frames = 0;
            }
        });
    }
    let weak = ui.as_weak();
    frame_watch::spawn(&frame_path, move || {
        let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_frame_ready());
    })?;

    ui.run()?;
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let mut ui = PathBuf::from("config/frontend.toml");
    let mut keyboard = PathBuf::from("config/keyboard.toml");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => ui = PathBuf::from(args.next().ok_or("--config needs a path")?),
            "--keyboard" => keyboard = PathBuf::from(args.next().ok_or("--keyboard needs a path")?),
            "-h" | "--help" => {
                println!(
                    "NavBridge UI\n\nUsage: navbridge-ui [--config PATH] [--keyboard PATH]\n\nUI: 400x240, or 320x240 by Settings' Aspect ratio or frontend.toml's width; media session: 800x480; DPI configured in core.toml"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok((ui, keyboard))
}
