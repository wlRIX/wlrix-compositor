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
    desktop::{PopupManager, Window},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use crate::Wlrix;
use crate::config::FocusPolicy;

/// Whether a layer surface has asked for the keyboard outright.
///
/// While one has, nothing else may take focus. `KeyboardInteractivity::Exclusive` is not a
/// preference to weigh against click-to-focus -- it is a client saying "every key is mine
/// until I go away", which is what a screen locker, a full-screen menu and
/// `wlrix-screenshot`'s region overlay all need. Without this the overlay's Escape would work
/// only after a click, and every keystroke before that would go, invisibly, to whatever window
/// the overlay is covering.
///
/// Checked at the few entry points below rather than inside `keyboard.set_focus`, so the
/// refusal is visible where focus is *decided*.
///
/// **Do not call this while a `layer_map_for_output` guard is held.** It takes one of its own,
/// and that mutex is not reentrant -- a second guard deadlocks the event loop.
fn layer_holds_the_keyboard(state: &Wlrix) -> bool {
    state.exclusive_layer().is_some()
}

/// Give keyboard focus to `window` and raise it, as clicking one does.
pub fn focus_window(state: &mut Wlrix, window: &Window) {
    if layer_holds_the_keyboard(state) {
        return;
    }
    focus(state, window, Raise::Yes);
}

/// Give keyboard focus to `window` without touching the stacking order.
///
/// What pointer focus uses; see the module docs for why it must not raise.
pub fn focus_window_in_place(state: &mut Wlrix, window: &Window) {
    if layer_holds_the_keyboard(state) {
        return;
    }
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

/// Whether `window` may be given the keyboard at all.
///
/// An override-redirect X11 surface may not: a menu, a tooltip, a combo-box drop-down. It is by
/// definition a window the window manager does not manage -- the client put it there and the
/// client takes it down -- and the toolkit that opened it expects its *own* window to stay the
/// active one for as long as it is up.
///
/// Focusing one is not merely pointless, it destroys the popup. Focus carries the activated
/// state with it, an X11 client sees that as `_NET_WM_STATE_FOCUSED` leaving its main window,
/// and a toolkit closes its light-dismiss popups when the window they belong to is deactivated.
/// The menu would shut itself the moment the pointer crossed onto it, or the first click landed
/// on an item -- which reads as a menu that cannot be used rather than as a focus bug.
///
/// The click still reaches the client either way: pointer events are routed by
/// [`Wlrix::surface_under`], which has nothing to do with who holds the keyboard.
pub fn focusable(window: &Window) -> bool {
    !window
        .x11_surface()
        .is_some_and(|surface| surface.is_override_redirect())
}

/// Whether `window` has a menu of its own open: a popup, a drop-down, a tooltip.
///
/// A Wayland popup is an xdg surface hung off its toplevel rather than a window in the space, so
/// it is never what [`window_under`] finds -- the popup tree on the toplevel is the only place to
/// look. Smithay filters dead popups out of that tree, so this goes false the moment the client
/// destroys the menu rather than waiting on the next `PopupManager::cleanup`.
///
/// An X11 menu is an override-redirect window instead, and *is* in the space. There is nothing
/// to match one against its parent by -- every X11 client shares XWayland's single Wayland
/// client, so they all look like one client from here -- so any mapped one counts, and only
/// while an X11 window holds the keyboard. A Wayland app is therefore never pinned by some other
/// application's menu; an X11 one can be pinned by another X11 application's, which is the same
/// answer 4Dwm gave, a posted menu being modal to the desktop rather than to its window.
fn has_menu_open(state: &Wlrix, window: &Window) -> bool {
    if window.wl_surface().is_some_and(|surface| {
        PopupManager::popups_for_surface(surface.as_ref())
            .next()
            .is_some()
    }) {
        return true;
    }
    window.x11_surface().is_some()
        && state.space.elements().any(|other| {
            other
                .x11_surface()
                .is_some_and(|x11| x11.is_override_redirect())
        })
}

/// The window under `point`, its 4Dwm frame counting as part of it.
///
/// The frame is asked first. A point can be inside one window's border *and* inside a lower
/// window's client area at the same time, and the border is drawn on top there -- asking the
/// space first would answer with the window underneath. [`Wlrix::frame_under`] already refuses
/// to find a frame through a window covering it, so a miss there is a genuine miss.
///
/// A popup under the pointer answers `None` rather than the window beneath it: the pointer is
/// genuinely over the popup, and the callers all treat "nothing here" as *leave focus alone*,
/// which is what should happen while a menu is open.
pub fn window_under(state: &Wlrix, point: Point<f64, Logical>) -> Option<Window> {
    if let Some((window, _)) = state.frame_under(point) {
        return Some(window);
    }
    state
        .space
        .element_under(point)
        .map(|(window, _)| window.clone())
        .filter(focusable)
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
    // The same rule for a *client's* menu, which is a popup rather than compositor chrome.
    // 4Dwm worked this way: a posted menu owns the interaction until it is taken down, and
    // sliding off it -- onto the desktop, onto a frame drawn beneath it, onto another window
    // altogether -- does not hand the keyboard elsewhere.
    //
    // Here it is not merely a nicety. A toolkit closes its popups when the window they belong
    // to loses focus, so pointer focus wandering off an open menu dismissed the very menu the
    // user was reaching for. The menu goes when it is dismissed on purpose -- a click outside,
    // or Escape -- and the next motion after that is free to move focus again.
    if state
        .focused_window()
        .is_some_and(|window| has_menu_open(state, &window))
    {
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
    // A window closing while an exclusive layer surface is up must not hand the keyboard to
    // the next window along -- which is exactly what would happen when the window that was
    // focused before the overlay went up is closed underneath it.
    if let Some(surface) = state.exclusive_layer() {
        focus_layer_surface(state, &surface);
        return;
    }
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
    // Bound first so the borrow of the space ends before focusing needs it mutably. Topmost of
    // the windows that can hold focus: an open menu is above them all and is not one of them.
    let topmost = state.space.elements().rev().find(|w| focusable(w)).cloned();
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
    // Never while the session is locked. The lock surface holds the keyboard and must keep it:
    // a layer surface taking it would put whatever that client draws in front of a locked
    // screen, with the user's typing going to it. Guarded here rather than at the two call
    // sites, because this is the only way a layer surface can ever get the keyboard -- the
    // click path in `crate::input` reaches it, and so does the exclusive-interactivity path in
    // `crate::handlers::layer_shell`.
    if state.lock.is_locked() {
        return;
    }
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
    if layer_holds_the_keyboard(state) {
        return;
    }
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
