// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
//
//! wlRIX compositor
//!
//! A minimal Wayland compositor built on Smithay: opens a nested window under an
//! existing Wayland/X11 session, renders client windows with the GLES renderer, and
//! speaks xdg-shell.
#![allow(irrefutable_let_patterns)]

mod backend;
mod grabs;
mod handlers;
mod input;
mod state;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};
use tracing::{info, warn};

pub use state::Wlrix;

pub struct CalloopData {
    state: Wlrix,
    display_handle: DisplayHandle,
    /// udev/DRM backend state; `None` under the winit backend.
    udev: Option<backend::udev::UdevState>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;

    let display: Display<Wlrix> = Display::new()?;
    let display_handle = display.handle();
    let state = Wlrix::new(&mut event_loop, display);

    let mut data = CalloopData {
        state,
        display_handle,
        udev: None,
    };

    // A backend either drives the event loop (winit; later the udev render loop) or
    // is a one-shot that has already finished (the current udev discovery checkpoint).
    if !crate::backend::init(&mut event_loop, &mut data)? {
        return Ok(());
    }

    // Point clients we spawn at our socket. This must happen *after* the backend is
    // up, because the winit backend needs the host's WAYLAND_DISPLAY to nest into.
    // SAFETY: single-threaded startup, before any client or thread reads the env.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &data.state.socket_name) };

    info!(
        socket = %data.state.socket_name.to_string_lossy(),
        "wlRIX compositor up. Point clients at WAYLAND_DISPLAY to connect."
    );

    // Optionally, auto-spawn a client: `wlrix-compositor -c <command>`.
    let mut args = std::env::args().skip(1);
    if let (Some("-c") | Some("--command"), Some(command)) = (args.next().as_deref(), args.next()) {
        match std::process::Command::new(&command).spawn() {
            Ok(child) => info!(%command, pid = child.id(), "spawned client"),
            Err(err) => warn!(%command, ?err, "failed to spawn client"),
        }
    }

    event_loop.run(None, &mut data, move |_| {
        // wlRIX is running.
    })?;

    Ok(())
}
