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
                Rectangle::new(initial_window_location, initial_window_size),
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

        let output = self.space.outputs().next().unwrap();
        let output_geo = self.space.output_geometry(output).unwrap();
        let window_geo = self.space.element_geometry(window).unwrap();

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
