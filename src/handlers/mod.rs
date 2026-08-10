// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
mod compositor;
mod idle_inhibit;
pub mod layer_shell;
mod session_lock;
mod xdg_shell;
pub mod xwayland;

use crate::Wlrix;

//
// Wl Seat
//

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};

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
        // A move or resize grab owns the pointer outright while it runs. Two things would
        // otherwise take it away: smithay itself, which resets to the default arrow when the
        // grab clears the pointer focus, and the client, whose `set_cursor` for a surface it no
        // longer has the pointer over is stale by definition. See `Wlrix::grab_cursor`.
        if self.grab_cursor.is_some() {
            return;
        }

        // A client set (or hid) its cursor; the backends render whatever is current. The
        // compositor no longer owns it, so it must not hand anything "back" on the next motion.
        self.cursor_status = image;
        self.cursor_from_chrome = false;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

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
        let Some(xwm) = self.xwm.as_mut() else {
            return;
        };
        if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
            tracing::warn!(?err, ?ty, "failed to fetch an X11 selection for Wayland");
        }
    }
}

impl DataDeviceHandler for Wlrix {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

// Drag-and-drop: the defaults cancel a client-initiated drag, which is what wlRIX does today.
// `DndGrabHandler` is the input-side half (drop/cancel); wlRIX draws no drag icon, so there is
// nothing to tear down and the defaults suffice.
impl WaylandDndGrabHandler for Wlrix {}
impl smithay::input::dnd::DndGrabHandler for Wlrix {}

//
// Wl Output & Xdg Output
//

impl OutputHandler for Wlrix {}

//
// Linux dmabuf — lets clients hand us GPU buffers instead of shared memory,
// which is what makes hardware-accelerated clients (and direct scanout) possible.
//

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
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
use smithay::desktop::{PopupKind, PopupManager};
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface as ImePopupSurface};
use smithay::wayland::seat::WaylandFocus;

impl PrimarySelectionHandler for Wlrix {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

// Clipboard managers (`wl-paste --watch`, `cliphist`). The selection plumbing is shared with
// `SelectionHandler` above, so this is only the state getter.
impl smithay::wayland::selection::wlr_data_control::DataControlHandler for Wlrix {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::wlr_data_control::DataControlState {
        &mut self.data_control_state
    }
}

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

    // wlRIX does not honor the position hint, so a released constraint needs no warp.
    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
    }
}

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

impl XdgDecorationHandler for Wlrix {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        crate::frame::mark_negotiated_decorations(toplevel.wl_surface());
        set_server_side_decorations(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // We always draw the 4Dwm frame, so a client's preference is answered with
        // server-side regardless of what it asked for.
        crate::frame::mark_negotiated_decorations(toplevel.wl_surface());
        set_server_side_decorations(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // "No preference" still means the client is willing to be decorated by us, which is
        // the distinction that matters here -- unlike a client that never asked at all.
        crate::frame::mark_negotiated_decorations(toplevel.wl_surface());
        set_server_side_decorations(&toplevel);
    }
}

/// wlRIX draws 4Dwm-style server-side frames, so clients are told not to draw their own.
///
/// Only configure once the client has had its initial configure. Sending one before
/// that marks the initial configure as done, so the real one is never sent and the
/// client waits forever without ever mapping.
fn set_server_side_decorations(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

//
// Input methods (IME): text-input-v3 + input-method-v2 + virtual-keyboard-v1.
//
// An IME such as `fcitx5` binds input-method and drives the conversion; the application's text
// field speaks text-input. The candidate window arrives as an input-method popup, which is
// tracked in the same `PopupManager` as xdg popups -- `Window::render_elements` walks
// `PopupManager::popups_for_surface`, so tracking is all that is needed to make it appear.
//

impl InputMethodHandler for Wlrix {
    fn new_popup(&mut self, surface: ImePopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!(?err, "could not track the input-method popup");
        }
    }

    fn dismiss_popup(&mut self, surface: ImePopupSurface) {
        // Take it out of the parent's popup tree, rather than waiting for `PopupManager::cleanup`
        // to notice: the surface outlives the dismissal, so it would otherwise keep being drawn.
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
        self.request_redraw();
    }

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {
        self.request_redraw();
    }

    /// The parent window's **own** geometry, not its position in the space.
    ///
    /// This is the popup's parent offset, and the render path subtracts it from the popup's
    /// location (`Window::render_elements` draws popups at
    /// `window_render_loc + popup.location() - popup.geometry().loc`). The window's position is
    /// already accounted for by `window_render_loc`, so returning global coordinates here
    /// subtracts it a second time and pins the candidate window near the screen origin instead
    /// of leaving it at the caret.
    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        self.space
            .elements()
            .find(|window| window.wl_surface().as_deref() == Some(parent))
            .map(|window| window.geometry())
            .unwrap_or_default()
    }
}

// Named cursors. The shape arrives as `CursorImageStatus::Named` through `SeatHandler::
// cursor_image`, which is the same path the compositor's own chrome uses, so `cursor.rs`
// resolves it against the XCursor theme without anything further here.
//
// `cursor-shape` also lets a *tablet tool* name its cursor, so it requires this handler. wlRIX
// advertises no tablet manager, so no tool can exist to ask; the defaulted no-op is right.
impl smithay::input::tablet::TabletSeatHandler for Wlrix {
    type ToolFocus = WlSurface;
}

// The read-only running-window list bars and overviews read. The handles themselves are driven
// from `crate::foreign_toplevel`.
impl smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler for Wlrix {
    fn foreign_toplevel_list_state(
        &mut self,
    ) -> &mut smithay::wayland::foreign_toplevel_list::ForeignToplevelListState {
        &mut self.foreign_toplevel_state
    }
}

// Cross-client surface parenting: a portal or file picker exports a handle for the window that
// asked for it, and the dialog imports it to parent itself. Smithay tracks the relationship, so
// there is nothing to do beyond handing it the state.
impl smithay::wayland::xdg_foreign::XdgForeignHandler for Wlrix {
    fn xdg_foreign_state(&mut self) -> &mut smithay::wayland::xdg_foreign::XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

// Cheap client conveniences, both entirely handled by smithay: a 1x1 solid-color buffer, and a
// per-surface content-type tag. The tag is recorded in the surface's cached state for a future
// presentation policy (tearing, refresh matching); nothing reads it yet.

smithay::delegate_dispatch2!(Wlrix);
