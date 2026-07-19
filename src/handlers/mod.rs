// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
mod compositor;
pub mod layer_shell;
mod session_lock;
mod xdg_shell;
mod xwayland;

use crate::Wlrix;

//
// Wl Seat
//

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for Wlrix {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Wlrix> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // A client set (or hid) its cursor; the backends render whatever is current.
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

delegate_seat!(Wlrix);

//
// Wl Data Device
//

impl SelectionHandler for Wlrix {
    type SelectionUserData = ();

    /// A Wayland client took ownership of a selection: offer it to X11 clients too.
    ///
    /// A source of `None` means the selection now belongs to X11 itself, which we must
    /// not echo back -- that would loop.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let Some(xwm) = self.xwm.as_mut() else {
            return;
        };
        if let Some(source) = source
            && let Err(err) = xwm.new_selection(ty, Some(source.mime_types()))
        {
            tracing::warn!(?err, ?ty, "failed to offer a Wayland selection to X11");
        }
    }

    /// A Wayland client is reading a selection an X11 client owns.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        let loop_handle = self.loop_handle.clone();
        let Some(xwm) = self.xwm.as_mut() else {
            return;
        };
        if let Err(err) = xwm.send_selection(ty, mime_type, fd, loop_handle) {
            tracing::warn!(?err, ?ty, "failed to fetch an X11 selection for Wayland");
        }
    }
}

impl DataDeviceHandler for Wlrix {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Wlrix {}
impl ServerDndGrabHandler for Wlrix {}

delegate_data_device!(Wlrix);

//
// Wl Output & Xdg Output
//

impl OutputHandler for Wlrix {}
delegate_output!(Wlrix);

//
// Linux dmabuf — lets clients hand us GPU buffers instead of shared memory,
// which is what makes hardware-accelerated clients (and direct scanout) possible.
//

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_dmabuf;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};

impl DmabufHandler for Wlrix {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state
            .as_mut()
            .expect("dmabuf global advertised without dmabuf state")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let imported = match self.renderer.as_ref() {
            // udev: test-import against the primary renderer so we only accept
            // buffers we can actually use.
            Some(renderer) => renderer.borrow_mut().import_dmabuf(&dmabuf, None).is_ok(),
            // winit: the renderer is owned by the winit backend and isn't shareable,
            // so we can't test-import here. Accept — the import is still performed
            // (and can still fail) per-frame at render time. Nested dev loop only.
            None => true,
        };

        if imported {
            let _ = notifier.successful::<Wlrix>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(Wlrix);

//
// Client compatibility protocols.
//
// Mostly small: clients probe for these and quietly lose functionality without them.
//

use smithay::wayland::{
    fractional_scale::{FractionalScaleHandler, with_fractional_scale},
    pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint},
    selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
    xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::{
    delegate_fractional_scale, delegate_pointer_constraints, delegate_presentation,
    delegate_primary_selection, delegate_relative_pointer, delegate_viewporter,
    delegate_xdg_activation, delegate_xdg_decoration,
};

impl PrimarySelectionHandler for Wlrix {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Wlrix);

delegate_presentation!(Wlrix);
delegate_viewporter!(Wlrix);
delegate_relative_pointer!(Wlrix);

impl FractionalScaleHandler for Wlrix {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Tell the client the scale of the output it is on, so it can render to match.
        let scale = self
            .space
            .outputs()
            .next()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}
delegate_fractional_scale!(Wlrix);

impl PointerConstraintsHandler for Wlrix {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        // Only honor a constraint while the pointer is actually over the surface.
        if pointer
            .current_focus()
            .is_some_and(|focus| &focus == surface)
        {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
    }
}
delegate_pointer_constraints!(Wlrix);

impl XdgActivationHandler for Wlrix {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Raise the window being activated rather than stealing keyboard focus.
        let window = self
            .space
            .elements()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == &surface)
            })
            .cloned();
        if let Some(window) = window {
            self.space.raise_element(&window, true);
            self.request_redraw();
        }
    }
}
delegate_xdg_activation!(Wlrix);

impl XdgDecorationHandler for Wlrix {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_client_side_decorations(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_client_side_decorations(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_client_side_decorations(&toplevel);
    }
}

/// wlRIX will draw 4Dwm-style frames eventually; until then clients draw their own,
/// which is what they do by default anyway.
///
/// Only configure once the client has had its initial configure. Sending one before
/// that marks the initial configure as done, so the real one is never sent and the
/// client waits forever without ever mapping.
fn set_client_side_decorations(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ClientSide);
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}
delegate_xdg_decoration!(Wlrix);
