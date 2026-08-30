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
    wayland::{compositor::with_states, seat::WaylandFocus, shell::xdg::XdgToplevelSurfaceData},
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

/// Where a window opens. `Corner` cannot express "centered", so the two live side by side
/// rather than one bending to fit the other.
///
/// Everything here is in **frame** coordinates -- the client plus its 4Dwm decorations -- and
/// every variant is clamped into the work area afterwards, so no arm has to defend against
/// falling off an edge on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Corner(Corner),
    Centered,
    /// Centered on another window's frame: a dialog over the window that opened it.
    CenteredOn(Rectangle<i32, Logical>),
    /// Exactly here, because the window asked for it.
    Frame(Point<i32, Logical>),
    /// Diagonally offset by how many windows are already up. What a window that has asked
    /// for nothing gets.
    Cascade(i32),
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

/// The frame a wlRIX shell app gets, when it is not framed like an ordinary window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellFrame {
    /// No frame at all, so the user cannot move, resize, minimize or maximize it. The greeter
    /// must not be dismissable or draggable.
    Bare,
    /// A titlebar and nothing else: no border, and none of the three buttons.
    ///
    /// What IRIX's toolchest had. The titlebar is not decoration for its own sake -- it is what
    /// makes the panel movable and gives it a window menu, and it is why the client needs no
    /// chrome of its own. Everything else is dropped: a border would be a resize grip on a
    /// panel that does not resize, and the buttons do things a toolchest does not do.
    TitlebarOnly,
}

/// How a wlRIX shell app should be framed, or `None` for an ordinary window.
///
/// The Desks overview is an ordinary framed window and is not listed. `wlrix-desktop` is not
/// here either, and never will be: the desktop icons are a **layer-shell background surface**,
/// not a window, so there is no frame to suppress.
pub(crate) fn shell_frame(app_id: &str) -> Option<ShellFrame> {
    match app_id {
        "com.wlrix.toolchest" => Some(ShellFrame::TitlebarOnly),
        "com.wlrix.greeter" => Some(ShellFrame::Bare),
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

/// Where a frame of `size` goes for a given placement, within `area`.
///
/// The result is not yet on-screen-safe: [`clamp_frame`] is what guarantees that, for every
/// arm at once.
fn frame_position(
    placement: Placement,
    area: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    match placement {
        Placement::Corner(corner) => corner_position(corner, area, size),
        // Centered on this output's work area -- the pointer's monitor, not spread
        // across both -- clamped so an oversized window still starts on-screen.
        Placement::Centered => center_of(area, size),
        // Centered on the parent window instead of on the monitor. The parent is passed as a
        // rectangle rather than looked up here so the arithmetic stays testable without a
        // `Space` to hang two real windows off.
        Placement::CenteredOn(parent) => center_of(parent, size),
        Placement::Frame(position) => position,
        Placement::Cascade(depth) => {
            let offset = CASCADE_STEP * (depth % CASCADE_WRAP);
            area.loc + Point::from((offset, offset))
        }
    }
}

/// The position that centers a frame of `size` within `within`.
///
/// `max(0)` rather than a signed halving: a window larger than what it is being centered in
/// would otherwise get a negative offset and start off the top-left corner, which for the
/// greeter means a login field nothing can reach.
fn center_of(within: Rectangle<i32, Logical>, size: Size<i32, Logical>) -> Point<i32, Logical> {
    (
        within.loc.x + (within.size.w - size.w).max(0) / 2,
        within.loc.y + (within.size.h - size.h).max(0) / 2,
    )
        .into()
}

/// Pull a frame back inside the work area: never past the far edge, never before the near one.
///
/// Applied to every placement, not just the cascade, because the client-driven ones are the
/// least trustworthy of the lot -- an X11 window asking for the coordinates it had on a monitor
/// that is no longer plugged in, a dialog whose parent straddles an edge -- and a window that
/// opens where it cannot be reached is worse than one that opens somewhere unexpected.
///
/// A frame bigger than the work area clamps to the near edge: `max_*` would otherwise fall
/// below `area.loc` and invert the range.
fn clamp_frame(
    position: Point<i32, Logical>,
    size: Size<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    let max_x = area.loc.x + (area.size.w - size.w).max(0);
    let max_y = area.loc.y + (area.size.h - size.h).max(0);
    Point::from((
        position.x.clamp(area.loc.x, max_x.max(area.loc.x)),
        position.y.clamp(area.loc.y, max_y.max(area.loc.y)),
    ))
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

/// Whether `window` has had its initial position chosen yet.
///
/// Before that it is in the space at the origin with no size, so anything derived from where it
/// *is* -- which monitor it is on, most of all -- is asking about a placeholder.
pub fn is_placed(window: &Window) -> bool {
    window.user_data().get::<Placed>().is_some()
}

/// The area of `output` available to ordinary windows.
///
/// `non_exclusive_zone` already subtracts what layer-shell clients reserved; it is
/// output-relative, so it is offset into the space's coordinates here.
///
/// Locks the output's layer map (`layer_map_for_output`). Do **not** call this while already
/// holding that guard for the same output -- it is a non-reentrant mutex, so a second lock on
/// the same thread deadlocks. `render::output_elements` holds it for its whole body and so
/// derives its work area from the guard directly rather than calling this.
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

/// The window `window` is a dialog of, if it named one and that window is on screen.
///
/// The two shells say it differently and neither is reachable from the other: a Wayland
/// toplevel names its parent surface through `xdg_toplevel.set_parent`, an X11 window names a
/// window id through `WM_TRANSIENT_FOR`.
///
/// A parent that is not in the space -- on another desk, or minimized -- answers `None`, and
/// the dialog falls back to the cascade. Centering on a window that is not being shown would
/// put the dialog somewhere with nothing to explain it.
fn parent_of(space: &Space<Window>, window: &Window) -> Option<Window> {
    if let Some(toplevel) = window.toplevel() {
        let parent = toplevel.parent()?;
        return space
            .elements()
            .find(|candidate| candidate.wl_surface().as_deref() == Some(&parent))
            .cloned();
    }

    let parent = window.x11_surface()?.is_transient_for()?;
    space
        .elements()
        .find(|candidate| {
            candidate
                .x11_surface()
                .is_some_and(|surface| surface.window_id() == parent)
        })
        .cloned()
}

/// A window's frame rectangle -- what it occupies on the desktop, decorations included.
fn frame_rect(space: &Space<Window>, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let client = space.element_location(window)?;
    let size = window.geometry().size;
    let (left, top, right, bottom) = frame_insets(window);
    Some(Rectangle::new(
        client - Point::from((left, top)),
        Size::from((size.w + left + right, size.h + top + bottom)),
    ))
}

/// Where an X11 client asked to be put, if it asked at all.
///
/// This is the one thing Wayland cannot express and X11 can. `WM_NORMAL_HINTS` carries a
/// position field, and a window manager is expected to honor it -- it is how a client restores
/// a window to where the user last left it, and how a game opens on the monitor it was told to.
///
/// Two details make reading it less obvious than it looks:
///
/// - **The flag is the signal, not the numbers.** ICCCM deprecated the `x`/`y` inside the hints
///   themselves; every current toolkit sets the flag and puts the real coordinates on the
///   window, which reaches us as [`X11Surface::last_configure`]. `geometry()` is no use here --
///   its origin is always (0, 0), the trap `mapped_override_redirect_window` documents.
/// - **`ProgramSpecified` counts too.** Honoring only `UserSpecified` would honor almost
///   nothing: that flag means an explicit `-geometry` on the command line, while every toolkit
///   that moves a window before mapping it sets the program flag instead. The cost is that a
///   toolkit which sets the flag without meaning anything by it is taken at its word, so a
///   position of exactly (0, 0) -- what a window that never moved reports -- is read as "no
///   preference" rather than as a request for the corner.
fn requested_position(window: &Window) -> Option<Point<i32, Logical>> {
    let surface = window.x11_surface()?;
    // The presence of the field is what is being tested; its contents are the deprecated half.
    surface.size_hints()?.position?;
    let position = surface.last_configure().loc;
    (position.x != 0 || position.y != 0).then_some(position)
}

/// How a newly mapped window of `frame_size` should be positioned within `area`.
///
/// Resolved most-specific-first, and the order is the policy:
///
/// 1. **The wlRIX shell apps**, which open where IRIX put them whatever they ask for. These are
///    the desktop's own furniture and none of them asks for anything, so this costs nothing --
///    it is here so that a wlRIX app could never talk itself out of its customary corner.
/// 2. **Already maximized**, which a client may be before it has drawn a single frame: it
///    called `set_maximized` before its first commit. Its size is already the work area, so the
///    frame belongs at the work area's origin -- cascading it would push a window sized to fill
///    the screen partly off the screen.
/// 3. **A position the client asked for**, which only an X11 client can do.
/// 4. **A dialog**, which opens centered over the window that opened it. This is what
///    `WindowStartupLocation="CenterOwner"` amounts to for a toolkit that cannot set its own
///    position, and it is what Motif and 4Dwm did with transients regardless.
/// 5. **Everything else**, cascaded.
fn placement_for(
    space: &Space<Window>,
    window: &Window,
    inset: Point<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Placement {
    if let Some(placement) = app_id(window).as_deref().and_then(default_placement) {
        return placement;
    }

    if crate::desks::window_state(window).borrow().maximized {
        return Placement::Frame(area.loc);
    }

    // An X11 client names where its *client* rectangle should go; the frame hangs off the top
    // and left of that, so back out the inset to get the frame position the rest of this works
    // in. The clamp then keeps the titlebar on screen for a window that asked for y = 0.
    if let Some(position) = requested_position(window) {
        return Placement::Frame(position - inset);
    }

    if let Some(parent) = parent_of(space, window).and_then(|parent| frame_rect(space, &parent)) {
        return Placement::CenteredOn(parent);
    }

    Placement::Cascade(
        space
            .elements()
            .filter(|candidate| *candidate != window)
            .count() as i32,
    )
}

/// Pick a position for a newly mapped window of `size`.
///
/// Positions are for the client rectangle, but placement reasons about the *frame* (the client
/// plus its 4Dwm decorations) so the titlebar and borders open inside the work area rather than
/// off the top of it. So the frame is what gets positioned and clamped, and the inset is added
/// back at the end to name the client.
pub fn place_new_window(
    space: &Space<Window>,
    output: &Output,
    new_window: &Window,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    // Fullscreen is measured against the whole output and clamped to nothing, because
    // covering the panels is the point of it. Everything below works in the work area, so this
    // cannot be an arm of `placement_for` -- it would be clamped back inside the panels.
    if crate::desks::window_state(new_window).borrow().fullscreen
        && let Some(output_geometry) = space.output_geometry(output)
    {
        return output_geometry.loc;
    }

    let area = work_area(space, output);
    let (left, top, right, bottom) = frame_insets(new_window);
    let inset = Point::from((left, top));
    let frame_size = Size::from((size.w + left + right, size.h + top + bottom));

    let placement = placement_for(space, new_window, inset, area);
    let frame_pos = frame_position(placement, area, frame_size);

    // The client sits inside its frame.
    clamp_frame(frame_pos, frame_size, area) + inset
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

/// The rectangle every output covers between them, or `None` when there are none.
///
/// The desktop as one coordinate space, which is what an absolute pointing device has to be
/// mapped onto: a tablet reports where it is within *itself*, and the whole desktop is what that
/// fraction should span. Two monitors side by side give one rectangle twice as wide, not the
/// first of them.
///
/// A layout with a gap or a step in it has a bounding box larger than the outputs inside it, so a
/// position taken from this can land where no monitor is; [`clamp_to_outputs`] puts it back.
pub fn output_layout(space: &Space<Window>) -> Option<Rectangle<i32, Logical>> {
    let mut geometries = space
        .outputs()
        .filter_map(|output| space.output_geometry(output));
    let first = geometries.next()?;
    Some(geometries.fold(first, Rectangle::merge))
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
                let center = geometry.to_f64().loc
                    + Point::from((geometry.size.w as f64 / 2.0, geometry.size.h as f64 / 2.0));
                (center.x - position.x).powi(2) + (center.y - position.y).powi(2)
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
/// Returns whether the window was placed, so the caller can focus it exactly once.
pub fn place_if_new(
    space: &mut Space<Window>,
    window: &Window,
    pointer: Point<f64, Logical>,
) -> bool {
    if window.user_data().get::<Placed>().is_some() {
        return false;
    }

    let Some(output) = output_for_new_window(space, window, pointer) else {
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
    // Where a window goes when its desk is next activated. A window maximized before it was
    // ever mapped has already written a guess here, from the output it could see at the time;
    // this is the position it actually got.
    crate::desks::window_state(window).borrow_mut().last_pos = position;
    window.user_data().insert_if_missing(|| Placed);
}

/// The output a new window should open on: its parent's monitor, or else the pointer's.
///
/// The pointer is the general rule, because a window should appear where the user is looking
/// rather than on whichever output happens to be first.
///
/// A dialog is the exception, and it has to be one. Its placement is measured from its parent's
/// frame, which may be on the other monitor entirely; the work area it is then clamped into has
/// to be the same monitor, or the clamp drags the dialog back across the seam and off the
/// window it belongs to. Picking the output here is what keeps those two agreeing.
pub fn output_for_new_window(
    space: &Space<Window>,
    window: &Window,
    pointer: Point<f64, Logical>,
) -> Option<Output> {
    if let Some(parent) = parent_of(space, window)
        && let Some(output) = space.outputs_for_element(&parent).into_iter().next()
    {
        return Some(output);
    }
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
                serial_number: "test".into(),
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

    /// The whole desktop, not the first monitor of it. An absolute device is mapped onto this,
    /// so getting it wrong makes every monitor but one unreachable by a tablet.
    #[test]
    fn the_layout_spans_every_monitor() {
        let (space, _left, _right) = dual_head();
        let layout = output_layout(&space).expect("two outputs are mapped");
        assert_eq!(layout.loc, Point::from((0, 0)));
        assert_eq!(layout.size, Size::from((5120, 1440)));
    }

    /// A monitor placed above and to the left contributes its own corner, so the layout is not
    /// simply "the widest one wins" -- and its origin is not the origin.
    #[test]
    fn the_layout_covers_monitors_in_any_arrangement() {
        let mut space: Space<Window> = Space::default();
        let high = test_output("high", (2560, 1440));
        let low = test_output("low", (1920, 1080));
        space.map_output(&high, (0, 0));
        space.map_output(&low, (-1920, 1440));
        let layout = output_layout(&space).expect("two outputs are mapped");
        assert_eq!(layout.loc, Point::from((-1920, 0)));
        assert_eq!(layout.size, Size::from((4480, 2520)));
    }

    #[test]
    fn a_session_with_no_outputs_has_no_layout() {
        let space: Space<Window> = Space::default();
        assert!(output_layout(&space).is_none());
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
    fn the_greeter_is_recognized_and_centered() {
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
        let pos = frame_position(Placement::Centered, area, size);
        // Middle of a 2560x1440 area at x-offset 2560: (2560 + (2560-800)/2, (1440-600)/2).
        assert_eq!(pos.x, 2560 + 880);
        assert_eq!(pos.y, 420);
    }

    #[test]
    fn an_oversized_greeter_still_starts_on_screen() {
        let (space, left, _right) = dual_head();
        let area = work_area(&space, &left);
        // Taller and wider than the monitor: centering must not push the top-left
        // corner off-screen, or the login field could be unreachable.
        let pos = frame_position(Placement::Centered, area, (4000, 2000).into());
        assert_eq!(pos, area.loc);
    }

    /// A representative 4Dwm frame: a titlebar on top, a border all round. The exact numbers
    /// are `decoration::insets`' business; what matters to placement is that the top inset is
    /// the big one, because that is the edge a self-positioning window falls off.
    const INSET: Point<i32, Logical> = Point::new(3, 24);

    /// What `placement_for` does with a position an X11 client asked for: the client names
    /// where its own rectangle goes, so the frame starts that far above and left of it.
    fn as_asked(
        client: (i32, i32),
        area: Rectangle<i32, Logical>,
        size: (i32, i32),
    ) -> Point<i32, Logical> {
        let frame_size = Size::from((size.0 + INSET.x * 2, size.1 + INSET.y + INSET.x));
        let position = frame_position(
            Placement::Frame(Point::from(client) - INSET),
            area,
            frame_size,
        );
        clamp_frame(position, frame_size, area) + INSET
    }

    #[test]
    fn a_dialog_opens_centered_on_the_window_that_opened_it() {
        // `WindowStartupLocation="CenterOwner"`, arrived at from the compositor's side: the
        // client never says it, and never has to.
        let parent = Rectangle::new(Point::from((400, 300)), Size::from((1000, 800)));
        let pos = frame_position(
            Placement::CenteredOn(parent),
            first_area(),
            (400, 200).into(),
        );
        assert_eq!(pos.x, 400 + 300);
        assert_eq!(pos.y, 300 + 300);
    }

    /// The multi-head half of the same rule, and the reason `output_for_new_window` consults the
    /// parent before the pointer. Centering is measured from the parent's frame, so a dialog for
    /// a window on the right-hand monitor lands on the right-hand monitor -- and the work area
    /// it is then clamped into has to be that one, or the clamp would drag it back over the seam
    /// and off the window it belongs to.
    #[test]
    fn a_dialog_follows_its_parent_to_the_second_monitor() {
        let (space, _left, right) = dual_head();
        let area = work_area(&space, &right);
        let parent = Rectangle::new(Point::from((2560 + 200, 100)), Size::from((1200, 900)));
        let size = Size::from((500, 300));

        let pos = clamp_frame(
            frame_position(Placement::CenteredOn(parent), area, size),
            size,
            area,
        );
        assert!(
            pos.x >= 2560,
            "the dialog belongs on its parent's monitor, got {pos:?}"
        );
        assert_eq!(pos.x, 2560 + 200 + 350);
        assert_eq!(pos.y, 100 + 300);
    }

    /// Clamped against the work area, not against the parent: a dialog bigger than the window
    /// that opened it centers to a negative offset, and would start off the top-left corner.
    #[test]
    fn a_dialog_larger_than_its_parent_still_starts_on_screen() {
        let area = first_area();
        let parent = Rectangle::new(Point::from((40, 40)), Size::from((300, 200)));
        let size = Size::from((900, 700));
        let pos = clamp_frame(
            frame_position(Placement::CenteredOn(parent), area, size),
            size,
            area,
        );
        assert_eq!(pos, Point::from((40, 40)));
    }

    /// The whole point of reading `WM_NORMAL_HINTS`: a window that names a spot is put there
    /// rather than dropped into the cascade.
    #[test]
    fn an_x11_window_that_asks_for_a_position_gets_it() {
        assert_eq!(
            as_asked((900, 500), first_area(), (640, 480)),
            Point::from((900, 500))
        );
    }

    /// ...but not at the cost of its titlebar. A client asking for the very top of the screen is
    /// asking for its *client* rectangle to go there, which would hang the frame off the top
    /// edge and leave the window with nothing to drag it by.
    #[test]
    fn a_window_cannot_ask_to_open_above_its_own_titlebar() {
        let pos = as_asked((900, 0), first_area(), (640, 480));
        assert_eq!(pos.x, 900, "x was reachable and should be untouched");
        assert_eq!(pos.y, INSET.y, "the titlebar has to be on screen");
    }

    /// A remembered position from a monitor that is no longer plugged in, which is the ordinary
    /// way this goes wrong rather than an exotic one.
    #[test]
    fn a_window_cannot_ask_to_open_past_the_far_edge() {
        let area = first_area();
        let pos = as_asked((9_000, 9_000), area, (640, 480));
        assert!(pos.x < area.size.w && pos.y < area.size.h, "got {pos:?}");
        // Flush against the bottom-right, frame included.
        assert_eq!(pos.x, area.size.w - 640 - INSET.x);
        assert_eq!(pos.y, area.size.h - 480 - INSET.x);
    }

    /// A client that called `set_maximized` before its first commit has already been sized to
    /// fill the work area. Cascading that would push a screen-sized window partly off screen.
    #[test]
    fn a_window_maximized_before_it_drew_opens_at_the_work_areas_origin() {
        let (space, _left, right) = dual_head();
        let area = work_area(&space, &right);
        let pos = frame_position(Placement::Frame(area.loc), area, area.size);
        assert_eq!(pos, area.loc);
        assert_eq!(clamp_frame(pos, area.size, area), area.loc);
    }

    /// The cascade restarts rather than marching off the screen -- and `CASCADE_WRAP` windows
    /// later it is back where it began, which is the behavior the clamp used to hide.
    #[test]
    fn the_cascade_wraps_round() {
        let area = first_area();
        let size = Size::from((640, 480));
        assert_eq!(frame_position(Placement::Cascade(0), area, size), area.loc);
        assert_eq!(
            frame_position(Placement::Cascade(1), area, size),
            area.loc + Point::from((CASCADE_STEP, CASCADE_STEP))
        );
        assert_eq!(
            frame_position(Placement::Cascade(CASCADE_WRAP), area, size),
            frame_position(Placement::Cascade(0), area, size)
        );
    }

    /// A single 2560x1440 work area at the origin, for the arithmetic that does not care which
    /// monitor it is on.
    fn first_area() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((0, 0)), Size::from((2560, 1440)))
    }
}

#[cfg(test)]
mod shell_frame_tests {
    use super::*;

    #[test]
    fn the_toolchest_keeps_a_titlebar_and_the_greeter_gets_nothing() {
        // The toolchest is movable and has a window menu; the greeter must be neither.
        assert_eq!(
            shell_frame("com.wlrix.toolchest"),
            Some(ShellFrame::TitlebarOnly)
        );
        assert_eq!(shell_frame("com.wlrix.greeter"), Some(ShellFrame::Bare));
    }

    #[test]
    fn everything_else_is_framed_like_an_ordinary_window() {
        // The Desks overview in particular: a wlRIX app, but a normal window.
        for app_id in ["com.wlrix.desks", "org.mozilla.firefox", "", "com.wlrix"] {
            assert_eq!(shell_frame(app_id), None, "{app_id}");
        }
    }
}
