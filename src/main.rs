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
mod color_management;
mod config;
mod cursor;
mod decoration;
mod desks;
mod desks_protocol;
mod focus;
mod foreign_toplevel;
mod foreign_toplevel_management;
mod frame;
mod gamma;
mod grabs;
mod handlers;
mod handshake;
mod hdr;
mod hdr_render;
mod idle;
mod image_capture;
mod input;
mod keybinds;
mod logging;
mod menu;
mod minimized;
mod output_management;
mod outputs;
mod palette;
mod pidfile;
mod placement;
mod pointer_constraints;
mod power;
mod protocols;
mod render;
mod screencopy;
mod security_context;
mod session_lock;
mod signals;
mod state;
mod text;
mod thumbnail;
mod vrr;
mod window_ops;
mod workspace_protocol;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{info, warn};

pub use state::Wlrix;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `--check-config <path>`: parse that file with the types below and say whether it would
    // be accepted. Handled before anything else because it starts nothing -- no log file, no
    // event loop, no socket -- and because it is what `wlrix-settings-daemon` runs against a
    // candidate file before renaming it into place. That check is what makes a settings app
    // structurally unable to write a config this compositor would reject, and with
    // `deny_unknown_fields` a rejected config is not a wrong setting, it is the user's whole
    // file replaced by built-in defaults.
    if let Some(exit) = check_config() {
        return exit;
    }

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
    // The pointer theme goes out **before** the socket, deliberately. `WAYLAND_DISPLAY` is what
    // the session is blocked on, and it stops reading the moment it has that plus `DISPLAY`, so
    // a fact announced after it is a fact that may never be read. This is also the whole reason
    // the compositor announces it at all: the theme is settled here, in `compositor.toml`, and
    // an app started by the session has to be told, because a process cannot reach into a
    // sibling's environment afterwards.
    handshake::announce("XCURSOR_THEME", &state.config.cursor.theme());
    handshake::announce("XCURSOR_SIZE", &state.config.cursor.size().to_string());
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

    // Start the blank countdown, if one is configured: a session left alone from the moment it
    // starts should still blank, without waiting for a first keypress to arm the timer.
    state.arm_blank_timer();

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
        // Stream live window geometry/state to any wlrix-desks client (the Desks Overview).
        // Once per dispatch, after refresh, so a dragged window's new position is picked up.
        state.emit_desk_updates();
        // Persist the desks if they were renamed, reordered, added to or switched. A no-op
        // otherwise, and once per dispatch rather than per change, so a client doing several
        // at once writes the file a single time.
        state.save_desks_if_dirty();
        // Answer any `wp_image_description_v1.get_information` from the last dispatch. Cannot
        // be done inline; see `flush_image_description_info`.
        state.flush_image_description_info();
        // Tell any surface that has moved to a differently-colored output. Reconciled here
        // rather than signaled from the move, because a window changes output in several
        // places and none of them are about color.
        state.refresh_color_feedback();
        // Announce new windows / changed titles to the read-only window list.
        state.refresh_foreign_toplevels();
        state.refresh_foreign_toplevel_management();
        // Keep image-capture sessions' buffer sizes in step with what they are capturing.
        state.refresh_image_capture();
        let _ = state.display_handle.flush_clients();
    })?;

    Ok(())
}

/// Handle `--check-config <path>`, if that is what was asked for.
///
/// `Some` means the program is done -- the answer has been printed and the exit status is the
/// result. `None` means carry on and be a compositor.
///
/// Deliberately not routed through `config::load`: that one reports through `tracing` and falls
/// back to defaults, which is right for starting up (a broken config should still leave a
/// desktop to repair it from) and exactly wrong here, where the whole question is whether the
/// file is acceptable.
fn check_config() -> Option<Result<(), Box<dyn std::error::Error>>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--check-config") {
        return None;
    }
    let Some(path) = args.next() else {
        eprintln!("--check-config needs a path");
        std::process::exit(2);
    };
    match config::check(std::path::Path::new(&path)) {
        Ok(()) => Some(Ok(())),
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(1);
        }
    }
}
