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
mod config;
mod cursor;
mod focus;
mod grabs;
mod handlers;
mod handshake;
mod idle;
mod input;
mod logging;
mod output_management;
mod outputs;
mod palette;
mod pidfile;
mod placement;
mod render;
mod screencopy;
mod session_lock;
mod signals;
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

    // The config decides the keyboard keymap and (later) display defaults. Loaded here so
    // where it came from -- or that a file was rejected -- is logged once, up front.
    let loaded = config::load();
    loaded.source.report();

    let display: Display<Wlrix> = Display::new()?;
    let mut state = Wlrix::new(&mut event_loop, display, loaded.config);

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
    //
    // When that client exits, so does the compositor. This is what makes `-c` right for a
    // greeter: greetd starts `wlrix-compositor -c wlrix-greeter` and then waits for that
    // whole command to exit before starting the session. Without this the compositor
    // outlived the greeter that logged in, and greetd sat waiting for its kill timeout --
    // several seconds of a bare desktop between the login and the session.
    let mut args = std::env::args().skip(1);
    if let (Some("-c") | Some("--command"), Some(command)) = (args.next().as_deref(), args.next()) {
        match std::process::Command::new(&command).spawn() {
            Ok(mut child) => {
                info!(%command, pid = child.id(), "spawned client");
                // Poll for the child's exit and stop the loop when it goes. A pidfd would
                // be event-driven, but a short poll is simple, and only the short-lived
                // greeter compositor ever runs it -- the session's compositor has no `-c`.
                event_loop
                    .handle()
                    .insert_source(
                        smithay::reexports::calloop::timer::Timer::from_duration(
                            std::time::Duration::from_millis(200),
                        ),
                        move |_, _, state| {
                            use smithay::reexports::calloop::timer::TimeoutAction;
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    info!(?status, "spawned client exited; shutting down");
                                    state.loop_signal.stop();
                                    TimeoutAction::Drop
                                }
                                Ok(None) => {
                                    TimeoutAction::ToDuration(std::time::Duration::from_millis(200))
                                }
                                Err(err) => {
                                    warn!(?err, "could not check on the spawned client");
                                    TimeoutAction::Drop
                                }
                            }
                        },
                    )
                    .expect("could not watch the spawned client");
            }
            Err(err) => warn!(%command, ?err, "failed to spawn client"),
        }
    }

    // Drop a pidfile so the settings apps can find this compositor to SIGHUP it. Held
    // until the event loop returns, then its guard removes the file.
    let _pidfile = pidfile::write();

    // Stop cleanly on SIGTERM (greetd's teardown) or Ctrl+C, so the device is released
    // in order rather than abandoned. The handler fires the ping; the source stops the
    // loop.
    let (quit_ping, quit_source) =
        smithay::reexports::calloop::ping::make_ping().expect("could not create the quit ping");
    event_loop
        .handle()
        .insert_source(quit_source, |_, _, state| {
            info!("shutting down");
            state.loop_signal.stop();
        })
        .expect("could not insert the quit source");
    signals::forward_to_loop(quit_ping);

    // Re-read the config on SIGHUP, so the keyboard layout (and repeat) can change while
    // the compositor runs -- `kill -HUP`, or a future settings app, applies edits live
    // without a restart.
    let (reload_ping, reload_source) =
        smithay::reexports::calloop::ping::make_ping().expect("could not create the reload ping");
    event_loop
        .handle()
        .insert_source(reload_source, |_, _, state| {
            info!("reloading config (SIGHUP)");
            state.reload_config();
        })
        .expect("could not insert the reload source");
    signals::forward_reload_to_loop(reload_ping);

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
