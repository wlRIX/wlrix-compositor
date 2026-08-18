// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use crate::Wlrix;
use smithay::backend::input::ButtonState;
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor, shell::xdg::SurfaceCachedState},
};
use std::cell::RefCell;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();

        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

/// What ends a resize. The counterpart of [`super::move_grab::MoveEnd`], and for the same
/// reason: a resize can be started from a border, with the button still held, or chosen from
/// the window menu or a keybind, with no button held at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEnd {
    /// Dragging a border: the resize ends when the button that began it is let go.
    ButtonRelease,
    /// Chosen from the menu or a keybind: no button is held, so the window follows the
    /// pointer until the next click settles it.
    NextClick,
}

pub struct ResizeSurfaceGrab {
    start_data: PointerGrabStartData<Wlrix>,
    window: Window,

    edges: ResizeEdge,
    end: ResizeEnd,

    initial_rect: Rectangle<i32, Logical>,
    last_window_size: Size<i32, Logical>,
    /// Resize the window itself as the pointer moves, or only a wireframe of where it would
    /// end up. Read from the config when the grab starts, so a setting changed mid-resize
    /// cannot leave one half of it in each mode.
    opaque: bool,
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Wlrix>,
        window: Window,
        edges: ResizeEdge,
        end: ResizeEnd,
        initial_window_rect: Rectangle<i32, Logical>,
        opaque: bool,
    ) -> Self {
        let initial_rect = initial_window_rect;

        // Only a Wayland toplevel needs this. It is read back in [`handle_commit`], which is how
        // an xdg resize finishes: the client acknowledges the size, commits a buffer at it, and
        // only then can the window be moved to match a top- or left-edge drag. X11 has no such
        // round trip -- a configure *is* the new geometry -- so nothing reads this for one, and
        // an X11 window's surface is left alone.
        if let Some(toplevel) = window.toplevel() {
            ResizeSurfaceState::with(toplevel.wl_surface(), |state| {
                *state = ResizeSurfaceState::Resizing {
                    edges,
                    initial_rect,
                };
            });
        }

        Self {
            start_data,
            window,
            edges,
            end,
            initial_rect,
            last_window_size: initial_rect.size,
            opaque,
        }
    }

    /// Where the client area would sit at `size`, given which edges are being dragged.
    ///
    /// A resize from the top or left moves the window as well as changing its size. In opaque
    /// mode `handle_commit` works that out after the client has committed its new buffer;
    /// nothing commits during a non-opaque resize, so the wireframe has to work it out itself
    /// -- and it is the same arithmetic, kept next to the state it reads.
    fn client_rect(&self, size: Size<i32, Logical>) -> Rectangle<i32, Logical> {
        resized_rect(self.edges, self.initial_rect, size)
    }

    /// The size the client has said it will not go outside, in whichever protocol it speaks.
    ///
    /// Both spell "no limit" as zero, and the caller reads them that way, so an X11 window with
    /// no `WM_NORMAL_HINTS` at all comes back as unconstrained rather than as a window that
    /// cannot be resized. Which is what it is: hints are optional, and most windows set none.
    fn size_limits(&self) -> (Size<i32, Logical>, Size<i32, Logical>) {
        if let Some(toplevel) = self.window.toplevel() {
            return compositor::with_states(toplevel.wl_surface(), |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let data = guard.current();
                (data.min_size, data.max_size)
            });
        }
        if let Some(x11) = self.window.x11_surface() {
            return (
                x11.min_size().unwrap_or_default(),
                x11.max_size().unwrap_or_default(),
            );
        }
        Default::default()
    }

    /// Hand `last_window_size` to the client, however it is asked.
    ///
    /// `resizing` is whether the drag is still going: xdg-shell carries that as a window state,
    /// so a client can lay itself out cheaply while the pointer is down and do the expensive
    /// pass once on release.
    ///
    /// The two protocols differ in more than spelling. An xdg configure is a *proposal*: the
    /// client acknowledges it, commits a buffer at the new size, and `handle_commit` then moves
    /// the window if the drag was from the top or left. An X11 configure is the WM setting the
    /// geometry -- there is nothing to wait for and nothing to acknowledge -- so the move is done
    /// here, in the same breath as the resize.
    fn apply(&self, data: &mut Wlrix, resizing: bool) {
        if let Some(xdg) = self.window.toplevel() {
            xdg.with_pending_state(|state| {
                if resizing {
                    state.states.set(xdg_toplevel::State::Resizing);
                } else {
                    state.states.unset(xdg_toplevel::State::Resizing);
                }
                state.size = Some(self.last_window_size);
            });
            xdg.send_pending_configure();
            return;
        }

        let Some(x11) = self.window.x11_surface() else {
            return;
        };
        let rect = self.client_rect(self.last_window_size);
        // Nothing to be done about a failure: the window has gone, or is override-redirect and
        // was never ours to place. Both end the same way -- the grab finishes and the desktop
        // carries on -- and the existing X11 paths (maximize, minimize) say so the same way.
        let _ = x11.configure(rect);
        // `relocate_element`, not `map_element`: the latter always restacks, and a resize is not
        // a request to raise. Only needed on this path -- a Wayland window is moved by
        // `handle_commit` once its buffer matches.
        data.space.relocate_element(&self.window, rect.loc);
        data.request_redraw();
    }
}

/// Where a client rectangle ends up when resized to `size` by dragging `edges`.
///
/// A free function so it can be tested against the arithmetic in [`handle_commit`], which is
/// what puts an opaque resize in the same place -- the wireframe promising one thing and the
/// window landing somewhere else is the way this feature goes wrong.
pub fn resized_rect(
    edges: ResizeEdge,
    initial: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let mut loc = initial.loc;
    if edges.intersects(ResizeEdge::LEFT) {
        loc.x = initial.loc.x + (initial.size.w - size.w);
    }
    if edges.intersects(ResizeEdge::TOP) {
        loc.y = initial.loc.y + (initial.size.h - size.h);
    }
    Rectangle::new(loc, size)
}

impl PointerGrab<Wlrix> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        let mut delta = event.location - self.start_data.location;

        let mut new_window_width = self.initial_rect.size.w;
        let mut new_window_height = self.initial_rect.size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                delta.x = -delta.x;
            }

            new_window_width = (self.initial_rect.size.w as f64 + delta.x) as i32;
        }

        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) {
                delta.y = -delta.y;
            }

            new_window_height = (self.initial_rect.size.h as f64 + delta.y) as i32;
        }

        let (min_size, max_size) = self.size_limits();

        let min_width = min_size.w.max(1);
        let min_height = min_size.h.max(1);

        let max_width = if max_size.w == 0 {
            i32::MAX
        } else {
            max_size.w
        };
        let max_height = if max_size.h == 0 {
            i32::MAX
        } else {
            max_size.h
        };

        self.last_window_size = Size::from((
            new_window_width.max(min_width).min(max_width),
            new_window_height.max(min_height).min(max_height),
        ));

        // Non-opaque: the client is left alone entirely until the button comes up, and only
        // the wireframe follows the pointer. The size has already been clamped to the
        // client's own minimum and maximum above, so the outline never promises a size the
        // window could not take.
        if !self.opaque {
            data.drag_outline = Some(crate::decoration::DragOutline {
                client: self.client_rect(self.last_window_size),
                style: crate::frame::frame_style(&self.window),
            });
            data.request_redraw();
            return;
        }

        self.apply(data, true);
    }

    fn relative_motion(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        let done = match self.end {
            // A border drag: end on the release of whichever button began it.
            ResizeEnd::ButtonRelease => !handle.current_pressed().contains(&self.start_data.button),
            // A menu- or keybind-driven resize holds no button, so `current_pressed` is empty
            // and the test above would be true immediately -- the grab would end on the first
            // button event it saw. Any *press* settles it instead; the release of the click
            // that chose "Size" is a release, so it does not count.
            ResizeEnd::NextClick => event.state == ButtonState::Pressed,
        };
        if done {
            handle.unset_grab(self, data, event.serial, event.time, true);

            // The one place a non-opaque resize reaches the client at all: nothing was sent
            // while the wireframe was being dragged, so this configure is the whole resize.
            self.apply(data, false);

            // The last configure of a Wayland resize still has to be acknowledged, and the
            // window can only be moved to match a top- or left-edge drag once it has committed
            // a buffer at that size. An X11 window is already where `apply` put it.
            if let Some(xdg) = self.window.toplevel() {
                ResizeSurfaceState::with(xdg.wl_surface(), |state| {
                    *state = ResizeSurfaceState::WaitingForLastCommit {
                        edges: self.edges,
                        initial_rect: self.initial_rect,
                    };
                });
            }
        }
    }

    fn axis(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut Wlrix, handle: &mut PointerInnerHandle<'_, Wlrix>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<Wlrix> {
        &self.start_data
    }

    /// Clear the wireframe however the grab ended.
    ///
    /// The configure that actually applies the size is sent from `button`, which is also where
    /// the opaque path finishes; this only has to make sure no red rectangle is left behind.
    fn unset(&mut self, data: &mut Wlrix) {
        // See `MoveSurfaceGrab::unset`: released here, re-derived by the button handler, and not
        // asked about here because the pointer's lock is held while this runs.
        data.grab_cursor = None;

        data.drag_outline = None;
        data.request_redraw();
    }
}

/// State of the resize operation.
///
/// It is stored inside of WlSurface,
/// and can be accessed using [`ResizeSurfaceState::with`]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Resizing {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
    /// Resize is done, we are now waiting for last commit, to do the final move
    WaitingForLastCommit {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();

            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing {
                edges,
                initial_rect,
            } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit {
                edges,
                initial_rect,
            } => {
                // The resize is done, let's go back to idle
                *self = Self::Idle;

                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        state
            .commit()
            .and_then(|(edges, initial_rect)| {
                // If the window is being resized by top or left, its location must be adjusted
                // accordingly.
                edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                    let new_x = edges
                        .intersects(ResizeEdge::LEFT)
                        .then_some(initial_rect.loc.x + (initial_rect.size.w - geometry.size.w));

                    let new_y = edges
                        .intersects(ResizeEdge::TOP)
                        .then_some(initial_rect.loc.y + (initial_rect.size.h - geometry.size.h));

                    (new_x, new_y).into()
                })
            })
            .unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x {
        window_loc.x = new_x;
    }
    if let Some(new_y) = new_loc.y {
        window_loc.y = new_y;
    }

    if new_loc.x.is_some() || new_loc.y.is_some() {
        // If TOP or LEFT side of the window got resized, we have to move it
        space.map_element(window, window_loc, false);
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 400x300 client at (100, 100).
    fn initial() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((100, 100)), Size::from((400, 300)))
    }

    #[test]
    fn dragging_the_right_or_bottom_edge_leaves_the_corner_alone() {
        let grown = resized_rect(ResizeEdge::RIGHT, initial(), Size::from((500, 300)));
        assert_eq!(
            grown,
            Rectangle::new(Point::from((100, 100)), Size::from((500, 300)))
        );
        let taller = resized_rect(ResizeEdge::BOTTOM, initial(), Size::from((400, 400)));
        assert_eq!(taller.loc, Point::from((100, 100)));
    }

    #[test]
    fn dragging_the_left_or_top_edge_moves_the_window_as_well() {
        // The opposite edge is the one that stays put, which is the whole reason this
        // arithmetic exists: a wireframe that grew rightwards while the pointer pulled left
        // would be pointing at the wrong place.
        let wider = resized_rect(ResizeEdge::LEFT, initial(), Size::from((500, 300)));
        assert_eq!(wider.loc.x, 0, "the right edge should have stayed at 500");
        assert_eq!(wider.loc.x + wider.size.w, 500);

        let taller = resized_rect(ResizeEdge::TOP, initial(), Size::from((400, 400)));
        assert_eq!(taller.loc.y, 0, "the bottom edge should have stayed at 400");
        assert_eq!(taller.loc.y + taller.size.h, 400);
    }

    #[test]
    fn a_corner_drag_moves_in_both_axes() {
        let r = resized_rect(ResizeEdge::TOP_LEFT, initial(), Size::from((450, 350)));
        assert_eq!(r.loc, Point::from((50, 50)));
        // The bottom-right corner is the anchor and has not budged.
        assert_eq!(
            r.loc + Point::from((r.size.w, r.size.h)),
            Point::from((500, 400))
        );
    }

    #[test]
    fn shrinking_from_the_top_left_pulls_the_corner_in() {
        let r = resized_rect(ResizeEdge::TOP_LEFT, initial(), Size::from((200, 100)));
        assert_eq!(r.loc, Point::from((300, 300)));
        assert_eq!(
            r.loc + Point::from((r.size.w, r.size.h)),
            Point::from((500, 400))
        );
    }
}
