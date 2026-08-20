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
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
    wayland::{seat::WaylandFocus, shell::xdg::ToplevelSurface},
};

use crate::{Wlrix, desks};

impl Wlrix {
    /// Ask a window to close. This is a request, not a kill: a Wayland client may show a
    /// "save changes?" dialog and stay open. The window goes away for real when the client
    /// destroys its surface, handled in the shell teardown.
    pub fn close_window(&mut self, window: &Window) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.close();
        }
    }

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
        // A window with no minimize button must not be minimizable by the keybind or the menu
        // either. Enforced here, the one place every route goes through, rather than at each
        // of them -- a control that is drawn away but still reachable is worse than one that
        // was never removed.
        if !crate::frame::capabilities(window).minimizable {
            return;
        }
        {
            let mut state = desks::window_state(window).borrow_mut();
            if state.minimized {
                return;
            }
            state.minimized = true;
            // Snapshot the window for its icon; the backend does the capture on its next draw,
            // while the window's buffer is still around (a minimized window gets no frame
            // callbacks, so it never replaces it).
            state.needs_thumbnail = true;
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
        // Give it a cell in the minimized-icon grid (its old cell if still free).
        self.assign_icon_slot(window);
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
            // Drop the snapshot so a later re-minimize captures the window's fresh contents.
            state.needs_thumbnail = false;
            state.thumbnail = None;
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
        // Fullscreen outranks maximized: the window is already covering more than the work
        // area, so there is no geometry to apply. The flag is still recorded, because it is
        // what `unfullscreen_window` reads to decide where the window lands when it leaves
        // fullscreen -- a client that maximizes underneath comes back maximized.
        if desks::window_state(window).borrow().fullscreen {
            desks::window_state(window).borrow_mut().maximized = true;
            self.desks_changed();
            return;
        }
        // Same rule as `minimize_window`. Only the *entry* into the state is guarded:
        // `unmaximize_window` must keep working whatever the window says now, or a window that
        // narrowed its size hints while maximized could never be restored.
        if !crate::frame::capabilities(window).maximizable {
            return;
        }
        // A client may call `set_maximized` before its first commit, so this can run on a
        // window that has no size and no position yet. `outputs_for_element` would answer for a
        // zero-sized rectangle at the origin -- the first monitor, whichever that is -- and the
        // window would then be sized to fill a screen it is not about to open on. Ask placement
        // where it *will* go instead, and the two agree.
        let output = if crate::placement::is_placed(window) {
            self.space
                .outputs_for_element(window)
                .into_iter()
                .next()
                .or_else(|| self.space.outputs().next().cloned())
        } else {
            let pointer = self.pointer_location();
            crate::placement::output_for_new_window(&self.space, window, pointer)
        };
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
            // Only a window that has actually been on screen has anywhere to go back to. One
            // that asked to open maximized has not: it is sitting at the origin with no size
            // yet, and recording *that* as its restore geometry would drop it in the top-left
            // corner at nothing by nothing the first time it was un-maximized. Left `None`,
            // `unmaximize_window` hands the size back to the client and leaves it where it is,
            // which is the honest answer -- there was no earlier geometry.
            let size = window.geometry().size;
            if let Some(loc) = self.space.element_location(window)
                && crate::placement::is_placed(window)
                && size.w > 0
                && size.h > 0
            {
                state.restore_geo = Some(Rectangle::new(loc, size));
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

    /// The geometry half of maximizing: fill `output`'s work area, and nothing else.
    ///
    /// Split out so [`Wlrix::unfullscreen_window`] can put a window back to maximized without
    /// going through [`Wlrix::maximize_window`], which would overwrite `restore_geo` with the
    /// fullscreen rectangle and lose the geometry the window is eventually meant to return to.
    ///
    /// Reads the window's *current* frame insets, so a caller leaving fullscreen must clear
    /// that flag first -- a fullscreen window is undecorated, and computing against a frame it
    /// does not have yet would size it wrong by a titlebar.
    fn apply_maximize(&mut self, window: &Window, output: &Output) {
        let area = crate::placement::work_area(&self.space, output);
        let (l, t, r, b) = crate::frame::frame_style(window)
            .map(crate::decoration::insets)
            .unwrap_or((0, 0, 0, 0));
        let client_loc = area.loc + Point::from((l, t));
        let client_size = Size::from(((area.size.w - l - r).max(1), (area.size.h - t - b).max(1)));

        desks::window_state(window).borrow_mut().last_pos = client_loc;

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
        if self.space.element_location(window).is_some() {
            self.space.map_element(window.clone(), client_loc, true);
        }
    }

    /// Return a maximized window to its pre-maximize geometry.
    pub fn unmaximize_window(&mut self, window: &Window) {
        // See `maximize_window`: while fullscreen there is no maximized geometry on screen to
        // undo, only the flag saying where the window goes when fullscreen ends.
        if desks::window_state(window).borrow().fullscreen {
            desks::window_state(window).borrow_mut().maximized = false;
            self.desks_changed();
            return;
        }
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

    /// Make a window cover a whole output, panels and its own 4Dwm frame included.
    ///
    /// This is the one window state IRIX never had, and it is here for the applications Linux
    /// does have: a game asking for the screen, a video player, a browser going presentation
    /// mode. `output` is the monitor the client named, if it named one -- `xdg_toplevel
    /// .set_fullscreen` takes an optional `wl_output` and a game started on the second monitor
    /// is entitled to fill the second monitor.
    ///
    /// Unlike maximizing, this fills the **output geometry**, not the work area: covering the
    /// panels is the point.
    pub fn fullscreen_window(&mut self, window: &Window, output: Option<Output>) {
        if desks::window_state(window).borrow().fullscreen {
            return;
        }
        // The output the client asked for, else the one it is on, else -- for a window that
        // asked to open fullscreen before it had a position at all -- the one it is about to
        // open on. Same reasoning as `maximize_window`.
        let output = output.or_else(|| {
            if crate::placement::is_placed(window) {
                self.space
                    .outputs_for_element(window)
                    .into_iter()
                    .next()
                    .or_else(|| self.space.outputs().next().cloned())
            } else {
                let pointer = self.pointer_location();
                crate::placement::output_for_new_window(&self.space, window, pointer)
            }
        });
        let Some(output) = output else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };

        let mapped = self.space.element_location(window).is_some();
        {
            let mut state = desks::window_state(window).borrow_mut();
            // Only if it has actually been on screen, for the reason `maximize_window` gives:
            // a window that asked to open fullscreen has no earlier geometry, and recording
            // the placeholder would strand it at nothing by nothing in the corner.
            let size = window.geometry().size;
            if let Some(loc) = self.space.element_location(window)
                && crate::placement::is_placed(window)
                && size.w > 0
                && size.h > 0
            {
                state.pre_fullscreen = Some(Rectangle::new(loc, size));
            }
            state.fullscreen = true;
            state.last_pos = geometry.loc;
        }

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.set(xdg_toplevel::State::Fullscreen);
                s.size = Some(geometry.size);
            });
            toplevel.send_pending_configure();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_fullscreen(true);
            let _ = x11.configure(geometry);
        }
        if mapped {
            self.space.map_element(window.clone(), geometry.loc, true);
        }
        self.desks_changed();
        self.request_redraw();
    }

    /// Take a window out of fullscreen.
    ///
    /// Where it lands depends on what it was underneath: a window that was maximized when it
    /// went fullscreen comes back maximized, and only one that was neither maximized nor born
    /// fullscreen has a rectangle of its own to return to.
    pub fn unfullscreen_window(&mut self, window: &Window) {
        let (restore, mapped, maximized) = {
            let mut state = desks::window_state(window).borrow_mut();
            if !state.fullscreen {
                return;
            }
            // Cleared *before* anything reads the window's frame: `frame_of` suppresses
            // decorations while this is set, and the geometry below has to be computed against
            // the frame the window is getting back, not the one it is losing.
            state.fullscreen = false;
            (
                state.pre_fullscreen.take(),
                self.space.element_location(window).is_some(),
                state.maximized,
            )
        };

        if maximized {
            // Back to filling the work area. The Fullscreen state has to come off explicitly --
            // `apply_maximize` only sets Maximized, and a client left holding both would go on
            // drawing as if it still owned the whole screen.
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Fullscreen);
                });
            } else if let Some(x11) = window.x11_surface() {
                let _ = x11.set_fullscreen(false);
            }
            let output = self
                .space
                .outputs_for_element(window)
                .into_iter()
                .next()
                .or_else(|| self.space.outputs().next().cloned());
            if let Some(output) = output {
                self.apply_maximize(window, &output);
            }
            self.desks_changed();
            self.request_redraw();
            return;
        }

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.unset(xdg_toplevel::State::Fullscreen);
                s.size = restore.map(|g| g.size);
            });
            toplevel.send_pending_configure();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_fullscreen(false);
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

    /// Make a window fullscreen, or take it out if it already is.
    pub fn toggle_fullscreen_window(&mut self, window: &Window) {
        if desks::window_state(window).borrow().fullscreen {
            self.unfullscreen_window(window);
        } else {
            self.fullscreen_window(window, None);
        }
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
