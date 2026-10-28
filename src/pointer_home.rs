// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the pointer starts: the middle of the first monitor.
//!
//! IRIX centered the cursor when X came up under the login screen, and KDE puts it back in the
//! middle at login. Left alone, the pointer starts at the desktop's origin -- the top-left corner
//! -- which is where smithay's seat puts it and where nobody is looking.
//!
//! The login screen and the session are each their own compositor, so centering when a
//! compositor starts does both: the pointer is in the middle of the login screen, as on IRIX,
//! and back in the middle once the desktop comes up, as on KDE.
//!
//! The first monitor is the one at the layout's origin -- the left-most of the default
//! left-to-right row, or whichever one a saved layout puts there -- since monitors connect one
//! at a time at startup and the first to arrive need not be the first in the row. Each one that
//! arrives re-centers, so the pointer ends on the right one whatever the order, until the user
//! has moved it; from then on a monitor plugged in leaves the pointer alone.

use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle};

use crate::state::Wlrix;

/// How far the pointer has got from where the session put it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PointerHome {
    /// No monitor yet, so nowhere to put it.
    #[default]
    Unplaced,
    /// Centered here, and not moved since.
    Centered(Point<f64, Logical>),
    /// The user has moved it, and it is theirs.
    Moved,
}

/// The middle of the first monitor, given every monitor's place in the layout. `None` with no
/// monitors at all.
pub fn home(outputs: &[Rectangle<i32, Logical>]) -> Option<Point<f64, Logical>> {
    let first = outputs
        .iter()
        .find(|geometry| geometry.contains((0, 0)))
        .or_else(|| outputs.first())?;
    Some(Point::from((
        first.loc.x as f64 + first.size.w as f64 / 2.0,
        first.loc.y as f64 + first.size.h as f64 / 2.0,
    )))
}

impl Wlrix {
    /// Put the pointer in the middle of the first monitor, unless the user has already moved
    /// it. Called by the backends each time a monitor is mapped.
    pub fn center_pointer_at_start(&mut self) {
        match self.pointer_home {
            PointerHome::Moved => return,
            PointerHome::Centered(at) if self.pointer_location() != at => {
                self.pointer_home = PointerHome::Moved;
                return;
            }
            PointerHome::Unplaced | PointerHome::Centered(_) => {}
        }
        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .collect();
        let Some(target) = home(&outputs) else {
            return;
        };
        let time = Duration::from(self.clock.now()).as_millis() as u32;
        self.move_pointer_to(target, time);
        self.pointer_home = PointerHome::Centered(self.pointer_location());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn one_monitor_is_centered() {
        assert_eq!(
            home(&[rect(0, 0, 2560, 1440)]),
            Some(Point::from((1280.0, 720.0)))
        );
    }

    #[test]
    fn the_monitor_at_the_origin_wins_over_the_first_to_arrive() {
        // The right-hand monitor connected first; the pointer still goes to the left one.
        let outputs = [rect(2560, 0, 1920, 1080), rect(0, 0, 2560, 1440)];
        assert_eq!(home(&outputs), Some(Point::from((1280.0, 720.0))));
    }

    #[test]
    fn a_layout_with_nothing_at_the_origin_uses_the_first() {
        let outputs = [rect(-1920, 200, 1920, 1080), rect(100, 0, 1280, 800)];
        assert_eq!(home(&outputs), Some(Point::from((-960.0, 740.0))));
    }

    #[test]
    fn no_monitors_is_nowhere() {
        assert_eq!(home(&[]), None);
    }
}
