// SPDX-License-Identifier: GPL-3.0-or-later
//! `zwp_idle_inhibit_manager_v1`: a client asking not to be considered idle.
//!
//! A video player holds one of these while playing, so the screen does not dim or lock
//! part way through. It feeds the idle notifier in [`crate::idle`].

use smithay::{
    delegate_idle_inhibit, reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::idle_inhibit::IdleInhibitHandler,
};

use crate::Wlrix;

impl IdleInhibitHandler for Wlrix {
    fn inhibit(&mut self, _surface: WlSurface) {
        // Counted rather than tracked per surface: the notifier only cares whether
        // anything at all is inhibiting.
        //
        // A stricter compositor would only honor an inhibitor whose surface is
        // actually visible, so a minimized player cannot hold the session awake. That
        // needs visibility tracking wlRIX does not have yet.
        crate::idle::set_inhibited(self, true);
    }

    fn uninhibit(&mut self, _surface: WlSurface) {
        crate::idle::set_inhibited(self, false);
    }
}

delegate_idle_inhibit!(Wlrix);
