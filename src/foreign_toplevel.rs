// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ext-foreign-toplevel-list-v1` window list: what a taskbar or overview reads to learn
//! which windows exist, and their titles and app ids.
//!
//! Read-only by design -- the protocol carries no way to act on a window. Control lives in the
//! bespoke [`crate::desks_protocol`] (and, later, `zwlr_foreign_toplevel_management_v1`).
//!
//! Smithay owns the protocol objects; the compositor has to tell it when a window appears,
//! when its title or app id changes, and when it goes. Rather than hooking the several places
//! a window can be mapped, this reconciles once per event-loop dispatch: any window without a
//! handle gets one, and any handle whose title or app id has drifted is updated. Closing is
//! the exception -- a destroyed window cannot be found by walking, so it is reported from the
//! shell teardown handlers.

use smithay::{desktop::Window, wayland::foreign_toplevel_list::ForeignToplevelHandle};

use crate::Wlrix;

/// Windows the list should not carry: override-redirect X11 surfaces are menus and tooltips,
/// not application windows, and a taskbar listing them would be noise.
fn is_listable(window: &Window) -> bool {
    !window
        .x11_surface()
        .is_some_and(|surface| surface.is_override_redirect())
}

impl Wlrix {
    /// Announce new windows and push any title/app-id changes. Called once per dispatch.
    pub fn refresh_foreign_toplevels(&mut self) {
        // Collected first: announcing borrows the protocol state mutably, which cannot happen
        // while the space is still borrowed for iteration.
        let windows: Vec<Window> = self
            .space
            .elements()
            .cloned()
            .chain(self.desks.hidden().iter().cloned())
            .filter(is_listable)
            .collect();

        for window in windows {
            let title = crate::frame::window_title(&window);
            let app_id = crate::placement::app_id(&window).unwrap_or_default();

            match window.user_data().get::<ForeignToplevelHandle>() {
                Some(handle) => {
                    // `title()`/`app_id()` are what was last sent, so this is the diff.
                    let mut changed = false;
                    if handle.title() != title {
                        handle.send_title(&title);
                        changed = true;
                    }
                    if handle.app_id() != app_id {
                        handle.send_app_id(&app_id);
                        changed = true;
                    }
                    if changed {
                        handle.send_done();
                    }
                }
                None => {
                    let handle = self
                        .foreign_toplevel_state
                        .new_toplevel::<Self>(title, app_id);
                    window.user_data().insert_if_missing(|| handle);
                }
            }
        }

        self.foreign_toplevel_state.cleanup_closed_handles();
    }

    /// Report a window as gone. Called from the shell teardown, where a destroyed window is
    /// still in hand -- it can no longer be found by walking the space.
    pub fn forget_foreign_toplevel(&mut self, window: &Window) {
        let Some(handle) = window.user_data().get::<ForeignToplevelHandle>() else {
            return;
        };
        handle.send_closed();
        self.foreign_toplevel_state.remove_toplevel(handle);
    }
}
