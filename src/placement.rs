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
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

/// Diagonal offset between successive windows, and how many steps before restarting.
const CASCADE_STEP: i32 = 32;
const CASCADE_WRAP: i32 = 8;

/// How far the wlRIX apps sit in from their corner. IRIX leaves them slightly off the
/// edge rather than flush against it.
const CORNER_INSET: i32 = 16;

/// A corner of the work area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    BottomLeft,
}

/// Where an app opens by default. `Corner` cannot express "centered", so the two live
/// side by side rather than one bending to fit the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Corner(Corner),
    Centered,
}

/// Where a wlRIX app opens by default.
///
/// The toolchest and desks are ordinary windows -- they stack, move and close like any
/// other -- they simply have a customary place to appear. So this is a starting
/// position and nothing more: no anchoring, no stacking rules, no reserved space.
///
/// When desks (virtual desktops) arrive, these should open on the global desk so they
/// are present on all of them.
fn default_placement(app_id: &str) -> Option<Placement> {
    match app_id {
        "com.wlrix.toolchest" => Some(Placement::Corner(Corner::TopLeft)),
        "com.wlrix.desks" => Some(Placement::Corner(Corner::BottomLeft)),
        // The greeter sits in the middle of the screen, like IRIX's clogin. It is a
        // plain toplevel; this is the only thing that marks it out.
        "com.wlrix.greeter" => Some(Placement::Centered),
        _ => None,
    }
}

/// A window's `app_id`, if it has one.
pub(crate) fn app_id(window: &Window) -> Option<String> {
    let toplevel = window.toplevel()?;
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.app_id.clone()))
    })
}

/// Where a window of `size` goes for a given placement, within `area`.
fn placement_position(
    placement: Placement,
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    match placement {
        Placement::Corner(corner) => corner_position(corner, area, size),
        // Centered on this output's work area -- the pointer's monitor, not spread
        // across both -- clamped so an oversized window still starts on-screen.
        Placement::Centered => (
            area.loc.x + (area.size.w - size.w).max(0) / 2,
            area.loc.y + (area.size.h - size.h).max(0) / 2,
        )
            .into(),
    }
}

/// The position of `corner`, inset from the edges of `area`.
fn corner_position(
    corner: Corner,
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let x = area.loc.x + CORNER_INSET;
    match corner {
        Corner::TopLeft => (x, area.loc.y + CORNER_INSET).into(),
        Corner::BottomLeft => (x, area.loc.y + (area.size.h - size.h - CORNER_INSET).max(0)).into(),
    }
}

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

/// The server-side frame insets for a window: (left, top, right, bottom), or zeros when it
/// draws no frame (override-redirect X11 surfaces).
fn frame_insets(window: &Window) -> (i32, i32, i32, i32) {
    crate::frame::frame_style(window)
        .map(crate::decoration::insets)
        .unwrap_or((0, 0, 0, 0))
}

/// Pick a position for a newly mapped window of `size`.
///
/// Positions are for the client rectangle, but placement reasons about the *frame* (the client
/// plus its 4Dwm decorations) so the titlebar and borders open inside the work area rather than
/// off the top of it.
pub fn place_new_window(
    space: &Space<Window>,
    output: &Output,
    new_window: &Window,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let area = work_area(space, output);
    let (left, top, right, bottom) = frame_insets(new_window);
    let inset = Point::from((left, top));

    // A wlRIX app opens in its customary place; shift the client in by the frame so the frame's
    // top-left lands where the client used to.
    if let Some(placement) = app_id(new_window).as_deref().and_then(default_placement) {
        return placement_position(placement, area, size) + inset;
    }

    // Everything else cascades the *frame* by how many windows are already up.
    let placed = space
        .elements()
        .filter(|window| *window != new_window)
        .count() as i32;
    let offset = CASCADE_STEP * (placed % CASCADE_WRAP);
    let mut frame_pos = area.loc + Point::from((offset, offset));

    // Keep the whole frame on screen: never past the far edge, never before the work area.
    let frame_w = size.w + left + right;
    let frame_h = size.h + top + bottom;
    let max_x = area.loc.x + (area.size.w - frame_w).max(0);
    let max_y = area.loc.y + (area.size.h - frame_h).max(0);
    frame_pos.x = frame_pos.x.clamp(area.loc.x, max_x.max(area.loc.x));
    frame_pos.y = frame_pos.y.clamp(area.loc.y, max_y.max(area.loc.y));

    // The client sits inside its frame.
    frame_pos + inset
}

/// The output the pointer is on, falling back to the first available one.
fn output_for_pointer(space: &Space<Window>, pointer: Point<f64, Logical>) -> Option<Output> {
    space
        .output_under(pointer)
        .next()
        .cloned()
        .or_else(|| space.outputs().next().cloned())
}

/// Move windows that no longer sit on any output back onto one.
///
/// Unplugging a monitor leaves its windows at coordinates nothing covers any more, so
/// they would be stranded off-screen with no way to reach them.
pub fn relocate_orphaned_windows(space: &mut Space<Window>, pointer: Point<f64, Logical>) {
    let Some(output) = output_for_pointer(space, pointer) else {
        // No outputs left at all; nothing to move them onto.
        return;
    };

    let orphaned: Vec<Window> = space
        .elements()
        .filter(|window| space.outputs_for_element(window).is_empty())
        .cloned()
        .collect();

    for window in orphaned {
        let size = window.geometry().size;
        let position = place_new_window(space, &output, &window, size);
        tracing::info!(?position, "moving window off a disconnected output");
        space.map_element(window, position, false);
    }
}

/// Keep the pointer on a monitor.
///
/// Relative motion accumulates freely, so without this the cursor would wander off
/// into space that no output covers and become unreachable.
pub fn clamp_to_outputs(
    space: &Space<Window>,
    position: Point<f64, Logical>,
) -> Point<f64, Logical> {
    let geometries: Vec<Rectangle<i32, Logical>> = space
        .outputs()
        .filter_map(|output| space.output_geometry(output))
        .collect();

    let Some(&first) = geometries.first() else {
        return position;
    };

    // On a monitor already: leave it be, so the pointer crosses freely between them.
    if geometries
        .iter()
        .any(|geometry| geometry.to_f64().contains(position))
    {
        return position;
    }

    // Otherwise pull it back onto whichever output is nearest.
    let nearest = geometries
        .iter()
        .min_by(|a, b| {
            let distance = |geometry: &Rectangle<i32, Logical>| {
                let centre = geometry.to_f64().loc
                    + Point::from((geometry.size.w as f64 / 2.0, geometry.size.h as f64 / 2.0));
                (centre.x - position.x).powi(2) + (centre.y - position.y).powi(2)
            };
            distance(a).total_cmp(&distance(b))
        })
        .copied()
        .unwrap_or(first);

    Point::from((
        position.x.clamp(
            nearest.loc.x as f64,
            (nearest.loc.x + nearest.size.w - 1) as f64,
        ),
        position.y.clamp(
            nearest.loc.y as f64,
            (nearest.loc.y + nearest.size.h - 1) as f64,
        ),
    ))
}

/// Place `window` if it has not been placed yet. Called once its size is known.
///
/// `pointer` decides which monitor it opens on: windows should appear where the user
/// is looking, not always on whichever output happens to be first.
/// Returns whether the window was placed, so the caller can focus it exactly once.
pub fn place_if_new(
    space: &mut Space<Window>,
    window: &Window,
    pointer: Point<f64, Logical>,
) -> bool {
    if window.user_data().get::<Placed>().is_some() {
        return false;
    }

    let Some(output) = output_for_pointer(space, pointer) else {
        return false;
    };

    // A window has no size until it has drawn. Placing now would clamp against a
    // zero-sized window, so leave it (undrawn, hence invisible) and try again on the
    // next commit.
    let size = window.geometry().size;
    if size.w <= 0 || size.h <= 0 {
        return false;
    }

    place_now(space, window, &output, size);
    true
}

/// Place a window whose size is already known.
///
/// X11 windows report their size when they ask to be mapped, and nothing calls back
/// later to retry, so they are placed straight away rather than waiting for a commit.
pub fn place_now(
    space: &mut Space<Window>,
    window: &Window,
    output: &Output,
    size: Size<i32, Logical>,
) {
    if window.user_data().get::<Placed>().is_some() {
        return;
    }

    let position = place_new_window(space, output, window, size);
    tracing::debug!(?position, ?size, "placing new window");
    space.map_element(window.clone(), position, false);
    window.user_data().insert_if_missing(|| Placed);
}

/// The output a new window should open on.
pub fn output_for_new_window(
    space: &Space<Window>,
    pointer: Point<f64, Logical>,
) -> Option<Output> {
    output_for_pointer(space, pointer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        output::{Mode, Output, PhysicalProperties, Subpixel},
        utils::Transform,
    };

    /// An output of `size`, as the udev backend would build it.
    fn test_output(name: &str, size: (i32, i32)) -> Output {
        let output = Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "wlRIX".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            Some(Transform::Normal),
            None,
            None,
        );
        output
    }

    /// Two 1440p monitors side by side, as on the development machine.
    fn dual_head() -> (Space<Window>, Output, Output) {
        let mut space: Space<Window> = Space::default();
        let left = test_output("left", (2560, 1440));
        let right = test_output("right", (2560, 1440));
        space.map_output(&left, (0, 0));
        space.map_output(&right, (2560, 0));
        (space, left, right)
    }

    #[test]
    fn window_opens_on_the_monitor_the_pointer_is_on() {
        let (space, _left, _right) = dual_head();

        let on_left = output_for_pointer(&space, (100.0, 100.0).into()).unwrap();
        assert_eq!(on_left.name(), "left");

        let on_right = output_for_pointer(&space, (3000.0, 200.0).into()).unwrap();
        assert_eq!(on_right.name(), "right");
    }

    #[test]
    fn pointer_outside_every_output_still_picks_one() {
        let (space, _left, _right) = dual_head();
        // Can happen between an output going away and the pointer being clamped.
        assert!(output_for_pointer(&space, (99_999.0, 99_999.0).into()).is_some());
    }

    #[test]
    fn no_outputs_means_nowhere_to_place() {
        let space: Space<Window> = Space::default();
        assert!(output_for_pointer(&space, (0.0, 0.0).into()).is_none());
    }

    #[test]
    fn pointer_moves_freely_between_adjacent_monitors() {
        let (space, _left, _right) = dual_head();
        // Crossing the seam must not be clamped: both sides are on an output.
        let just_left = Point::from((2559.0, 700.0));
        let just_right = Point::from((2561.0, 700.0));
        assert_eq!(clamp_to_outputs(&space, just_left), just_left);
        assert_eq!(clamp_to_outputs(&space, just_right), just_right);
    }

    #[test]
    fn pointer_cannot_wander_off_the_far_edge() {
        let (space, _left, _right) = dual_head();
        // Relative motion accumulates, so this is reachable by just moving right.
        let escaped = clamp_to_outputs(&space, (99_999.0, 700.0).into());
        assert_eq!(escaped.x, 5119.0); // right edge of the right-hand monitor
        assert_eq!(escaped.y, 700.0);

        let above = clamp_to_outputs(&space, (100.0, -500.0).into());
        assert_eq!(above.y, 0.0);
        assert_eq!(above.x, 100.0);
    }

    #[test]
    fn clamping_without_outputs_is_a_no_op() {
        let space: Space<Window> = Space::default();
        let anywhere = Point::from((42.0, 42.0));
        assert_eq!(clamp_to_outputs(&space, anywhere), anywhere);
    }

    #[test]
    fn work_area_covers_the_output_when_nothing_is_reserved() {
        let (space, _left, right) = dual_head();
        // No layer-shell clients, so the whole of the right-hand output is usable,
        // offset into space coordinates rather than starting at the origin.
        let area = work_area(&space, &right);
        assert_eq!(area.loc.x, 2560);
        assert_eq!(area.size.w, 2560);
        assert_eq!(area.size.h, 1440);
    }

    #[test]
    fn the_greeter_is_recognised_and_centered() {
        assert_eq!(
            default_placement("com.wlrix.greeter"),
            Some(Placement::Centered)
        );
    }

    #[test]
    fn centring_lands_in_the_middle_of_its_own_monitor() {
        let (space, _left, right) = dual_head();
        // The right-hand output, so the result must sit within its work area and not
        // straddle the seam or center across the whole desktop.
        let area = work_area(&space, &right);
        let size = Size::from((800, 600));
        let pos = placement_position(Placement::Centered, area, size);
        // Middle of a 2560x1440 area at x-offset 2560: (2560 + (2560-800)/2, (1440-600)/2).
        assert_eq!(pos.x, 2560 + 880);
        assert_eq!(pos.y, 420);
    }

    #[test]
    fn an_oversized_greeter_still_starts_on_screen() {
        let (space, left, _right) = dual_head();
        let area = work_area(&space, &left);
        // Taller and wider than the monitor: centring must not push the top-left
        // corner off-screen, or the login field could be unreachable.
        let pos = placement_position(Placement::Centered, area, (4000, 2000).into());
        assert_eq!(pos, area.loc);
    }
}
