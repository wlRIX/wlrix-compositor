// SPDX-License-Identifier: GPL-3.0-or-later
//! Letting a client hold on to the pointer.
//!
//! `zwp_pointer_constraints_v1` is how a client says the mouse belongs to it for now. There are
//! two kinds and the difference is the whole thing:
//!
//! - **Locked**: the pointer does not move, and the client is sent no motion events at all. It
//!   steers by relative deltas alone and draws its own aim wherever it likes. This is what a
//!   first-person game asks for, and what an emulator does when it captures the mouse for the
//!   machine inside it.
//! - **Confined**: the pointer still moves and the client still gets motion, but it may not
//!   leave a region of the surface.
//!
//! The protocol was already advertised and `zwp_relative_pointer_v1` was already delivering
//! deltas, which is why capturing the mouse half-worked: the client got its deltas, and the
//! cursor walked out of the window anyway, because nothing on the motion path had ever been told
//! to stop it. That is what [`constrain`] is.
//!
//! X11 clients get this too without knowing it. XWayland implements an X pointer grab with
//! `confineTo`, and XI2 raw motion, on top of these two protocols -- so an X11 game or emulator
//! capturing the mouse ends up here just the same as a Wayland one.
//!
//! ## The policy
//!
//! A constraint is the client's request and the compositor's decision, and ours is the
//! conservative one: honor a constraint only while the pointer is already over the surface that
//! asked for it, and let go the moment focus leaves (smithay sends `unlocked`/`unconfined` for
//! that on its own). A client cannot use this to take a pointer it does not already have.

use smithay::{
    input::pointer::PointerHandle,
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::{RectangleKind, RegionAttributes},
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
    },
};

use crate::Wlrix;

/// What a motion event is allowed to do to the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// Move it here. The ordinary answer, and the only one when nothing is constraining it.
    To(Point<f64, Logical>),
    /// Leave it exactly where it is, and tell the client nothing: a locked pointer receives no
    /// motion events by protocol, which is precisely what makes it useful to aim with.
    Locked,
}

impl Motion {
    /// Where the pointer ends up, `current` being where it is now.
    pub fn position(self, current: Point<f64, Logical>) -> Point<f64, Logical> {
        match self {
            Motion::To(location) => location,
            Motion::Locked => current,
        }
    }

    /// Whether the client must be told nothing about this movement.
    pub fn is_locked(self) -> bool {
        self == Motion::Locked
    }
}

/// Resolve a proposed pointer position against whatever the client under it has asked for.
///
/// `current` is where the pointer is, `proposed` where the movement would put it. Unconstrained,
/// the answer is `proposed` and nothing else happens.
pub fn constrain(
    state: &Wlrix,
    pointer: &PointerHandle<Wlrix>,
    current: Point<f64, Logical>,
    proposed: Point<f64, Logical>,
) -> Motion {
    // The ordinary case, settled as cheaply as it can be. A motion event arrives for every
    // report of the mouse -- a thousand times a second on a gaming one -- and everything below
    // this point costs a walk of the desktop, which no ordinary session should ever pay for.
    // The pointer's focus is already known, and asking whether it carries a constraint is a
    // lock and a hash lookup.
    let Some(surface) = pointer.current_focus() else {
        return Motion::To(proposed);
    };
    if with_pointer_constraint(&surface, pointer, |constraint| constraint.is_none()) {
        return Motion::To(proposed);
    }

    // Where the constrained surface is, so its region can be read in its own coordinates. If
    // the pointer is not really over it -- compositor chrome laid on top, say -- there is
    // nothing to be constrained by.
    let Some(origin) = state
        .surface_under(current)
        .filter(|(under, _)| *under == surface)
        .map(|(_, origin)| origin)
    else {
        return Motion::To(proposed);
    };
    // Only needed for a confinement that named no region; see below. Read out here rather than
    // inside the closure, which holds the constraint lock.
    let still_on_surface = state
        .surface_under(proposed)
        .is_some_and(|(under, _)| under == surface);

    with_pointer_constraint(&surface, pointer, |constraint| {
        let Some(constraint) = constraint else {
            return Motion::To(proposed);
        };
        // A region holds the pointer only while it is *inside* that region: outside, the
        // constraint is inert. That is what lets a client fence off part of its surface rather
        // than all of it -- and it is also how a constraint set up before the pointer arrived
        // becomes true, which is the moment to tell the client about it.
        if !within(constraint.region(), current - origin) {
            return Motion::To(proposed);
        }
        if !constraint.is_active() {
            constraint.activate();
        }
        match &*constraint {
            PointerConstraint::Locked(_) => Motion::Locked,
            PointerConstraint::Confined(confined) => match confined.region() {
                Some(region) => Motion::To(clamp_into(region, origin, current, proposed)),
                // No region named means the surface itself is the fence, and its extent is not
                // something this protocol hands over. So ask the desktop what is under the
                // proposed point instead: leaving the surface is refused, moving within it is
                // not.
                None if still_on_surface => Motion::To(proposed),
                None => Motion::To(current),
            },
        }
    })
}

/// Whether `point`, in surface coordinates, is inside `region`. A constraint with no region
/// covers everything it applies to, so `None` is always inside.
fn within(region: Option<&RegionAttributes>, point: Point<f64, Logical>) -> bool {
    region.is_none_or(|region| region.contains(point.to_i32_round()))
}

/// Pull `proposed` back inside `region`, which is in surface coordinates measured from `origin`.
///
/// Clamping rather than refusing, so the pointer slides along the inside of the fence instead of
/// stopping dead the moment a diagonal movement meets an edge.
///
/// A region is an arbitrary set of added and subtracted rectangles, so the clamp is to the
/// bounding box of what it adds and the result is then checked properly. A concave region the
/// pointer cannot be pulled back into keeps it where it was: sliding around the inside of an L
/// is more than a fence has to do, and standing still is at least honest.
fn clamp_into(
    region: &RegionAttributes,
    origin: Point<f64, Logical>,
    current: Point<f64, Logical>,
    proposed: Point<f64, Logical>,
) -> Point<f64, Logical> {
    let Some(bounds) = adds_up_to(region) else {
        // A region that adds nothing has no inside to be pulled back into.
        return current;
    };
    let local = proposed - origin;
    // A rectangle does not contain its far edge, so the limit is a pixel inside it.
    let clamped = Point::<f64, Logical>::from((
        local.x.clamp(
            f64::from(bounds.loc.x),
            f64::from(bounds.loc.x + bounds.size.w) - 1.0,
        ),
        local.y.clamp(
            f64::from(bounds.loc.y),
            f64::from(bounds.loc.y + bounds.size.h) - 1.0,
        ),
    ));
    if region.contains(clamped.to_i32_round()) {
        clamped + origin
    } else {
        current
    }
}

/// The bounding box of the parts of `region` that add to it, or `None` for one that adds nothing.
fn adds_up_to(region: &RegionAttributes) -> Option<Rectangle<i32, Logical>> {
    region
        .rects
        .iter()
        .filter(|(kind, _)| matches!(kind, RectangleKind::Add))
        .map(|(_, rect)| *rect)
        .reduce(Rectangle::merge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(rects: &[(RectangleKind, Rectangle<i32, Logical>)]) -> RegionAttributes {
        RegionAttributes {
            rects: rects.to_vec(),
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), (w, h).into())
    }

    const ORIGIN: Point<f64, Logical> = Point::new(100.0, 50.0);

    /// The point of a fence: a movement that would cross it stops at it, in that axis only, so
    /// dragging diagonally into an edge still slides along it.
    #[test]
    fn a_movement_out_of_the_region_slides_along_the_edge() {
        let fence = region(&[(RectangleKind::Add, rect(0, 0, 200, 100))]);
        // Heading right and down out of a 200x100 fence from inside it.
        let landed = clamp_into(
            &fence,
            ORIGIN,
            ORIGIN + Point::from((150.0, 40.0)),
            ORIGIN + Point::from((260.0, 70.0)),
        );
        // Held at the right edge, but the vertical part of the movement was allowed.
        assert_eq!(landed, ORIGIN + Point::from((199.0, 70.0)));
    }

    /// Inside the fence nothing is changed -- a confinement is not a grid.
    #[test]
    fn a_movement_inside_the_region_is_untouched() {
        let fence = region(&[(RectangleKind::Add, rect(0, 0, 200, 100))]);
        let wanted = ORIGIN + Point::from((20.0, 30.0));
        assert_eq!(clamp_into(&fence, ORIGIN, ORIGIN, wanted), wanted);
    }

    /// The region is read in the surface's coordinates, so a surface elsewhere on the desktop
    /// fences the same area of itself rather than the same area of the screen.
    #[test]
    fn the_region_moves_with_its_surface() {
        let fence = region(&[(RectangleKind::Add, rect(0, 0, 200, 100))]);
        let far = Point::<f64, Logical>::from((3000.0, 700.0));
        let landed = clamp_into(
            &fence,
            far,
            far + Point::from((10.0, 10.0)),
            far + Point::from((999.0, 10.0)),
        );
        assert_eq!(landed, far + Point::from((199.0, 10.0)));
    }

    /// A hole punched in the middle is a shape the bounding box cannot describe. Rather than
    /// letting the pointer into the hole, the movement is refused.
    #[test]
    fn a_movement_into_a_subtracted_hole_is_refused() {
        let fence = region(&[
            (RectangleKind::Add, rect(0, 0, 200, 200)),
            (RectangleKind::Subtract, rect(50, 50, 100, 100)),
        ]);
        let start = ORIGIN + Point::from((10.0, 10.0));
        let into_the_hole = ORIGIN + Point::from((100.0, 100.0));
        assert_eq!(clamp_into(&fence, ORIGIN, start, into_the_hole), start);
    }

    /// A region that adds nothing has no inside, so nothing may move within it.
    #[test]
    fn an_empty_region_pins_the_pointer() {
        let fence = region(&[(RectangleKind::Subtract, rect(0, 0, 10, 10))]);
        let start = ORIGIN + Point::from((10.0, 10.0));
        assert_eq!(clamp_into(&fence, ORIGIN, start, ORIGIN), start);
        assert!(adds_up_to(&fence).is_none());
    }

    /// Several added rectangles fence their union, not the first of them.
    #[test]
    fn the_bounding_box_covers_every_added_rectangle() {
        let fence = region(&[
            (RectangleKind::Add, rect(0, 0, 50, 50)),
            (RectangleKind::Add, rect(100, 100, 50, 50)),
        ]);
        assert_eq!(adds_up_to(&fence), Some(rect(0, 0, 150, 150)));
    }

    /// No region at all means "wherever this constraint applies", which is everywhere.
    #[test]
    fn a_constraint_without_a_region_covers_everything() {
        assert!(within(None, Point::from((-9000.0, 9000.0))));
    }

    #[test]
    fn a_locked_motion_leaves_the_pointer_alone() {
        let here = Point::<f64, Logical>::from((640.0, 480.0));
        assert_eq!(Motion::Locked.position(here), here);
        assert!(Motion::Locked.is_locked());
        let there = Point::<f64, Logical>::from((1.0, 2.0));
        assert_eq!(Motion::To(there).position(here), there);
        assert!(!Motion::To(there).is_locked());
    }
}
