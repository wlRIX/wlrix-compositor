// SPDX-License-Identifier: GPL-3.0-or-later
//! Stopping cleanly when asked to.
//!
//! greetd tears the greeter's compositor down with `SIGTERM` when a login succeeds, and a
//! person running the compositor from a terminal presses Ctrl+C. Left to the default
//! disposition the process dies at once, but abruptly: smithay's teardown never runs, so
//! the DRM master and the libseat session are dropped the hard way, and the handover to
//! the next session is slower and rougher for it.
//!
//! So the signal is turned into an orderly shutdown. A calloop [`Ping`] is fired from the
//! handler -- an eventfd write, which is async-signal-safe -- and its source, on the
//! event loop, stops the loop. The compositor then unwinds normally and releases the
//! device on its way out.

use std::sync::OnceLock;

use smithay::reexports::calloop::ping::Ping;

/// The ping the quit handler fires. Set once, before the handlers are installed.
static QUIT: OnceLock<Ping> = OnceLock::new();
/// The ping the `SIGHUP` handler fires, to reload the config on the event loop.
static RELOAD: OnceLock<Ping> = OnceLock::new();

/// Install `SIGTERM`/`SIGINT` handlers that fire `quit`, so the loop can stop itself.
pub fn forward_to_loop(quit: Ping) {
    if QUIT.set(quit).is_err() {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler does only async-signal-safe work -- firing the ping, which
        // is an eventfd write.
        unsafe { libc::signal(signal, handle as *const () as libc::sighandler_t) };
    }
}

/// Install a `SIGHUP` handler that fires `reload`, so the loop can re-read the config.
///
/// Kept separate from the quit handler on purpose: `SIGHUP` means "reload", not "stop".
pub fn forward_reload_to_loop(reload: Ping) {
    if RELOAD.set(reload).is_err() {
        return;
    }
    // SAFETY: async-signal-safe work only -- firing the ping (an eventfd write).
    unsafe {
        libc::signal(
            libc::SIGHUP,
            handle_reload as *const () as libc::sighandler_t,
        )
    };
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle(_signal: libc::c_int) {
    if let Some(quit) = QUIT.get() {
        quit.ping();
    }
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle_reload(_signal: libc::c_int) {
    if let Some(reload) = RELOAD.get() {
        reload.ping();
    }
}
