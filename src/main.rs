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
mod cursor;
mod focus;
mod grabs;
mod handlers;
mod handshake;
mod idle;
mod input;
mod logging;
mod output_management;
mod palette;
mod placement;
mod render;
mod screencopy;
mod session_lock;
mod state;
mod vrr;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{info, warn};

pub use state::Wlrix;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Logs to stderr and to a file. stderr leaves stdout free to carry the session
    // handshake without the two interleaving; the file is the only one readable after
    // a TTY session, where the compositor covers the console it was writing to.
    let log_path = logging::init();

    let mut event_loop: EventLoop<Wlrix> = EventLoop::try_new()?;

    let display: Display<Wlrix> = Display::new()?;
    let mut state = Wlrix::new(&mut event_loop, display);

    // A backend either drives the event loop (winit; later the udev render loop) or
    // is a one-shot that has already finished (the current udev discovery checkpoint).
    if !crate::backend::init(&mut event_loop, &mut state)? {
        return Ok(());
    }

    // Point clients we spawn at our socket. This must happen *after* the backend is
    // up, because the winit backend needs the host's WAYLAND_DISPLAY to nest into.
    // SAFETY: single-threaded startup, before any client or thread reads the env.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };

    // X11 applications, via XWayland. Started after the backend so it inherits a
    // working environment.
    state.start_xwayland();

    if let Some(path) = &log_path {
        info!(path = %path.display(), "logging to file");
    }
    info!(
        socket = %state.socket_name.to_string_lossy(),
        "wlRIX compositor up. Point clients at WAYLAND_DISPLAY to connect."
    );
    handshake::announce("WAYLAND_DISPLAY", &state.socket_name.to_string_lossy());

    // Optionally auto-spawn a client: `wlrix-compositor -c <command>`.
    let mut args = std::env::args().skip(1);
    if let (Some("-c") | Some("--command"), Some(command)) = (args.next().as_deref(), args.next()) {
        match std::process::Command::new(&command).spawn() {
            Ok(child) => info!(%command, pid = child.id(), "spawned client"),
            Err(err) => warn!(%command, ?err, "failed to spawn client"),
        }
    }

    event_loop.run(None, &mut state, move |state| {
        // Push queued protocol events (bind replies, xdg_surface.configure, frame
        // callbacks) out to clients after every dispatch. Without this a client
        // connects and then hangs forever waiting for its initial configure, so it
        // never draws anything.
        state.space.refresh();
        state.popups.cleanup();
        let _ = state.display_handle.flush_clients();
    })?;

    Ok(())
}
