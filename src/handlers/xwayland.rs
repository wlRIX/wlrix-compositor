// SPDX-License-Identifier: GPL-3.0-or-later
//! XWayland: running X11 applications.
//!
//! XWayland is an X server that presents its clients' windows as Wayland surfaces, and
//! expects the compositor to act as their X11 window manager. That is what [`XwmHandler`]
//! is: X11 clients ask to be mapped, moved, and resized through X protocol requests
//! rather than xdg-shell, and those requests arrive here.
//!
//! Smithay's [`Window`] wraps an `X11Surface` as readily as a Wayland toplevel, so once
//! a window is in the space it is placed, rendered, and stacked like any other. Only the
//! mapping handshake is different -- and X11 wants a two-way conversation: the client is
//! told the geometry it ended up with.

use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        seat::WaylandFocus,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, XwmId},
    },
};
use std::os::unix::io::OwnedFd;
use tracing::warn;

use crate::{Wlrix, decoration};

impl XWaylandShellHandler for Wlrix {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

/// The window in the space backed by `surface`, if it is mapped.
fn window_for(state: &Wlrix, surface: &X11Surface) -> Option<Window> {
    state
        .space
        .elements()
        .find(|window| window.x11_surface() == Some(surface))
        .cloned()
}

/// Which of our resize edges an X11 client asked for.
///
/// Hand-written rather than derived: smithay's X11 type is an enum of the eight directions and
/// ours is a bitfield, and there is no conversion between them in either direction.
fn resize_edge(edge: ResizeEdge) -> decoration::ResizeEdge {
    let (top, bottom) = match edge {
        ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => (true, false),
        ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => (false, true),
        ResizeEdge::Left | ResizeEdge::Right => (false, false),
    };
    let (left, right) = match edge {
        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => (true, false),
        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => (false, true),
        ResizeEdge::Top | ResizeEdge::Bottom => (false, false),
    };
    decoration::ResizeEdge {
        top,
        bottom,
        left,
        right,
    }
}

impl Wlrix {
    /// The window `surface` may start a pointer drag on, if it is allowed one at all.
    ///
    /// xdg-shell's move and resize requests carry the serial of the click that prompted them,
    /// and `check_grab` refuses one that does not match a real press. `_NET_WM_MOVERESIZE` has
    /// no serial, so an X11 client can ask at any moment for any reason, and something has to
    /// stand in for that check:
    ///
    /// - **It must have the keyboard.** A background window asking to be dragged is not a user
    ///   dragging it, and honoring that would let any X11 client take the pointer away from
    ///   whatever is actually being used.
    /// - **Nothing else may be grabbing.** A second grab replaces the first, so without this a
    ///   client could interrupt a drag already under way -- including one of its own windows
    ///   being resized by the frame.
    ///
    /// Neither is as strong as a serial, which is the honest state of `_NET_WM_MOVERESIZE`: the
    /// protocol does not carry enough to do better.
    fn grabbable(&self, surface: &X11Surface) -> Option<Window> {
        let window = window_for(self, surface)?;
        if self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return None;
        }
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        (focused.is_some() && focused.as_ref() == window.wl_surface().as_deref()).then_some(window)
    }
}

impl XwmHandler for Wlrix {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 window manager is running")
    }

    // The window exists but has not asked to be shown.
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        let class = surface.class();
        tracing::info!(class = %class, "X11 window mapped");
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
        // The window joins a desk, then takes focus when it opens, as a Wayland one does.
        crate::desks::assign_new_window(&self.desks, &window, Some(&class));
        crate::focus::focus_window(self, &window);
        self.desks_changed();

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
        // The window may be on an inactive desk (held in `desks.hidden`, not the space), so
        // look there too or its desk membership would leak.
        let window = window_for(self, &surface).or_else(|| {
            self.desks
                .hidden()
                .iter()
                .find(|w| w.x11_surface() == Some(&surface))
                .cloned()
        });
        if let Some(window) = window {
            self.space.unmap_elem(&window);
            crate::desks::forget_window(&mut self.desks, &window);
            self.forget_window_menu(&window);
            self.forget_foreign_toplevel(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
        // Focus would otherwise be left on a window that is gone.
        crate::focus::focus_topmost(self);
        self.desks_changed();
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
        // Honor size requests but keep placement ours, so an X11 client cannot drop
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

    /// `_NET_WM_MOVERESIZE`, resize half: the client is asking us to run a drag it has decided
    /// belongs to the window manager.
    ///
    /// This is how a window that draws its own chrome resizes at all -- Chromium and Edge draw
    /// their own edges and then hand the drag over here -- and it is the same grab a drag on our
    /// own 4Dwm border starts, so both routes end in one implementation.
    fn resize_request(&mut self, _xwm: XwmId, surface: X11Surface, button: u32, edge: ResizeEdge) {
        let Some(window) = self.grabbable(&surface) else {
            return;
        };
        self.start_resize(
            &window,
            resize_edge(edge),
            SERIAL_COUNTER.next_serial(),
            button,
        );
    }

    /// The move half of the same request, for a client dragging its own titlebar.
    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, button: u32) {
        let Some(window) = self.grabbable(&surface) else {
            return;
        };
        self.start_move(&window, SERIAL_COUNTER.next_serial(), button);
    }

    /// Whether an X11 client may read the selection.
    ///
    /// Only while one of its own windows holds focus: otherwise any X11 client could
    /// read the clipboard of whatever the user is actually working in.
    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        let Some(focus) = self.seat.get_keyboard().and_then(|kbd| kbd.current_focus()) else {
            return false;
        };
        self.space.elements().any(|window| {
            window.wl_surface().as_deref() == Some(&focus)
                && window
                    .x11_surface()
                    .and_then(|surface| surface.xwm_id())
                    .is_some_and(|id| id == xwm)
        })
    }

    /// An X11 client is reading a selection a Wayland client owns.
    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        // Each selection has its own error type, so they are reported separately.
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    warn!(?err, "failed to hand the Wayland clipboard to X11");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    warn!(?err, "failed to hand the Wayland primary selection to X11");
                }
            }
        }
    }

    /// An X11 client took ownership of a selection: offer it to Wayland clients.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        // Only clear what X11 itself put there, or we would drop a Wayland client's
        // selection on the floor.
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(top: bool, bottom: bool, left: bool, right: bool) -> decoration::ResizeEdge {
        decoration::ResizeEdge {
            top,
            bottom,
            left,
            right,
        }
    }

    /// All eight, because the conversion is two independent matches over the same value and
    /// nothing else would notice one of them being off by a corner -- the symptom is a window
    /// that grows from the wrong side, which looks like a resize bug rather than a mapping one.
    #[test]
    fn every_x11_resize_edge_maps_to_the_same_edge() {
        for (x11, expected) in [
            (ResizeEdge::Top, edges(true, false, false, false)),
            (ResizeEdge::Bottom, edges(false, true, false, false)),
            (ResizeEdge::Left, edges(false, false, true, false)),
            (ResizeEdge::Right, edges(false, false, false, true)),
            (ResizeEdge::TopLeft, edges(true, false, true, false)),
            (ResizeEdge::TopRight, edges(true, false, false, true)),
            (ResizeEdge::BottomLeft, edges(false, true, true, false)),
            (ResizeEdge::BottomRight, edges(false, true, false, true)),
        ] {
            assert_eq!(resize_edge(x11), expected, "{x11:?}");
        }
    }
}
