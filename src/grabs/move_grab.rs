// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use crate::Wlrix;
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
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);
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

    fn unset(&mut self, _data: &mut Wlrix) {}
}
