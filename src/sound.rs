// SPDX-License-Identifier: GPL-3.0-or-later
//! Asking `wlrix-sound` to play something, from a wlRIX component.
//!
//! **This file is copied verbatim into each Rust component that plays sounds** -- the
//! compositor, the desktop and the session -- as `src/sound.rs`. Kept in step by hand, like
//! `pidfile.rs`; see `_docs/WORKSPACE.md` on `wlrix-common`. It needs nothing but `std`, so a
//! copy costs its host no dependencies.
//!
//! The daemon listens on a datagram socket, `$XDG_RUNTIME_DIR/wlrix-sound.sock`. A request is one
//! line of text:
//!
//! ```text
//! play <name>        play a sound; no reply
//! play-wait <name>   play a sound; the daemon answers "done" when it has finished
//! ```
//!
//! A datagram rather than the daemon's D-Bus interface because the callers are event loops that
//! must not stall -- the compositor most of all -- and have no bus connection to make the call
//! on. A send to a local datagram socket returns at once, whatever the daemon is doing, and
//! fails at once when there is no daemon. Every failure here is silence and nothing else: a
//! desktop with no sound daemon is a quiet desktop, not a broken one.
//!
//! Which sounds actually play -- the theme, the master switch, the muted list -- is the daemon's
//! decision, from `sounds.toml`. A caller asks for every sound and never reads that file.

#![allow(dead_code)]

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The daemon's socket, relative to `$XDG_RUNTIME_DIR`.
pub const SOCKET_NAME: &str = "wlrix-sound.sock";

/// Where the daemon listens, or `None` without a runtime directory.
pub fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(SOCKET_NAME))
}

/// Play the sound called `name` -- a freedesktop sound name such as `dialog-error`, or one of
/// wlRIX's own such as `wlrix-launch`. Returns at once; never fails.
pub fn play(name: &str) {
    let Some(path) = socket_path() else {
        return;
    };
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        let _ = socket.send_to(format!("play {name}").as_bytes(), path);
    }
}

/// Play `name` and wait until it has finished, or for `timeout`, whichever comes first. Returns
/// whether the daemon said it was done.
///
/// For a sound that has to be heard before something else happens -- the logout sound, before
/// the session takes everything down. A muted or missing sound is "done" at once.
pub fn play_and_wait(name: &str, timeout: Duration) -> bool {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let Some(path) = socket_path() else {
        return false;
    };
    // The reply needs an address to come back to. An abstract one, so there is no file to clean
    // up, named for this process and the moment so two waits never collide.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let name_bytes = format!("wlrix-sound-client.{}.{nanos}", std::process::id());
    let Ok(address) = SocketAddr::from_abstract_name(name_bytes.as_bytes()) else {
        return false;
    };
    let Ok(socket) = UnixDatagram::bind_addr(&address) else {
        return false;
    };
    if socket
        .send_to(format!("play-wait {name}").as_bytes(), path)
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + timeout;
    let mut reply = [0_u8; 16];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match socket.recv(&mut reply) {
            Ok(length) if &reply[..length] == b"done" => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// Wait up to `timeout` for the daemon to be listening. Returns whether it is.
///
/// For a caller that has just started it, as the session does at login: a sound asked for before
/// the daemon has bound its socket is simply lost.
pub fn wait_for_daemon(timeout: Duration) -> bool {
    let Some(path) = socket_path() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(socket) = UnixDatagram::unbound()
            && socket.connect(&path).is_ok()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
