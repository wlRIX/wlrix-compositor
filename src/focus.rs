// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyboard focus.
//!
//! Everything that changes focus goes through here: opening a window, clicking one, and
//! closing the one that had focus.
//!
//! wlRIX follows click-to-focus, the modern default. IRIX also offers pointer focus,
//! where keyboard focus follows whichever window the cursor is over; that becomes a
//! configurable choice once wlRIX has configuration, and it belongs here -- the callers
//! decide *when* focus should change, this decides what changing it means.

use smithay::{
    desktop::Window, reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER, wayland::seat::WaylandFocus,
};

use crate::Wlrix;

/// Give keyboard focus to `window` and raise it.
pub fn focus_window(state: &mut Wlrix, window: &Window) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

    tracing::debug!(
        app_id = crate::placement::app_id(window)
            .as_deref()
            .unwrap_or("<none>"),
        "focusing window"
    );
    state.space.raise_element(window, true);
    set_activated(state, Some(window));

    // Works for X11 windows as well as Wayland toplevels.
    let surface = window.wl_surface().map(|surface| surface.into_owned());
    keyboard.set_focus(state, surface, serial);
}

/// Focus the topmost window, or clear focus if none are left.
///
/// Used when the focused window goes away: focus would otherwise be left pointing at a
/// surface that no longer exists, and typing would go nowhere.
pub fn focus_topmost(state: &mut Wlrix) {
    // Bound first so the borrow of the space ends before focusing needs it mutably.
    let topmost = state.space.elements().next_back().cloned();
    match topmost {
        Some(window) => focus_window(state, &window),
        None => clear_focus(state),
    }
}

/// Give keyboard focus to a layer-shell surface.
///
/// Layer surfaces are not in the `Space`, so there is nothing to raise -- the layer they sit
/// on already decides where they are in the stack -- and no window should be left drawing
/// itself as active. Only a surface that asked for `on-demand` or `exclusive` keyboard
/// interactivity ever reaches here; see [`Wlrix::focusable_layer_under`].
pub fn focus_layer_surface(state: &mut Wlrix, surface: &WlSurface) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

    tracing::debug!("focusing a layer surface");
    set_activated(state, None);
    keyboard.set_focus(state, Some(surface.clone()), serial);
}

/// Take keyboard focus away from every window.
pub fn clear_focus(state: &mut Wlrix) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

    tracing::debug!("clearing focus");
    set_activated(state, None);
    keyboard.set_focus(state, Option::<WlSurface>::None, serial);
}

/// Mark only `focused` as active, so clients draw themselves as focused or not.
fn set_activated(state: &Wlrix, focused: Option<&Window>) {
    for window in state.space.elements() {
        window.set_activated(Some(window) == focused);
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_pending_configure();
        }
    }
}
