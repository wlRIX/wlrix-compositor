// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-session-lock-v1`: locking the screen.
//!
//! A lock client (a screen locker) asks to lock the session, then supplies one surface
//! per output to draw the lock screen. While locked, no client content may be visible
//! and no input may reach ordinary windows -- that is the whole point of the protocol,
//! and it is the compositor's job to enforce, not the locker's.
//!
//! Enforcement lives in two places, both of them choke points every path already goes
//! through: [`lock_elements`] decides what an output draws, and `Wlrix::surface_under`
//! decides what the pointer can reach. A lock that is only honored by the drawing code
//! would still leak clicks through to the desktop underneath.
//!
//! An output with no lock surface -- one that appeared after the lock, or whose locker
//! died -- is painted black rather than left showing the desktop. Failing closed is the
//! only safe direction here.
//!
//! The keyboard is enforced at a third choke point, [`keep_keyboard`], which runs before every
//! key while locked. Each way of taking the keyboard is also refused where it is asked for, but
//! this is what keeps a password from reaching a client through a path nobody thought of: an
//! input method's grab, say, which smithay installs without asking the compositor.
//!
//! Only the client that holds the lock may unlock it, draw on it or be answered `locked`. A
//! second client is told `finished`, unless the holder has died: then the next locker takes
//! over. That also means a process running as the user can kill the locker and take its place,
//! which is as far as a compositor can go while it cannot tell a real locker from any other
//! client of the same user.

use smithay::backend::renderer::Color32F;
use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem,
        element::{
            Kind,
            solid::{SolidColorBuffer, SolidColorRenderElement},
            surface::render_elements_from_surface_tree,
        },
    },
    desktop::{PopupManager, layer_map_for_output},
    input::pointer::MotionEvent,
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER},
    wayland::{
        seat::WaylandFocus,
        session_lock::{LockSurface, SessionLocker},
    },
};

use crate::{
    Wlrix,
    render::{OutputElem, OutputElement},
};

/// Whether the session is locked, and what each output should show while it is.
#[derive(Default)]
pub struct LockState {
    locked: bool,
    /// Keyed by output name, since an `Output` is not hashable and names are stable
    /// for as long as the connector exists.
    surfaces: HashMap<String, LockSurface>,
    /// Held from the lock request until a locked frame has actually been drawn.
    ///
    /// The client is told the session is locked only once the desktop is genuinely off
    /// screen; confirming any earlier would be claiming something not yet true, and the
    /// client may reveal secrets on the strength of it.
    pending: Option<SessionLocker>,
    /// The lock object of the client the session belongs to while locked.
    holder: Option<ExtSessionLockV1>,
}

impl LockState {
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Whether the session is locked by a client that is still there to unlock it.
    pub fn is_held(&self) -> bool {
        self.locked && self.holder.as_ref().is_some_and(|holder| holder.is_alive())
    }

    /// Whether `surface` belongs to the client that holds the lock.
    pub fn is_held_by(&self, surface: &WlSurface) -> bool {
        match (
            self.holder.as_ref().and_then(|holder| holder.client()),
            surface.client(),
        ) {
            (Some(holder), Some(client)) => holder.id() == client.id(),
            _ => false,
        }
    }

    /// Begin locking, taking over from a holder that has died. The confirmation is held until a
    /// locked frame has been drawn.
    pub fn begin(&mut self, confirmation: SessionLocker) {
        self.surfaces.clear();
        self.locked = true;
        self.holder = Some(confirmation.ext_session_lock().clone());
        self.pending = Some(confirmation);
    }

    /// Confirm the lock, once the caller has drawn a frame in the locked state.
    ///
    /// Dropping a [`SessionLocker`] without calling `lock` reports failure to the
    /// client, so this must not be reached before the screen is actually covered.
    pub fn confirm(&mut self) {
        if let Some(confirmation) = self.pending.take() {
            confirmation.lock();
            // Here rather than when the lock was asked for: this is the moment the screen is
            // actually covered, and a sound that said "locked" before then would be a lie.
            crate::sound::play("desktop-screen-lock");
        }
    }

    pub fn add_surface(&mut self, output: &Output, surface: LockSurface) {
        configure(output, &surface);
        self.surfaces.insert(output.name(), surface);
    }

    /// Tell every locker surface its output's size again, after a change to the layout -- a
    /// scale, a mode, a rotation -- so the lock still covers the whole screen.
    pub fn reconfigure(&self, outputs: &[Output]) {
        for output in outputs {
            if let Some(surface) = self.surfaces.get(&output.name()) {
                configure(output, surface);
            }
        }
    }

    /// Forget everything: the session is unlocked, or the locker is gone.
    pub fn clear(&mut self) {
        self.locked = false;
        self.surfaces.clear();
        self.pending = None;
        self.holder = None;
    }

    fn surface_for(&self, output: &Output) -> Option<&LockSurface> {
        self.surfaces.get(&output.name())
    }

    /// The surfaces that can still take input. A locker that died leaves its entries behind.
    fn live_surfaces(&self) -> impl Iterator<Item = &WlSurface> {
        self.surfaces
            .values()
            .map(|surface| surface.wl_surface())
            .filter(|surface| surface.alive() && surface.client().is_some())
    }
}

/// What `output` draws while the session is locked.
///
/// Always at least an opaque black fill, so an output with no lock surface shows
/// nothing of the desktop rather than falling through to it.
pub fn lock_elements<R>(state: &Wlrix, renderer: &mut R, output: &Output) -> Vec<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
    let size = state
        .space
        .output_geometry(output)
        .map(|geometry| geometry.size)
        .unwrap_or_default();
    let mut elements: Vec<OutputElem<R>> = Vec::new();

    if let Some(surface) = state.lock.surface_for(output) {
        // Output-relative, like everything else the damage tracker is handed: the
        // locker's surface covers its own output starting at that output's corner.
        elements.extend(
            render_elements_from_surface_tree(
                renderer,
                surface.wl_surface(),
                (0, 0),
                scale,
                1.0,
                Kind::Unspecified,
            )
            .into_iter()
            .map(OutputElement::Surface),
        );
    }

    // Underneath whatever the locker drew, in case it is translucent or undersized.
    let mut buffer =
        SolidColorBuffer::new(size, Color32F::from(state.palette.locked.to_f32_array()));
    buffer.resize(size);
    elements.push(OutputElement::Solid(SolidColorRenderElement::from_buffer(
        &buffer,
        (0, 0),
        scale,
        1.0,
        smithay::backend::renderer::element::Kind::Unspecified,
    )));

    elements
}

/// Called by a backend once it has finished drawing a frame.
///
/// This is what confirms the lock to the client: by now the locked frame has been
/// composited, so the desktop really is off screen and the claim is true.
pub fn after_render(state: &mut Wlrix) {
    if state.lock.is_locked() {
        state.lock.confirm();
    }
}

/// The lock surface under `pos`, if the pointer is over one.
///
/// While locked this replaces the usual window hit-testing entirely, so a click cannot
/// reach the desktop underneath.
pub fn surface_under(
    state: &Wlrix,
    pos: Point<f64, Logical>,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    let output = state.space.output_under(pos).next()?;
    let geometry = state.space.output_geometry(output)?;
    let surface = state.lock.surface_for(output)?;
    // The locker's surface covers its output exactly, so it starts at the output's corner.
    // That corner is what is returned: like every `surface_under`, the second half is where the
    // surface's origin is on the desktop, and smithay subtracts it from the pointer to get the
    // surface-local position. This once returned the surface-local position itself, so the
    // subtraction cancelled the pointer out and every click and tap reached the locker at its
    // top-left corner -- typing worked, the password field and the Unlock button never did.
    Some((surface.wl_surface().clone(), geometry.loc.to_f64()))
}

/// Move keyboard focus onto a lock surface, so typing reaches the locker and not the
/// desktop behind it. With no locker to give it to, nobody has the keyboard: focus left on a
/// window would send the next password attempt to something the lock is hiding.
pub fn focus_lock_surface(state: &mut Wlrix) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    // Any of them will do -- the locker is one client, and this is only about getting
    // focus off the desktop.
    let surface = state.lock.live_surfaces().next().cloned();
    keyboard.set_focus(
        state,
        surface.map(crate::focus::KeyboardFocusTarget::Surface),
        SERIAL_COUNTER.next_serial(),
    );
}

/// Make sure the next key goes to the locker, and only the locker. Called before every key while
/// locked; see the module docs.
pub fn keep_keyboard(state: &mut Wlrix) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    if keyboard.is_grabbed() {
        tracing::warn!("ending a keyboard grab that was active while locked");
        keyboard.unset_grab(state);
    }
    let focus = keyboard.current_focus();
    let with_the_locker = match focus.as_ref().and_then(|focus| focus.wl_surface()) {
        Some(surface) => state.lock.live_surfaces().any(|live| live == &*surface),
        None => focus.is_none() && state.lock.live_surfaces().next().is_none(),
    };
    if !with_the_locker {
        focus_lock_surface(state);
    }
}

/// Take the seat away from the desktop, as a lock begins: every menu closed, every grab ended, and
/// the keyboard and pointer off whatever had them. A grab outlives a focus change, so an open menu
/// would otherwise go on receiving what is typed at the lock screen.
pub fn seal_input(state: &mut Wlrix) {
    state.close_window_menu();
    dismiss_popups(state);

    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.unset_grab(state);
    }
    focus_lock_surface(state);

    if let Some(pointer) = state.seat.get_pointer() {
        if pointer.is_grabbed() {
            pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), now_msec(state));
        }
    }
    refresh_pointer(state);
}

/// Tell every client with a popup open that it is gone.
fn dismiss_popups(state: &Wlrix) {
    let mut roots: Vec<WlSurface> = state
        .space
        .elements()
        .chain(state.desks.hidden().iter())
        .filter_map(|window| window.toplevel().map(|top| top.wl_surface().clone()))
        .collect();
    // Collected first: `layer_map_for_output` hands back a guard that is not reentrant.
    for output in state.space.outputs() {
        let map = layer_map_for_output(output);
        roots.extend(map.layers().map(|layer| layer.wl_surface().clone()));
    }
    for root in roots {
        let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
        for (popup, _) in popups {
            let _ = PopupManager::dismiss_popup(&root, &popup);
        }
    }
}

/// Send the pointer's focus to whatever is under it now, without waiting for it to move. Until it
/// moves, a click would otherwise still go to the window under it before the lock.
pub fn refresh_pointer(state: &mut Wlrix) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let location = pointer.current_location();
    let under = state.surface_under(location);
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: now_msec(state),
        },
    );
    pointer.frame(state);
}

/// Now, on the clock input events are stamped with, for the pointer events a lock makes up.
fn now_msec(state: &Wlrix) -> u32 {
    std::time::Duration::from(state.clock.now()).as_millis() as u32
}

/// Tell a locker surface the size of its output in logical pixels: the mode, turned for the
/// output's rotation, divided by its *fractional* scale. The integer scale this once used rounds
/// 1.5 up to 2, so on a 150 % laptop panel the lock screen covered three quarters of it each way
/// and left the rest black.
fn configure(output: &Output, surface: &LockSurface) {
    let mode = output
        .current_mode()
        .map(|mode| mode.size)
        .unwrap_or_default();
    let logical = output
        .current_transform()
        .transform_size(mode)
        .to_f64()
        .to_logical(output.current_scale().fractional_scale())
        .to_i32_round::<i32>();
    surface.with_pending_state(|state| {
        state.size = Some((logical.w.max(0) as u32, logical.h.max(0) as u32).into());
    });
    surface.send_configure();
}
