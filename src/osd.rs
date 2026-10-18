// SPDX-License-Identifier: GPL-3.0-or-later
//! Handing the volume and brightness keys to `wlrix-osd`.
//!
//! The compositor binds the keys -- they are global, and only it sees every key press -- but
//! does nothing with them itself: it has no business talking to PipeWire or logind. Each press is
//! one line on `wlrix-osd`'s datagram socket, `$XDG_RUNTIME_DIR/wlrix-osd.sock`, and that daemon
//! changes the level and shows the bar.
//!
//! A datagram for the reason [`crate::sound`] uses one: the event loop must never wait on anybody,
//! and a send to a local datagram socket returns at once, whatever is listening, or fails at once
//! when nothing is. A key pressed with no `wlrix-osd` running does nothing, and says so once in
//! the log.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;

use crate::keybinds::OsdKey;

/// The daemon's socket, relative to `$XDG_RUNTIME_DIR`. Hand-kept in step with `wlrix-osd`'s
/// `control.rs`.
const SOCKET_NAME: &str = "wlrix-osd.sock";

/// Whether a failed send has been logged, so a session without the daemon logs it once rather
/// than on every press of a key that is held.
static REPORTED: AtomicBool = AtomicBool::new(false);

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(SOCKET_NAME))
}

/// Send one key. Returns at once; never fails the caller.
pub fn send(key: OsdKey) {
    let Some(path) = socket_path() else {
        return;
    };
    let sent = UnixDatagram::unbound().and_then(|socket| {
        socket.set_nonblocking(true)?;
        socket.send_to(key.request().as_bytes(), &path)
    });
    match sent {
        Ok(_) => REPORTED.store(false, Ordering::Relaxed),
        Err(err) => {
            if !REPORTED.swap(true, Ordering::Relaxed) {
                warn!(
                    ?err,
                    "could not hand {} to wlrix-osd; is it running?",
                    key.request()
                );
            }
        }
    }
}
