// SPDX-License-Identifier: GPL-3.0-or-later
//! Window operations: the single set of methods that both xdg client requests (see
//! `handlers/xdg_shell.rs`) and the `wlrix-desks` protocol drive.
//!
//! Each op works on a `smithay::desktop::Window` whether it wraps a Wayland toplevel or an
//! X11 surface, branching only where the two protocols differ. Ops are desk-aware: a window
//! on an inactive desk is held in `desks.hidden`, so an op updates its state and reconciles
//! whether it should be in the `Space` (visible) based on its desk.

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
    wayland::{seat::WaylandFocus, shell::xdg::ToplevelSurface},
};

use crate::{Wlrix, desks};

impl Wlrix {
    /// The window that currently holds keyboard focus, if any.
    pub fn focused_window(&self) -> Option<Window> {
        let focus: WlSurface = self.seat.get_keyboard()?.current_focus()?;
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&focus))
            .cloned()
    }

    /// The window backed by `surface`, whether it is mapped (active desk) or held aside.
    pub fn window_for_toplevel(&self, surface: &ToplevelSurface) -> Option<Window> {
        let matches = |w: &&Window| w.toplevel().is_some_and(|t| t == surface);
        self.space
            .elements()
            .find(matches)
            .cloned()
            .or_else(|| self.desks.hidden().iter().find(matches).cloned())
    }

    /// Whether a window's desk is currently on screen (the active desk, or the global one).
    fn on_visible_desk(&self, window: &Window) -> bool {
        let desk = desks::desk_of(window);
        desk == self.desks.active() || desk.is_global()
    }

    /// Raise a window to the top of the stack and focus it.
    pub fn raise_window(&mut self, window: &Window) {
        if self.space.element_location(window).is_none() {
            return; // not visible; nothing to raise
        }
        self.space.raise_element(window, true);
        crate::focus::focus_window(self, window);
        self.desks_changed();
        self.request_redraw();
    }

    /// Send a window to the bottom of the stack.
    ///
    /// Smithay's `Space` has no lower, so the visible stack is rebuilt with `window` at the
    /// bottom: capture every other mapped element with its position, then re-map `window`
    /// first and the rest on top in their existing order.
    pub fn lower_window(&mut self, window: &Window) {
        if self.space.element_location(window).is_none() {
            return;
        }
        let others: Vec<(Window, Point<i32, Logical>)> = self
            .space
            .elements()
            .filter(|w| *w != window)
            .filter_map(|w| self.space.element_location(w).map(|loc| (w.clone(), loc)))
            .collect();
        let Some(target_loc) = self.space.element_location(window) else {
            return;
        };

        for (w, _) in &others {
            self.space.unmap_elem(w);
        }
        self.space.unmap_elem(window);
        self.space.map_element(window.clone(), target_loc, false);
        for (w, loc) in others {
            self.space.map_element(w, loc, false);
        }
        // The lowered window should no longer hold focus; hand it to the new top.
        crate::focus::focus_topmost(self);
        self.desks_changed();
        self.request_redraw();
    }

    /// Hide a window without closing it. Restorable via [`Wlrix::restore_window`].
    pub fn minimize_window(&mut self, window: &Window) {
        {
            let mut state = desks::window_state(window).borrow_mut();
            if state.minimized {
                return;
            }
            state.minimized = true;
            if let Some(loc) = self.space.element_location(window) {
                state.restore_geo = Some(Rectangle::new(loc, window.geometry().size));
                state.last_pos = loc;
            }
        }
        if let Some(x11) = window.x11_surface() {
            let _ = x11.set_mapped(false);
        }
        // Mapped (active desk) -> take it out of the space; already hidden otherwise.
        if self.space.element_location(window).is_some() {
            self.space.unmap_elem(window);
            self.desks.hide(window.clone());
        }
        crate::focus::focus_topmost(self);
        self.desks_changed();
        self.request_redraw();
    }

    /// Un-minimize a window. Only shows it if its desk is on screen; otherwise it will
    /// appear (no longer minimized) when its desk is next activated.
    pub fn restore_window(&mut self, window: &Window) {
        {
            let mut state = desks::window_state(window).borrow_mut();
            if !state.minimized {
                return;
            }
            state.minimized = false;
        }
        if let Some(x11) = window.x11_surface() {
            let _ = x11.set_mapped(true);
        }
        if self.on_visible_desk(window) {
            let pos = {
                let state = desks::window_state(window).borrow();
                state.restore_geo.map(|g| g.loc).unwrap_or(state.last_pos)
            };
            self.desks.unhide(window);
            self.space.map_element(window.clone(), pos, true);
            crate::focus::focus_window(self, window);
        }
        self.desks_changed();
        self.request_redraw();
    }

    /// Resize a window to fill its output's work area.
    pub fn maximize_window(&mut self, window: &Window) {
        if desks::window_state(window).borrow().maximized {
            return;
        }
        let output = self
            .space
            .outputs_for_element(window)
            .into_iter()
            .next()
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };
        let area = crate::placement::work_area(&self.space, &output);
        // The client fills the work area *minus* its 4Dwm frame, so the frame (titlebar +
        // borders) fills the work area exactly rather than overflowing off-screen.
        let (l, t, r, b) = crate::frame::frame_style(window)
            .map(crate::decoration::insets)
            .unwrap_or((0, 0, 0, 0));
        let client_loc = area.loc + Point::from((l, t));
        let client_size = Size::from(((area.size.w - l - r).max(1), (area.size.h - t - b).max(1)));

        let mapped = self.space.element_location(window).is_some();
        {
            let mut state = desks::window_state(window).borrow_mut();
            if let Some(loc) = self.space.element_location(window) {
                state.restore_geo = Some(Rectangle::new(loc, window.geometry().size));
            }
            state.maximized = true;
            state.last_pos = client_loc;
        }

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.set(xdg_toplevel::State::Maximized);
                s.size = Some(client_size);
            });
            toplevel.send_pending_configure();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(true);
            let _ = x11.configure(Rectangle::new(client_loc, client_size));
        }
        if mapped {
            self.space.map_element(window.clone(), client_loc, true);
        }
        self.desks_changed();
        self.request_redraw();
    }

    /// Return a maximized window to its pre-maximize geometry.
    pub fn unmaximize_window(&mut self, window: &Window) {
        let (restore, mapped) = {
            let mut state = desks::window_state(window).borrow_mut();
            if !state.maximized {
                return;
            }
            state.maximized = false;
            (
                state.restore_geo.take(),
                self.space.element_location(window).is_some(),
            )
        };
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.unset(xdg_toplevel::State::Maximized);
                s.size = restore.map(|g| g.size);
            });
            toplevel.send_pending_configure();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(false);
            if let Some(geometry) = restore {
                let _ = x11.configure(geometry);
            }
        }
        if let Some(geometry) = restore {
            desks::window_state(window).borrow_mut().last_pos = geometry.loc;
            if mapped {
                self.space.map_element(window.clone(), geometry.loc, true);
            }
        }
        self.desks_changed();
        self.request_redraw();
    }

    /// Maximize a window, or un-maximize it if it already is.
    pub fn toggle_maximize_window(&mut self, window: &Window) {
        if desks::window_state(window).borrow().maximized {
            self.unmaximize_window(window);
        } else {
            self.maximize_window(window);
        }
    }

    /// Un-minimize every minimized window (used by a temporary keybind while there is no
    /// UI to pick one).
    pub fn restore_all_minimized(&mut self) {
        let minimized: Vec<Window> = self
            .desks
            .hidden()
            .iter()
            .filter(|w| desks::window_state(w).borrow().minimized)
            .cloned()
            .collect();
        for window in minimized {
            self.restore_window(&window);
        }
    }

    /// Move a window to another desk, reconciling whether it stays visible.
    pub fn move_window_to_desk(&mut self, window: &Window, target: desks::DeskId) {
        if !self.desks.exists(target) || desks::desk_of(window) == target {
            return;
        }
        desks::window_state(window).borrow_mut().desk = target;

        let visible = target == self.desks.active() || target.is_global();
        let minimized = desks::window_state(window).borrow().minimized;
        let mapped = self.space.element_location(window).is_some();

        if visible && !minimized && !mapped {
            let pos = desks::window_state(window).borrow().last_pos;
            self.desks.unhide(window);
            self.space.map_element(window.clone(), pos, false);
        } else if !visible && mapped {
            if let Some(loc) = self.space.element_location(window) {
                desks::window_state(window).borrow_mut().last_pos = loc;
            }
            self.space.unmap_elem(window);
            self.desks.hide(window.clone());
        }
        crate::focus::focus_topmost(self);
        self.desks_changed();
        self.request_redraw();
    }
}
