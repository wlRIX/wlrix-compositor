// SPDX-License-Identifier: GPL-3.0-or-later
//! XWayland: running X11 applications.
//!
//! XWayland is an X server that presents its clients' windows as Wayland surfaces, and
//! expects the compositor to act as their X11 window manager. That is what [`XwmHandler`]
//! is: X11 clients ask to be mapped, moved and resized through X protocol requests
//! rather than xdg-shell, and those requests arrive here.
//!
//! Smithay's [`Window`] wraps an `X11Surface` as readily as a Wayland toplevel, so once
//! a window is in the space it is placed, rendered and stacked like any other. Only the
//! mapping handshake is different -- and X11 wants a two-way conversation: the client is
//! told the geometry it ended up with.

use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::{
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};
use std::os::unix::io::OwnedFd;
use tracing::warn;

use crate::{CalloopData, Wlrix};

impl XWaylandShellHandler for Wlrix {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
smithay::delegate_xwayland_shell!(Wlrix);

/// The window in the space backed by `surface`, if it is mapped.
fn window_for(state: &Wlrix, surface: &X11Surface) -> Option<Window> {
    state
        .space
        .elements()
        .find(|window| window.x11_surface() == Some(surface))
        .cloned()
}

// Smithay expects the dispatch state and the event loop's data to be the same type.
// They are not here, so the logic lives on `Wlrix` -- which has everything it needs --
// and `CalloopData` forwards to it. Both bounds are then satisfied.
impl XwmHandler for Wlrix {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 window manager is running")
    }

    // The window exists but has not asked to be shown.
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        tracing::info!(class = surface.class(), "X11 window mapped");
        if let Err(err) = surface.set_mapped(true) {
            warn!(?err, "failed to map X11 window");
            return;
        }

        // X11 reports the size now, so the window can be placed immediately -- unlike
        // a Wayland toplevel, nothing will call back once it has drawn.
        let size = surface.geometry().size;
        let window = Window::new_x11_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);

        let pointer = self.pointer_location();
        if let Some(output) = crate::placement::output_for_new_window(&self.space, pointer) {
            crate::placement::place_now(&mut self.space, &window, &output, size);
        }

        // X11 clients are told the geometry they ended up with, unlike Wayland ones
        // which are simply drawn where we put them.
        if let Some(location) = self.space.element_location(&window)
            && let Some(surface) = window.x11_surface()
        {
            let geometry = Rectangle::new(location, window.geometry().size);
            if let Err(err) = surface.configure(geometry) {
                warn!(?err, "failed to configure X11 window");
            }
        }

        self.request_redraw();
    }

    fn map_window_notify(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        // Menus, tooltips and the like: the client positions these itself, so they are
        // placed exactly where it asked rather than by our placement rules.
        let location = surface.geometry().loc;
        let window = Window::new_x11_window(surface);
        self.space.map_element(window, location, true);
        self.request_redraw();
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = window_for(self, &surface) {
            self.space.unmap_elem(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
        self.request_redraw();
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Honour size requests but keep placement ours, so an X11 client cannot drop
        // itself wherever it likes on the desktop.
        let mut geometry = surface.geometry();
        if let Some(w) = w {
            geometry.size.w = w as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h as i32;
        }
        if let Err(err) = surface.configure(geometry) {
            warn!(?err, "failed to answer X11 configure request");
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // The X server moved the window itself; follow it in the space.
        let Some(window) = window_for(self, &surface) else {
            return;
        };
        self.space.map_element(window, geometry.loc, false);
        self.request_redraw();
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _surface: X11Surface,
        _button: u32,
        _edge: ResizeEdge,
    ) {
        // Interactive resize of X11 windows is not wired up yet.
    }

    fn move_request(&mut self, _xwm: XwmId, _surface: X11Surface, _button: u32) {
        // Interactive move of X11 windows is not wired up yet.
    }

    // Clipboard between X11 and Wayland clients is not bridged yet, so refuse rather
    // than half-answer.
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        false
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        _selection: SelectionTarget,
        _mime_type: String,
        _fd: OwnedFd,
    ) {
    }
}

/// `X11Wm::start_wm` requires this on the loop data as well.
impl XWaylandShellHandler for CalloopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.state.xwayland_shell_state()
    }
}

/// Forwards to the implementation on [`Wlrix`]; see the note above.
impl XwmHandler for CalloopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window)
    }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window)
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window)
    }
    fn map_window_notify(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_notify(xwm, window)
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window)
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window)
    }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window)
    }
    #[allow(clippy::too_many_arguments)]
    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state
            .configure_request(xwm, window, x, y, w, h, reorder)
    }
    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above)
    }
    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        self.state.resize_request(xwm, window, button, edge)
    }
    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        self.state.move_request(xwm, window, button)
    }
    fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool {
        self.state.allow_selection_access(xwm, selection)
    }
    fn send_selection(
        &mut self,
        xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        self.state.send_selection(xwm, selection, mime_type, fd)
    }
}
