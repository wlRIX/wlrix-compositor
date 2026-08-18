// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use smithay::{
    desktop::{
        PopupKind, PopupManager, Space, Window, find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{
        Seat,
        pointer::{Focus, GrabStartData as PointerGrabStartData},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Resource,
            protocol::{wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, Serial},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    Wlrix,
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
};
use smithay::wayland::seat::WaylandFocus;

impl XdgShellHandler for Wlrix {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), false);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| {
                    w.toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == wl_surface)
                })
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();

            let grab = MoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
                end: crate::grabs::move_grab::MoveEnd::ButtonRelease,
                opaque: self.config.windows.opaque_move,
                current_location: initial_window_location,
            };

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| {
                    w.toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == wl_surface)
                })
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                // The client asked for this from inside a pointer grab of its own -- see
                // `check_grab` -- so a button is held and its release is what ends the resize.
                crate::grabs::resize_grab::ResizeEnd::ButtonRelease,
                Rectangle::new(initial_window_location, initial_window_size),
                self.config.windows.opaque_resize,
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The window may be on an inactive desk (held in `desks.hidden`, not the space), so
        // look there too or its desk membership would leak.
        let matches = |w: &&Window| w.toplevel().is_some_and(|toplevel| toplevel == &surface);
        let window = self
            .space
            .elements()
            .find(matches)
            .cloned()
            .or_else(|| self.desks.hidden().iter().find(matches).cloned());
        if let Some(window) = window {
            self.space.unmap_elem(&window);
            crate::desks::forget_window(&mut self.desks, &window);
            self.forget_window_menu(&window);
            self.forget_foreign_toplevel(&window);
        }
        // Focus would otherwise be left on a window that no longer exists.
        crate::focus::focus_topmost(self);
        self.desks_changed();
        self.request_redraw();
    }

    // Client-driven window state. Both these and the wlrix-desks protocol funnel into the
    // same `Wlrix` methods (see `window_ops.rs`).
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_toplevel(&surface) {
            self.maximize_window(&window);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_toplevel(&surface) {
            self.unmaximize_window(&window);
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_toplevel(&surface) {
            self.minimize_window(&window);
        }
    }
}

// Xdg Shell

fn check_grab(
    seat: &Seat<Wlrix>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<Wlrix>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
/// Returns a window that has just been placed, for the caller to focus.
pub fn handle_commit(
    popups: &mut PopupManager,
    space: &mut Space<Window>,
    surface: &WlSurface,
    pointer: Point<f64, Logical>,
) -> Option<Window> {
    let mut newly_placed = None;
    // Handle toplevel commits. Bound separately so the borrow of `space` ends before
    // placement needs it mutably.
    // Only Wayland toplevels: X11 windows share this space but carry no xdg state,
    // and they are placed when the X server asks us to map them.
    let toplevel = space
        .elements()
        .find(|w| {
            w.toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        })
        .cloned();
    if let Some(window) = toplevel {
        let (initial_configure_sent, app_id, title) = with_states(surface, |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (
                data.initial_configure_sent,
                data.app_id.clone(),
                data.title.clone(),
            )
        });

        if !initial_configure_sent {
            tracing::info!(
                app_id = app_id.as_deref().unwrap_or("<none>"),
                title = title.as_deref().unwrap_or("<none>"),
                "toplevel mapped"
            );

            window.toplevel().unwrap().send_configure();
        }

        // Give the window a position once its size is known; before that we would be
        // clamping against a zero-sized window.
        if crate::placement::place_if_new(space, &window, pointer) {
            newly_placed = Some(window);
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }

    newly_placed
}

/// The rectangle a popup is allowed to occupy, in its parent *surface's* coordinates.
///
/// The positioner works relative to the parent, so the output the popup must stay on has to be
/// moved into the parent's frame of reference: back by where the parent window sits, and back
/// again by where the popup's own parent surface sits inside that window (nested popups and
/// clients with a CSD margin are not at its origin).
fn popup_bounds(
    output_geometry: Rectangle<i32, Logical>,
    window_location: Point<i32, Logical>,
    toplevel_offset: Point<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let mut target = output_geometry;
    target.loc -= toplevel_offset;
    target.loc -= window_location;
    target
}

impl Wlrix {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&root))
        else {
            return;
        };

        // The output the parent window is *on*, not whichever one happens to come first.
        //
        // Naming the wrong monitor does not merely nudge a menu: the bounds are relative to the
        // parent, so the first output's rectangle, measured from a window on the second, lands
        // entirely to the *left* of that window. The positioner then dutifully slides the menu
        // out of the window to satisfy it, leaving a one-pixel sliver against the edge of the
        // other monitor. Every context menu in every window on the second screen was doing this.
        //
        // Same fallback as `open_window_menu` and `maximize_window`: a window on no output at
        // all still gets somewhere sensible to open its menus.
        let output = self
            .space
            .outputs_for_element(window)
            .into_iter()
            .next()
            .or_else(|| self.space.outputs().next().cloned());
        // Nothing to constrain against without an output. Answering with an empty rectangle
        // would be worse than answering with nothing: the positioner would shrink the menu to
        // fit it. These were three `unwrap`s, and a panic here takes the whole session down.
        let (Some(output), Some(window_geo)) = (output, self.space.element_geometry(window)) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let target = popup_bounds(
            output_geo,
            window_geo.loc,
            get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone())),
        );

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 2560x1440 monitors side by side, and a window on the right-hand one.
    fn first_output() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((0, 0)), (2560, 1440).into())
    }
    fn second_output() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((2560, 0)), (2560, 1440).into())
    }
    /// Where the Edge window that reported this actually sat.
    fn window_on_second() -> Point<i32, Logical> {
        Point::from((2703, 126))
    }

    /// A menu opened just inside a window has to be *allowed* just inside that window. This is
    /// the whole of the second-monitor bug: it is not that the menu was placed a little off,
    /// it is that the region it was told to stay within did not include the window at all, so
    /// the positioner had to move it somewhere else entirely.
    #[test]
    fn a_popup_may_open_inside_a_window_on_the_second_monitor() {
        let bounds = popup_bounds(second_output(), window_on_second(), Point::from((0, 0)));
        assert!(
            bounds.contains(Point::from((10, 10))),
            "a point just inside the window should be allowed, got {bounds:?}"
        );
    }

    /// The same window measured against the *first* output: what the bug did. Kept as a test so
    /// the failure is described rather than merely fixed -- every candidate position inside the
    /// window is outside these bounds.
    #[test]
    fn the_wrong_output_puts_the_whole_window_out_of_bounds() {
        let bounds = popup_bounds(first_output(), window_on_second(), Point::from((0, 0)));
        assert!(!bounds.contains(Point::from((10, 10))));
        // Off to the left of the window, which is why the menu ended up against the far edge.
        assert!(bounds.loc.x + bounds.size.w < 0);
    }

    /// A window on the first monitor was always fine, and must stay so.
    #[test]
    fn a_window_on_the_first_monitor_is_unaffected() {
        let bounds = popup_bounds(first_output(), Point::from((344, 8)), Point::from((0, 0)));
        assert!(bounds.contains(Point::from((10, 10))));
    }

    /// The popup's parent surface may sit inside the window rather than at its origin -- a
    /// nested submenu, or a client keeping a CSD margin -- and the bounds shift with it.
    #[test]
    fn the_parent_surface_offset_shifts_the_bounds() {
        let flat = popup_bounds(second_output(), window_on_second(), Point::from((0, 0)));
        let nested = popup_bounds(second_output(), window_on_second(), Point::from((40, 25)));
        assert_eq!(nested.loc, flat.loc - Point::from((40, 25)));
        assert_eq!(nested.size, flat.size);
    }
}
