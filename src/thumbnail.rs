// SPDX-License-Identifier: GPL-3.0-or-later
//! Snapshots of minimized windows for their desktop icons.
//!
//! A minimized window shows a small picture of its last contents in its icon tile (see
//! [`crate::minimized`]). Capturing that picture needs the renderer -- to draw the window's
//! surface tree, scaled down, into an offscreen texture and read it back -- and the renderer
//! only exists in the backend. So [`crate::window_ops::minimize_window`] just flags the window,
//! and the backend calls [`capture_pending`] on its next draw, the same way screen capture is
//! serviced (see [`crate::screencopy`]).
//!
//! The snapshot is stored as a [`MemoryRenderBuffer`] -- CPU-side and renderer-agnostic, like the
//! title text -- so the shared, generic render path can composite it without knowing the backend.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::{AsRenderElements, memory::MemoryRenderBuffer},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    output::Output,
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform},
};

use crate::{Wlrix, decoration, desks, render::OutputElem};

impl Wlrix {
    /// Take a snapshot for every minimized window still waiting for one. Called from the backend
    /// render loop, where the `GlesRenderer` lives, right after servicing screen capture.
    pub fn capture_pending_thumbnails(&mut self, renderer: &mut GlesRenderer, output: &Output) {
        // Icons only appear on the primary output's grid, so capture at its scale.
        if self.space.outputs().next() != Some(output) {
            return;
        }
        let scale = output.current_scale().fractional_scale();
        let logical = decoration::icon_thumbnail_size();
        let target = Size::<i32, Physical>::from((
            (logical.w as f64 * scale).round() as i32,
            (logical.h as f64 * scale).round() as i32,
        ));

        let pending: Vec<Window> = self
            .desks
            .hidden()
            .iter()
            .filter(|window| {
                let state = desks::window_state(window).borrow();
                state.minimized && state.needs_thumbnail
            })
            .cloned()
            .collect();

        for window in pending {
            let snapshot = snapshot(renderer, self.palette, &window, target);
            let mut state = desks::window_state(&window).borrow_mut();
            state.thumbnail = snapshot;
            state.needs_thumbnail = false;
        }
    }
}

/// Render `window`'s current content into a `target`-sized buffer, scaled to fit while keeping
/// its aspect ratio and letterboxed against the tile backdrop. `None` if the window has no size
/// or the offscreen render fails -- the icon then just shows the plain backdrop.
fn snapshot(
    renderer: &mut GlesRenderer,
    palette: &wlrix_ui::palette::Palette,
    window: &Window,
    target: Size<i32, Physical>,
) -> Option<MemoryRenderBuffer> {
    let geometry = window.geometry();
    if geometry.size.w <= 0 || geometry.size.h <= 0 || target.w <= 0 || target.h <= 0 {
        return None;
    }

    // Largest scale that fits the window inside the target, then center it.
    let fit =
        (target.w as f64 / geometry.size.w as f64).min(target.h as f64 / geometry.size.h as f64);
    let scaled = Size::<i32, Physical>::from((
        (geometry.size.w as f64 * fit).round() as i32,
        (geometry.size.h as f64 * fit).round() as i32,
    ));
    let offset =
        Point::<i32, Physical>::from(((target.w - scaled.w) / 2, (target.h - scaled.h) / 2));
    // `render_elements` places the surface origin; shift so the window's geometry origin (not the
    // surface's, which can differ for a client keeping a CSD margin) lands at `offset`.
    let render_loc = offset - geometry.loc.to_physical_precise_round(Scale::from(fit));

    let elements: Vec<OutputElem<GlesRenderer>> = window
        .render_elements::<OutputElem<GlesRenderer>>(renderer, render_loc, Scale::from(fit), 1.0);

    let buffer_size: Size<i32, BufferCoord> = (target.w, target.h).into();
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
    // Render upright regardless of backend: this is a plain offscreen texture, not the display
    // surface, so the winit output's `Flipped180` (its GL surface is upside down) does not apply.
    let mut damage = OutputDamageTracker::new(target, 1.0, Transform::Normal);
    let mut framebuffer = renderer.bind(&mut texture).ok()?;
    damage
        .render_output(
            renderer,
            &mut framebuffer,
            0,
            &elements,
            decoration::icon_well(palette),
        )
        .ok()?;

    // Read back as `Xrgb8888`, the format `screencopy` reads (proven to come out upright): the
    // snapshot is opaque, so dropping alpha costs nothing. `MemoryRenderBuffer` then interprets
    // the same format when it re-imports the pixels.
    let read_back: Rectangle<i32, BufferCoord> = Rectangle::from_size(buffer_size);
    let mapping = renderer
        .copy_framebuffer(&framebuffer, read_back, Fourcc::Xrgb8888)
        .ok()?;
    let pixels = renderer.map_texture(&mapping).ok()?;
    Some(MemoryRenderBuffer::from_slice(
        pixels,
        Fourcc::Xrgb8888,
        (target.w, target.h),
        1,
        Transform::Normal,
        None,
    ))
}
