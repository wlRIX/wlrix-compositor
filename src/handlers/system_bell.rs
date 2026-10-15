// SPDX-License-Identifier: GPL-3.0-or-later
//! `xdg_system_bell_v1`: a client ringing the bell.
//!
//! A terminal's `^G`, an editor refusing a keystroke, GTK's error bell. On IRIX this was
//! the keyboard bell, and so it is here: the `bell` sound, which `wlrix-sound` plays from the
//! theme when the System & Desktop Sounds panel's *Keyboard Bell* box is ticked. The same sound
//! the compositor plays itself for a window binding it refuses.
//!
//! The protocol lets a client name the surface the bell is for, so a compositor can flash it
//! instead of, or as well as, making a sound. IRIX had no visual bell, and neither does wlRIX,
//! so the surface is not used. A client ringing in a loop costs nothing: the sound daemon drops
//! a repeat that arrives while the last one has only just started.

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_system_bell::XdgSystemBellHandler,
};

use crate::Wlrix;

impl XdgSystemBellHandler for Wlrix {
    fn ring(&mut self, _surface: Option<WlSurface>) {
        crate::sound::play("bell");
    }
}
