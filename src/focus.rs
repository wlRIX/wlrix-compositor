// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyboard focus.
//!
//! Everything that changes focus goes through here: opening a window, clicking one, the
//! pointer crossing onto one, and closing the one that had focus. The callers decide *when*
//! focus should change; this decides what changing it means.
//!
//! ## The two policies
//!
//! `focus.policy` in `compositor.toml` picks between them, and both are IRIX's -- 4Dwm
//! inherited Motif's `explicit` and `pointer`:
//!
//! - **click** (the default): a press on a window focuses it *and raises it*.
//! - **pointer**: the window under the cursor has the keyboard, its 4Dwm frame included.
//!   Clicking still focuses and raises, which is the only way to bring a buried window to
//!   the front.
//!
//! Two things about pointer focus are worth stating outright, because both are decisions
//! rather than consequences:
//!
//! **It does not raise.** A window that leapt to the front the instant the cursor crossed it
//! would make a partly-covered window impossible to type into without disturbing the stack --
//! and sweeping the pointer across the screen would reshuffle every window it passed. Raising
//! stays a thing the user asks for by clicking.
//!
//! **Focus stays put over bare desktop.** Moving off a window and onto the desktop, an icon,
//! or a gap between windows leaves the keyboard where it was, rather than clearing it. Strict
//! Motif pointer focus drops focus to the root; here that would mean keystrokes disappearing
//! whenever the cursor crossed the few pixels between two windows, and it would fight the
//! desktop, which is a layer surface that takes focus of its own when clicked. A click on the
//! desktop still clears focus, so there is a deliberate way to let go.

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use crate::Wlrix;
use crate::config::FocusPolicy;

/// Give keyboard focus to `window` and raise it, as clicking one does.
pub fn focus_window(state: &mut Wlrix, window: &Window) {
    focus(state, window, Raise::Yes);
}

/// Give keyboard focus to `window` without touching the stacking order.
///
/// What pointer focus uses; see the module docs for why it must not raise.
pub fn focus_window_in_place(state: &mut Wlrix, window: &Window) {
    focus(state, window, Raise::No);
}

/// Whether focusing also brings the window to the front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Raise {
    Yes,
    No,
}

fn focus(state: &mut Wlrix, window: &Window, raise: Raise) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

    tracing::debug!(
        app_id = crate::placement::app_id(window)
            .as_deref()
            .unwrap_or("<none>"),
        raised = raise == Raise::Yes,
        "focusing window"
    );
    if raise == Raise::Yes {
        state.space.raise_element(window, true);
    }
    set_activated(state, Some(window));

    // Works for X11 windows as well as Wayland toplevels.
    let surface = window.wl_surface().map(|surface| surface.into_owned());
    keyboard.set_focus(state, surface, serial);
}

/// The window under `point`, its 4Dwm frame counting as part of it.
///
/// The frame is asked first. A point can be inside one window's border *and* inside a lower
/// window's client area at the same time, and the border is drawn on top there -- asking the
/// space first would answer with the window underneath. [`Wlrix::frame_under`] already refuses
/// to find a frame through a window covering it, so a miss there is a genuine miss.
pub fn window_under(state: &Wlrix, point: Point<f64, Logical>) -> Option<Window> {
    if let Some((window, _)) = state.frame_under(point) {
        return Some(window);
    }
    state
        .space
        .element_under(point)
        .map(|(window, _)| window.clone())
}

/// Move focus to whatever the pointer is now over, under the `pointer` policy.
///
/// Called on every motion event, so it is written to do nothing in the overwhelmingly common
/// case: the policy is checked first, and a window that already has focus is left alone rather
/// than being re-focused. Re-focusing would configure *every* window on screen -- see
/// [`set_activated`] -- thousands of times a second.
pub fn follow_pointer(state: &mut Wlrix, point: Point<f64, Logical>) {
    if state.config.focus.policy != FocusPolicy::Pointer {
        return;
    }
    // A locked session's focus belongs to the locker and nothing else.
    if state.lock.is_locked() {
        return;
    }
    // A grab owns the pointer: a window being moved or resized, or a client's own menu. The
    // window that started it keeps the keyboard until it is finished with.
    if state
        .seat
        .get_pointer()
        .is_some_and(|pointer| pointer.is_grabbed())
    {
        return;
    }
    // Compositor chrome laid over the windows. Both belong to a particular window and both
    // are steered by the pointer, so focus must not wander to whatever they are drawn over.
    if state.window_menu.is_some() || state.icon_drag.is_some() {
        return;
    }
    // An overlay- or top-layer surface covers every window here, so the pointer is not really
    // over the window beneath it.
    if state.layer_covers_windows_at(point) {
        return;
    }

    // No window under the pointer: keep focus where it is. See the module docs.
    let Some(window) = window_under(state, point) else {
        return;
    };
    if state.focused_window().as_ref() == Some(&window) {
        return;
    }
    focus_window_in_place(state, &window);
}

/// Hand focus to whatever should have it now, or clear it if nothing should.
///
/// Used when the focused window goes away, and when a desk switch changes what is on screen:
/// focus would otherwise be left pointing at a surface that is no longer there, and typing
/// would go nowhere.
///
/// The topmost window, normally. Under **pointer** focus the cursor is the authority on who
/// has the keyboard, and it has not moved -- so a window closing out from under it hands focus
/// to whatever the cursor is now over, rather than to whichever window happens to be on top.
/// Without this, pointer focus would work right up until something closed.
pub fn focus_topmost(state: &mut Wlrix) {
    if state.config.focus.policy == FocusPolicy::Pointer {
        let at = state
            .seat
            .get_pointer()
            .map(|pointer| pointer.current_location());
        if let Some(window) = at.and_then(|point| window_under(state, point)) {
            focus_window_in_place(state, &window);
            return;
        }
    }
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
