// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
//
//! wlRIX compositor
//!
//! A minimal Wayland compositor built on Smithay: opens a nested window under an
//! existing Wayland/X11 session, renders client windows with the GLES renderer, and
//! speaks xdg-shell.
#![allow(irrefutable_let_patterns)]

mod grabs;
mod handlers;
mod input;
mod state;
mod winit;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};
use tracing::info;

pub use state::Wlrix;

pub struct CalloopData {
    state: Wlrix,
    display_handle: DisplayHandle,
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
    };

    crate::winit::init_winit(&mut event_loop, &mut data)?;

    info!(
        socket = %data.state.socket_name.to_string_lossy(),
        "wlRIX compositor up. Point clients at WAYLAND_DISPLAY to connect."
    );

    // Optionally, auto-spawn a client: `wlrix-compositor -c <command>`.
    let mut args = std::env::args().skip(1);
    if let (Some("-c") | Some("--command"), Some(command)) = (args.next().as_deref(), args.next()) {
        info!(%command, "spawning client");
        std::process::Command::new(command).spawn().ok();
    }

    event_loop.run(None, &mut data, move |_| {
        // wlRIX is running.
    })?;

    Ok(())
}
