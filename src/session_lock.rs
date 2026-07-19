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
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::session_lock::{LockSurface, SessionLocker},
};

use crate::{
    Wlrix,
    render::{OutputElem, OutputElement},
};

/// The color an output shows while locked with nothing to draw on it.
const LOCKED_BACKGROUND: smithay::backend::renderer::Color32F =
    smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 1.0);

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
}

impl LockState {
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Begin locking. The confirmation is held until a locked frame has been drawn.
    pub fn begin(&mut self, confirmation: SessionLocker) {
        self.locked = true;
        self.pending = Some(confirmation);
    }

    /// Confirm the lock, once the caller has drawn a frame in the locked state.
    ///
    /// Dropping a [`SessionLocker`] without calling `lock` reports failure to the
    /// client, so this must not be reached before the screen is actually covered.
    pub fn confirm(&mut self) {
        if let Some(confirmation) = self.pending.take() {
            confirmation.lock();
        }
    }

    pub fn add_surface(&mut self, output: &Output, surface: LockSurface) {
        // The locker draws to the size we give it, so it is told the output's size in
        // logical pixels before it paints anything.
        let size = output
            .current_mode()
            .map(|mode| mode.size)
            .unwrap_or_default();
        let scale = output.current_scale().integer_scale();
        surface.with_pending_state(|state| {
            state.size = Some(
                (
                    (size.w / scale).max(0) as u32,
                    (size.h / scale).max(0) as u32,
                )
                    .into(),
            );
        });
        surface.send_configure();
        self.surfaces.insert(output.name(), surface);
    }

    /// Forget everything: the session is unlocked, or the locker is gone.
    pub fn clear(&mut self) {
        self.locked = false;
        self.surfaces.clear();
        self.pending = None;
    }

    fn surface_for(&self, output: &Output) -> Option<&LockSurface> {
        self.surfaces.get(&output.name())
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
    let mut buffer = SolidColorBuffer::new(size, LOCKED_BACKGROUND);
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
    // The locker's surface covers its output exactly, so a hit is relative to the
    // output's own corner.
    Some((surface.wl_surface().clone(), pos - geometry.loc.to_f64()))
}

/// Move keyboard focus onto a lock surface, so typing reaches the locker and not the
/// desktop behind it.
pub fn focus_lock_surface(state: &mut Wlrix) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    // Any of them will do -- the locker is one client, and this is only about getting
    // focus off the desktop.
    let surface = state
        .lock
        .surfaces
        .values()
        .next()
        .map(|surface| surface.wl_surface().clone());
    if surface.is_some() {
        keyboard.set_focus(state, surface, SERIAL_COUNTER.next_serial());
    }
}
