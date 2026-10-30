// SPDX-License-Identifier: GPL-3.0-or-later
//! Protocol glue for `ext-session-lock-v1`; the policy lives in [`crate::session_lock`].

use smithay::{
    output::Output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};

use crate::Wlrix;

impl SessionLockHandler for Wlrix {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // One locker at a time. Dropping the confirmation answers `finished`, and a client
        // holding a refused lock can neither draw on the lock screen nor unlock it. A holder
        // that has died gives way, or nothing could ever bring back a password prompt.
        if self.lock.is_held() {
            tracing::info!("refusing a lock: another client holds the session");
            return;
        }
        tracing::info!("session locking");
        self.lock.begin(confirmation);
        crate::session_lock::seal_input(self);
        // The confirmation is not sent yet: the client is told the session is locked
        // only once a locked frame has been drawn, in `after_render`.
        self.request_redraw();
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.lock.clear();
        // Focus was parked on the locker, which is going away.
        crate::focus::focus_topmost(self);
        self.request_redraw();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            // Without an output there is nothing to size the surface against, and
            // nowhere to draw it.
            return;
        };
        // A refused lock object still accepts this request. Taking its surface would let any
        // client replace the real lock screen and receive the password.
        if !self.lock.is_held_by(surface.wl_surface()) {
            tracing::warn!("ignoring a lock surface from a client that does not hold the lock");
            return;
        }
        self.lock.add_surface(&output, surface);
        // Typing must reach the locker rather than whatever had focus when the screen
        // locked.
        crate::session_lock::focus_lock_surface(self);
        crate::session_lock::refresh_pointer(self);
        self.request_redraw();
    }
}
