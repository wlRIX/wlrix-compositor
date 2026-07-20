// SPDX-License-Identifier: GPL-3.0-or-later
//! Telling `wlrix-session` what it cannot know in advance.
//!
//! The session launches the compositor, but two things it needs are only decided once
//! the compositor is running: the Wayland socket name, chosen to avoid collisions, and
//! the X11 display number, chosen by XWayland. Apps the session starts need both in
//! their environment, and a process cannot reach into a sibling's environment after the
//! fact -- so the compositor reports them and the session waits.
//!
//! The channel is stdout, one `KEY=VALUE` line per fact, because a pipe on stdout is the
//! one fd a parent can hand a child with no ceremony. Logs go to stderr, so the two
//! never interleave.
//!
//! Only active when `WLRIX_SESSION_HANDSHAKE=1`, so running the compositor by hand does
//! not print machine-readable noise.

use std::io::Write;

/// Whether the process was started by something waiting to be told these values.
pub fn enabled() -> bool {
    std::env::var("WLRIX_SESSION_HANDSHAKE").as_deref() == Ok("1")
}

/// Report one fact to the session.
///
/// Flushed immediately: the session is blocked waiting for this line, and a buffered
/// write would deadlock until the compositor produced enough output to flush on its own.
pub fn announce(key: &str, value: &str) {
    if !enabled() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{key}={value}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        // The session has gone away. That is its problem to notice, not a reason to
        // bring the compositor down.
        tracing::debug!(key, "could not report to the session");
    }
}
