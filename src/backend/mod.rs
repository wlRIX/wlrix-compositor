// SPDX-License-Identifier: GPL-3.0-or-later
//! Backend selection.
//!
//! wlRIX runs on one of two backends:
//! - **winit**: nested inside an existing Wayland/X11 session — the dev loop.
//! - **udev**: DRM/KMS on real hardware — multi-monitor, zero-copy scanout.
//!
//! [`init`] picks winit when a host display is present, otherwise udev (TTY).

pub mod udev;
pub mod winit;

use crate::CalloopData;
use smithay::reexports::calloop::EventLoop;

/// Choose and start a backend based on the environment.
///
/// Returns whether `main` should run the calloop event loop: `true` for backends
/// that drive it (winit; later the udev render loop), `false` for a one-shot that
/// has already completed (the current udev discovery checkpoint).
pub fn init(
    event_loop: &mut EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
) -> Result<bool, Box<dyn std::error::Error>> {
    let nested =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    if nested {
        tracing::info!("host display detected — using winit (nested) backend");
        winit::init_winit(event_loop, data)?;
        Ok(true)
    } else {
        tracing::info!("no host display — using udev (DRM/KMS) backend");
        udev::init_udev(event_loop, data)
    }
}
