// SPDX-License-Identifier: GPL-3.0-or-later
//! Where new windows go.
//!
//! Windows used to all map at the origin, stacking exactly on top of each other and
//! sitting underneath the toolchest. Instead they are cascaded within the output's work
//! area: the output minus any space reserved by layer-shell clients (panels, docks) and
//! clear of the wlRIX shell components.

use smithay::{
    desktop::{Space, Window, layer_map_for_output},
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::shell_rules::ShellComponent;

/// Diagonal offset between successive windows, and how many steps before restarting.
const CASCADE_STEP: i32 = 32;
const CASCADE_WRAP: i32 = 8;

/// Marker recording that a window has been given its initial position, so later
/// commits do not yank it back from wherever the user moved it.
pub struct Placed;

/// The area of `output` available to ordinary windows.
///
/// `non_exclusive_zone` already subtracts what layer-shell clients reserved; it is
/// output-relative, so it is offset into the space's coordinates here.
pub fn work_area(space: &Space<Window>, output: &Output) -> Rectangle<i32, Logical> {
    let output_geometry = space
        .output_geometry(output)
        .unwrap_or_else(|| Rectangle::from_size((0, 0).into()));

    let mut area = layer_map_for_output(output).non_exclusive_zone();
    area.loc += output_geometry.loc;
    area
}

/// Pick a position for a newly mapped window of `size`.
pub fn place_new_window(
    space: &Space<Window>,
    output: &Output,
    new_window: &Window,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let area = work_area(space, output);

    // Start at the top-left of the work area, but drop below any shell component
    // covering that corner (the toolchest) so windows do not open underneath it.
    let mut origin = area.loc;
    for window in space.elements() {
        if window == new_window || window.user_data().get::<ShellComponent>().is_none() {
            continue;
        }
        let Some(geometry) = space.element_geometry(window) else {
            continue;
        };
        if geometry.contains(origin) {
            origin.y = geometry.loc.y + geometry.size.h;
        }
    }

    // Cascade by how many ordinary windows are already up.
    let placed = space
        .elements()
        .filter(|window| {
            *window != new_window && window.user_data().get::<ShellComponent>().is_none()
        })
        .count() as i32;
    let offset = CASCADE_STEP * (placed % CASCADE_WRAP);
    let mut position = origin + Point::from((offset, offset));

    // Keep it on screen: never past the far edge, never before the work area.
    let max_x = area.loc.x + (area.size.w - size.w).max(0);
    let max_y = area.loc.y + (area.size.h - size.h).max(0);
    position.x = position.x.clamp(area.loc.x, max_x.max(area.loc.x));
    position.y = position.y.clamp(area.loc.y, max_y.max(area.loc.y));

    position
}

/// Place `window` if it has not been placed yet. Called once its size is known.
pub fn place_if_new(space: &mut Space<Window>, window: &Window) {
    if window.user_data().get::<Placed>().is_some()
        || window.user_data().get::<ShellComponent>().is_some()
    {
        return;
    }

    // Prefer the output the window already landed on, else the first one.
    let Some(output) = space
        .outputs_for_element(window)
        .first()
        .cloned()
        .or_else(|| space.outputs().next().cloned())
    else {
        return;
    };

    // A window has no size until it has drawn. Placing now would clamp against a
    // zero-sized window, so leave it (undrawn, hence invisible) and try again on the
    // next commit.
    let size = window.geometry().size;
    if size.w <= 0 || size.h <= 0 {
        return;
    }

    let position = place_new_window(space, &output, window, size);

    tracing::debug!(?position, ?size, "placing new window");
    space.map_element(window.clone(), position, false);
    window.user_data().insert_if_missing(|| Placed);
}
