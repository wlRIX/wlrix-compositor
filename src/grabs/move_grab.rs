// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use crate::Wlrix;
use crate::decoration::DragOutline;
use smithay::backend::input::ButtonState;
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

/// What ends a move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveEnd {
    /// Dragging: the move ends when the button that began it is let go.
    ButtonRelease,
    /// Chosen from the window menu: no button is held, so the window follows the pointer until
    /// the next click puts it down.
    NextClick,
}

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Wlrix>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub end: MoveEnd,
    /// Drag the window itself, or only a wireframe of where it would land. Read from the
    /// config when the grab starts, so changing the setting mid-drag cannot leave a move
    /// half in one mode and half in the other.
    pub opaque: bool,
    /// Where the window would go, tracked in both modes: opaque moves have already put it
    /// there, and non-opaque ones need it on release.
    pub current_location: Point<i32, Logical>,
}

impl PointerGrab<Wlrix> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Wlrix,
        handle: &mut PointerInnerHandle<'_, Wlrix>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let new_location = (self.initial_window_location.to_f64() + delta).to_i32_round();
        self.current_location = new_location;

        if self.opaque {
            data.space
                .map_element(self.window.clone(), new_location, true);
            return;
        }
        // Non-opaque: the window stays put and only the wireframe moves. Its size is the
        // window's current one, since a move does not change it.
        data.drag_outline = Some(DragOutline {
            client: smithay::utils::Rectangle::new(new_location, self.window.geometry().size),
            style: crate::frame::frame_style(&self.window),
        });
        data.request_redraw();
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
            // Left or middle can begin a drag (see `Wlrix::press_frame`), so end on whichever
            // one did rather than on a fixed button.
            MoveEnd::ButtonRelease => !handle.current_pressed().contains(&self.start_data.button),
            // A menu-driven move holds no button; any press puts the window down. The release of
            // the click that chose "Move" must not count, but that is a release, not a press.
            MoveEnd::NextClick => event.state == ButtonState::Pressed,
        };
        if done {
            handle.unset_grab(self, data, event.serial, event.time, true);
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

    /// Put the window down where the wireframe ended up.
    ///
    /// In `unset` rather than in `button`, so every way a grab can end goes through it -- a
    /// drag released, a menu-driven move clicked down, or the grab being taken away by
    /// something else. Leaving the outline behind would paint a red rectangle over the
    /// desktop until the next move.
    fn unset(&mut self, data: &mut Wlrix) {
        if self.opaque {
            return;
        }
        data.drag_outline = None;
        data.space
            .map_element(self.window.clone(), self.current_location, true);
        data.request_redraw();
    }
}
