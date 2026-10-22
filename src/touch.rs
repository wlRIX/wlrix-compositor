// SPDX-License-Identifier: GPL-3.0-or-later
//! The touchscreen: where a finger lands on the desktop, and who gets it.
//!
//! IRIX had no touchscreens, so none of this has a 4Dwm precedent; it follows GNOME and KDE
//! where there is a choice. The plan is `_docs/wlrix-touchscreen-roadmap.md`.
//!
//! # Where a finger lands
//!
//! A touchscreen is part of one panel and reports a finger as a fraction of *that panel*, in the
//! panel's own unrotated orientation. So unlike a tablet, which spans the whole desktop (see the
//! absolute-motion arm in `crate::input`), a touch is mapped onto one output: chosen by
//! [`pick_output`], turned the right way up by undoing the output's transform, and offset by the
//! output's place in the layout. The output's logical geometry already has its scale in it, so a
//! 150% panel needs nothing further. [`to_desktop`] is that mapping, and the tests at the bottom
//! hold it to the same transform the renderer draws with -- a finger lands on whatever is drawn
//! under it, on every rotation.
//!
//! # Who gets it
//!
//! Decided once per finger, when it goes down; see [`route`].
//!
//! - **A client that bound `wl_touch`** gets the finger as touch: Avalonia (the wlRIX fork's
//!   input dispatcher), GTK, Qt, the browsers, and XWayland, which turns it into XI2 touch and does
//!   its own pointer emulation for X clients that only speak the core protocol. Every further
//!   finger goes to whatever surface it lands on, as the protocol intends. The first finger
//!   focuses and raises the window it touches, exactly as a click does.
//! - **Everything else** gets a pointer driven by the first finger: the compositor's own chrome
//!   (4Dwm frames, the window menu, minimized icons), and clients that bound only `wl_pointer` --
//!   which today is every wlRIX client built on smithay-client-toolkit: the desktop icons, the
//!   tray, the lock screen, the screenshot overlay, the OSD. The pointer warps to the finger and
//!   the press goes through `Wlrix::pointer_button`, the very function a mouse button does, so
//!   none of them can tell a finger from a mouse.
//!
//! An emulated finger does not press straight away. A finger is imprecise and slow to leave the
//! glass, so it is held undecided until it either moves more than [`SLOP`] (a left-button drag
//! from where it went down: a titlebar moves its window), lifts (a left click), or stays still for
//! [`HOLD`] (a right-button press: the window menu on a frame, an icon's menu on the desktop). A
//! second tap close in time and place to the first lands exactly on the first, so a double tap
//! reads as a double click to a client whose threshold is a few pixels.
//!
//! While one finger is emulating, other fingers are ignored; and while fingers are down on
//! `wl_touch` clients, a finger that would need emulating is ignored, since the pointer can only
//! be in one place.
//!
//! # The cursor
//!
//! Hidden from the first touch until a mouse or touchpad moves or clicks, as GNOME and KDE do: an
//! arrow left wherever the mouse last was, or jumping about under a finger, is only in the way.
//!
//! # Grabs a client starts from a touch
//!
//! A client may answer a touch by asking for something smithay only knows how to do with a
//! pointer: an interactive move or resize (`xdg_toplevel.move` from a headerbar,
//! `_NET_WM_MOVERESIZE` from Steam), or a popup grab. Rather than a second, touch-driven copy of
//! each grab, the finger is handed over to the pointer:
//!
//! - **Move and resize:** [`Wlrix::adopt_touch`] cancels the finger's touch sequence and makes it
//!   the emulated pointer with the left button held, so the ordinary pointer grab runs and ends
//!   when the finger lifts.
//! - **Popups:** while the pointer holds any grab, a new finger is routed to the pointer. A popup
//!   grab is a pointer grab, so a tap outside the menu dismisses it and a tap inside chooses an
//!   item, as with a mouse.

use std::time::{Duration, Instant};

use smithay::{
    backend::input::{AbsolutePositionEvent, ButtonState, InputBackend, TouchSlot},
    input::{
        pointer::GrabStartData as PointerGrabStartData,
        touch::{DownEvent, MotionEvent, UpEvent},
    },
    output::Output,
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform},
};
use tracing::debug;

use crate::{
    frame::{BTN_LEFT, BTN_RIGHT},
    state::Wlrix,
};

/// How far, in logical pixels, an emulated finger may wander before it counts as a drag rather
/// than a tap or a hold. GTK's drag threshold is 8; a finger on a high-density laptop panel needs
/// a little more.
pub const SLOP: f64 = 10.0;

/// How long an emulated finger has to stay put to be a right-button press. GTK's long press.
pub const HOLD: Duration = Duration::from_millis(500);

/// How close, in logical pixels, a second tap has to land to the first to be moved onto it.
const DOUBLE_TAP_SLOP: f64 = 24.0;

/// Connector names of panels built into the machine, which is where a touchscreen is when nothing
/// says otherwise.
const BUILT_IN: [&str; 3] = ["eDP", "LVDS", "DSI"];

/// Everything the compositor remembers about the fingers on the glass.
#[derive(Debug, Default)]
pub struct TouchState {
    /// Fingers down on `wl_touch` clients.
    fingers: Vec<Finger>,
    /// The finger driving the pointer, if there is one.
    emulated: Option<Emulated>,
    /// Bumped for every emulated finger, so a hold timer that fires after its finger has gone
    /// cannot act on the next one.
    generation: u64,
    /// Whether a `wl_touch` event has been sent since the last frame, and so whether one is owed.
    frame_owed: bool,
    /// Where and when the last emulated tap went down, for [`DOUBLE_TAP_SLOP`].
    last_tap: Option<(Point<f64, Logical>, Instant)>,
    /// Whether the cursor is hidden because the touchscreen was the last thing used.
    pub cursor_hidden: bool,
    /// The output udev names for the device the current event came from (its `WL_OUTPUT`
    /// property), set by the udev backend just before the event is processed. Always `None`
    /// under the nested backend. See [`pick_output`].
    pub device_output: Option<String>,
}

/// A finger on a `wl_touch` client.
#[derive(Debug, Clone, Copy)]
struct Finger {
    slot: TouchSlot,
    /// The serial its `down` was sent with, which is what a client quotes back to ask for a move,
    /// a resize or a popup grab.
    serial: Serial,
    /// Where it is now.
    location: Point<f64, Logical>,
}

/// The finger driving the pointer.
#[derive(Debug)]
struct Emulated {
    slot: TouchSlot,
    /// Where it went down, which is where the press goes if it turns out to be one.
    origin: Point<f64, Logical>,
    phase: Phase,
    generation: u64,
}

#[derive(Debug)]
enum Phase {
    /// Not yet a tap, a drag or a hold. The timer is the hold's.
    Undecided(Option<RegistrationToken>),
    /// The button is down, and comes up with the finger.
    Pressed(u32),
}

/// Who a new finger goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The client under it, as `wl_touch`.
    Client,
    /// The pointer, as a click or a drag.
    Pointer,
    /// Nobody: another finger already has the pointer, or a finger that would need it arrived
    /// while others are on touch clients.
    Ignore,
}

/// Decide who a new finger goes to.
///
/// - `client_takes_touch`: whether there is a client surface under the finger, and if so whether
///   its client bound `wl_touch`. `None` is compositor chrome or bare desktop.
/// - `pointer_busy`: the compositor itself is in the middle of something pointer-shaped -- a
///   window menu is open, a minimized icon is waiting to be put down, or a grab holds the pointer
///   (a move, or a client's popup menu). The finger goes to that, not past it.
/// - `fingers_on_clients`: other fingers are already down on `wl_touch` clients.
/// - `emulating`: another finger is already driving the pointer.
pub fn route(
    client_takes_touch: Option<bool>,
    pointer_busy: bool,
    fingers_on_clients: bool,
    emulating: bool,
) -> Route {
    if emulating {
        Route::Ignore
    } else if client_takes_touch == Some(true) && !pointer_busy {
        Route::Client
    } else if fingers_on_clients {
        Route::Ignore
    } else {
        Route::Pointer
    }
}

/// Which of `names` (the connected outputs, in layout order) the touchscreen belongs to.
///
/// `[touchscreen] output` if it names a connected output; else the one udev names for the device;
/// else the first built-in panel; else the first output. `None` only when there are no outputs.
pub fn pick_output(names: &[&str], configured: Option<&str>, udev: Option<&str>) -> Option<usize> {
    let named = |wanted: Option<&str>| {
        wanted.and_then(|wanted| names.iter().position(|&name| name == wanted))
    };
    named(configured)
        .or_else(|| named(udev))
        .or_else(|| {
            names
                .iter()
                .position(|name| BUILT_IN.iter().any(|prefix| name.starts_with(prefix)))
        })
        .or_else(|| (!names.is_empty()).then_some(0))
}

/// The size of the touch panel in its own, unrotated orientation: the output's logical size with
/// its transform undone. A rotated output swaps width and height; a flip changes neither.
pub fn panel_size(geometry: Rectangle<i32, Logical>, transform: Transform) -> Size<i32, Logical> {
    transform.invert().transform_size(geometry.size)
}

/// Where on the desktop a touch at `on_panel` lands: a point within [`panel_size`], in the panel's
/// own orientation, on an output at `geometry` with `transform`.
///
/// The renderer puts a logical point `l` at `transform.invert()` of it within the output (see
/// smithay's damage tracker: an output transform is given as surface rotation, so its inverse is
/// how the output itself is drawn). This is the other way round, so the finger lands on whatever
/// is drawn under it.
///
/// "The other way round" is not simply `transform` again. For the four rotations it is, but a
/// flipped transform's point mapping is a reflection, which undoes itself -- while smithay's
/// `invert` pairs `Flipped90` with `Flipped270`. So what undoes the drawing is worked out from
/// the point mapping, and the test below holds it to the renderer's for all eight.
pub fn to_desktop(
    on_panel: Point<f64, Logical>,
    geometry: Rectangle<i32, Logical>,
    transform: Transform,
) -> Point<f64, Logical> {
    let drawn = transform.invert();
    let undo = match drawn {
        Transform::Flipped
        | Transform::Flipped90
        | Transform::Flipped180
        | Transform::Flipped270 => drawn,
        rotation => rotation.invert(),
    };
    let size = panel_size(geometry, transform).to_f64();
    undo.transform_point_in(on_panel, &size) + geometry.loc.to_f64()
}

/// Whether `b` is within `radius` of `a`.
fn near(a: Point<f64, Logical>, b: Point<f64, Logical>, radius: f64) -> bool {
    let d = b - a;
    d.x * d.x + d.y * d.y <= radius * radius
}

impl Wlrix {
    /// The output the touchscreen belongs to, with its geometry. See [`pick_output`].
    fn touch_output(&self) -> Option<(Output, Rectangle<i32, Logical>)> {
        let outputs: Vec<(Output, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.clone(), self.space.output_geometry(output)?)))
            .collect();
        let names: Vec<String> = outputs.iter().map(|(output, _)| output.name()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let index = pick_output(
            &names,
            self.config.touchscreen.output.as_deref(),
            self.touch.device_output.as_deref(),
        )?;
        outputs.into_iter().nth(index)
    }

    /// Where on the desktop a touch event lands, or `None` with no outputs to land on.
    pub fn touch_location<B: InputBackend, E: AbsolutePositionEvent<B>>(
        &self,
        event: &E,
    ) -> Option<Point<f64, Logical>> {
        let (output, geometry) = self.touch_output()?;
        let transform = output.current_transform();
        let on_panel = event.position_transformed(panel_size(geometry, transform));
        Some(to_desktop(on_panel, geometry, transform))
    }

    /// Hide the cursor: the touchscreen is in use.
    fn hide_cursor_for_touch(&mut self) {
        if !self.touch.cursor_hidden {
            self.touch.cursor_hidden = true;
            self.request_redraw();
        }
    }

    /// Show the cursor again, if a touch hid it: a mouse or touchpad is in use.
    pub fn reveal_cursor(&mut self) {
        if self.touch.cursor_hidden {
            self.touch.cursor_hidden = false;
            self.request_redraw();
        }
    }

    /// Whether the compositor is in the middle of something the pointer drives, which a new
    /// finger has to go to rather than past. See [`route`].
    fn pointer_busy(&self) -> bool {
        self.window_menu.is_some()
            || self.icon_move_awaits_click()
            || self
                .seat
                .get_pointer()
                .is_some_and(|pointer| pointer.is_grabbed())
    }

    /// Whether `surface`'s client has a `wl_touch` to send to.
    fn takes_touch(&self, surface: &WlSurface) -> bool {
        let (Some(touch), Some(client)) = (self.seat.get_touch(), surface.client()) else {
            return false;
        };
        touch.client_touch(&client).next().is_some()
    }

    pub fn touch_down(&mut self, slot: TouchSlot, location: Point<f64, Logical>, time: u32) {
        self.hide_cursor_for_touch();

        let under = self.surface_under(location);
        let client_takes_touch = under.as_ref().map(|(surface, _)| self.takes_touch(surface));
        let route = route(
            client_takes_touch,
            self.pointer_busy(),
            !self.touch.fingers.is_empty(),
            self.touch.emulated.is_some(),
        );
        debug!(?slot, ?location, ?route, "touch down");

        match route {
            Route::Ignore => {}
            Route::Client => {
                let Some(touch) = self.seat.get_touch() else {
                    return;
                };
                // The first finger is the click: it focuses and raises. The rest are the same
                // gesture and leave focus where the first put it.
                if self.touch.fingers.is_empty() {
                    self.focus_for_press(location);
                }
                let serial = SERIAL_COUNTER.next_serial();
                touch.down(
                    self,
                    under,
                    &DownEvent {
                        slot,
                        location,
                        serial,
                        time,
                    },
                );
                self.touch.fingers.push(Finger {
                    slot,
                    serial,
                    location,
                });
                self.touch.frame_owed = true;
                self.request_redraw();
            }
            Route::Pointer => self.start_emulating(slot, location, time),
        }
    }

    pub fn touch_motion(&mut self, slot: TouchSlot, location: Point<f64, Logical>, time: u32) {
        if self.touch.emulated.as_ref().is_some_and(|e| e.slot == slot) {
            self.emulated_motion(location, time);
            return;
        }
        let Some(finger) = self.touch.fingers.iter_mut().find(|f| f.slot == slot) else {
            return;
        };
        finger.location = location;
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        // The surface under the finger now is only for a drag-and-drop the finger is carrying;
        // the touch itself stays with the surface it went down on.
        let under = self.surface_under(location);
        touch.motion(
            self,
            under,
            &MotionEvent {
                slot,
                location,
                time,
            },
        );
        self.touch.frame_owed = true;
    }

    pub fn touch_up(&mut self, slot: TouchSlot, time: u32) {
        if self.touch.emulated.as_ref().is_some_and(|e| e.slot == slot) {
            self.stop_emulating(time);
            return;
        }
        let Some(index) = self.touch.fingers.iter().position(|f| f.slot == slot) else {
            return;
        };
        self.touch.fingers.remove(index);
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.up(
            self,
            &UpEvent {
                slot,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        self.touch.frame_owed = true;
    }

    pub fn touch_frame(&mut self) {
        if std::mem::take(&mut self.touch.frame_owed)
            && let Some(touch) = self.seat.get_touch()
        {
            touch.frame(self);
        }
    }

    /// The device gave up on every finger: a palm, or the session going away mid-touch.
    ///
    /// Clients are told to forget the sequence. An emulated finger that had already pressed is
    /// released where it is, there being no way to un-press a button.
    pub fn touch_cancel(&mut self) {
        self.cancel_client_fingers();
        if let Some(emulated) = self.touch.emulated.take() {
            match emulated.phase {
                Phase::Undecided(timer) => {
                    if let Some(timer) = timer {
                        self.loop_handle.remove(timer);
                    }
                }
                Phase::Pressed(button) => {
                    self.pointer_button(button, ButtonState::Released, self.now_msec())
                }
            }
        }
    }

    /// Now, on the clock input events are stamped with (`CLOCK_MONOTONIC`, in milliseconds), for
    /// the events a touch makes up without a device event to take the time from: the hold, and
    /// the release a cancel or an adopted finger needs.
    fn now_msec(&self) -> u32 {
        Duration::from(self.clock.now()).as_millis() as u32
    }

    /// Cancel every finger on a `wl_touch` client, telling the clients so.
    fn cancel_client_fingers(&mut self) {
        if !self.touch.fingers.is_empty()
            && let Some(touch) = self.seat.get_touch()
        {
            touch.cancel(self);
        }
        self.touch.fingers.clear();
        self.touch.frame_owed = false;
    }

    fn start_emulating(&mut self, slot: TouchSlot, location: Point<f64, Logical>, time: u32) {
        // A second tap close enough to the first lands on it exactly, so the client sees a
        // double click rather than two clicks a few pixels apart.
        let double_click = self.config.mouse.double_click();
        let origin = match self.touch.last_tap {
            Some((last, at))
                if at.elapsed() <= double_click && near(last, location, DOUBLE_TAP_SLOP) =>
            {
                last
            }
            _ => location,
        };

        self.touch.generation += 1;
        let generation = self.touch.generation;
        let timer = self
            .loop_handle
            .insert_source(Timer::from_duration(HOLD), move |_, _, state| {
                state.hold_fired(generation);
                TimeoutAction::Drop
            })
            .ok();
        self.touch.emulated = Some(Emulated {
            slot,
            origin,
            phase: Phase::Undecided(timer),
            generation,
        });
        // The pointer goes to the finger straight away, so what is under it can show it: a tray
        // item lights up, a frame's cursor would change if one were showing.
        self.move_pointer_to(origin, time);
    }

    /// The finger has stayed put for [`HOLD`]: it is a right-button press.
    fn hold_fired(&mut self, generation: u64) {
        let Some(emulated) = self.touch.emulated.as_mut() else {
            return;
        };
        if emulated.generation != generation || !matches!(emulated.phase, Phase::Undecided(_)) {
            return;
        }
        // The timer is the one firing, and is dropped by returning `Drop`; it must not also be
        // removed.
        emulated.phase = Phase::Pressed(BTN_RIGHT);
        self.touch.last_tap = None;
        debug!("touch hold: right button");
        self.pointer_button(BTN_RIGHT, ButtonState::Pressed, self.now_msec());
    }

    fn emulated_motion(&mut self, location: Point<f64, Logical>, time: u32) {
        let Some(emulated) = self.touch.emulated.as_mut() else {
            return;
        };
        if let Phase::Undecided(timer) = &mut emulated.phase {
            if near(emulated.origin, location, SLOP) {
                // Still a tap or a hold; the pointer stays where the press will be.
                return;
            }
            // A drag: the left button goes down where the finger did, then follows it.
            let timer = timer.take();
            emulated.phase = Phase::Pressed(BTN_LEFT);
            if let Some(timer) = timer {
                self.loop_handle.remove(timer);
            }
            self.touch.last_tap = None;
            self.pointer_button(BTN_LEFT, ButtonState::Pressed, time);
        }
        let location = crate::placement::clamp_to_outputs(&self.space, location);
        self.move_pointer_to(location, time);
    }

    fn stop_emulating(&mut self, time: u32) {
        let Some(emulated) = self.touch.emulated.take() else {
            return;
        };
        match emulated.phase {
            Phase::Undecided(timer) => {
                if let Some(timer) = timer {
                    self.loop_handle.remove(timer);
                }
                // A tap: a whole click, where the finger went down.
                self.pointer_button(BTN_LEFT, ButtonState::Pressed, time);
                self.pointer_button(BTN_LEFT, ButtonState::Released, time);
                self.touch.last_tap = Some((emulated.origin, Instant::now()));
            }
            Phase::Pressed(button) => self.pointer_button(button, ButtonState::Released, time),
        }
    }

    /// Hand a finger on a `wl_touch` client over to the pointer, with the left button held, so
    /// that a pointer grab the client asked for can run on it. Returns the grab's start data.
    ///
    /// `serial` is what the client quoted: a Wayland client's `xdg_toplevel.move` or `resize`
    /// names the `down` it is answering. An X11 client's `_NET_WM_MOVERESIZE` carries no serial
    /// the compositor gave out, so `None` takes the most recent finger on `surface`'s client,
    /// which is XWayland's.
    ///
    /// The client's touch sequence is cancelled: from here on the compositor owns the finger,
    /// and the release that ends the grab comes when it lifts.
    pub fn adopt_touch(
        &mut self,
        surface: &WlSurface,
        serial: Option<Serial>,
    ) -> Option<PointerGrabStartData<Wlrix>> {
        if self.touch.emulated.is_some() {
            return None;
        }
        let client = surface.client()?;
        let finger = self
            .touch
            .fingers
            .iter()
            .rev()
            .find(|finger| match serial {
                Some(serial) => finger.serial == serial,
                None => self
                    .surface_under(finger.location)
                    .is_some_and(|(under, _)| {
                        under.client().is_some_and(|c| c.id() == client.id())
                    }),
            })
            .copied()?;
        tracing::info!(?finger, "touch handed to the pointer for a client's drag");

        self.cancel_client_fingers();
        self.touch.generation += 1;
        self.touch.emulated = Some(Emulated {
            slot: finger.slot,
            origin: finger.location,
            phase: Phase::Pressed(BTN_LEFT),
            generation: self.touch.generation,
        });
        self.move_pointer_to(finger.location, self.now_msec());
        Some(PointerGrabStartData {
            focus: None,
            button: BTN_LEFT,
            location: finger.location,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::_90,
        Transform::_180,
        Transform::_270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn point(x: f64, y: f64) -> Point<f64, Logical> {
        (x, y).into()
    }

    /// A touch at a fraction of the panel lands at that fraction of the output, offset by where
    /// the output is.
    #[test]
    fn an_unrotated_panel_maps_by_fraction() {
        let geometry = rect(0, 0, 1920, 1080);
        let size = panel_size(geometry, Transform::Normal);
        assert_eq!(size, (1920, 1080).into());
        assert_eq!(
            to_desktop(point(960.0, 540.0), geometry, Transform::Normal),
            point(960.0, 540.0)
        );
    }

    /// The laptop: a 2880x1800 panel at 150% is 1920x1200 logical, and the libinput fraction is
    /// taken of that, so the scale needs no handling of its own.
    #[test]
    fn a_scaled_panel_maps_onto_its_logical_size() {
        let geometry = rect(0, 0, 1920, 1200);
        let size = panel_size(geometry, Transform::Normal);
        // What `position_transformed(size)` gives for a finger a quarter across, halfway down.
        let on_panel = point(size.w as f64 * 0.25, size.h as f64 * 0.5);
        assert_eq!(
            to_desktop(on_panel, geometry, Transform::Normal),
            point(480.0, 600.0)
        );
    }

    /// A touchscreen to the right of another monitor lands on itself, not on the monitor at the
    /// origin -- and not across the whole desktop, which is how a tablet is mapped.
    #[test]
    fn a_second_monitor_is_offset_by_its_place_in_the_layout() {
        let geometry = rect(2560, 360, 1920, 1080);
        assert_eq!(
            to_desktop(point(0.0, 0.0), geometry, Transform::Normal),
            point(2560.0, 360.0)
        );
        assert_eq!(
            to_desktop(point(1920.0, 1080.0), geometry, Transform::Normal),
            point(4480.0, 1440.0)
        );
    }

    /// A rotated panel reports in its own orientation, so its size is the logical one swapped.
    #[test]
    fn a_rotated_panel_is_the_logical_size_turned() {
        let portrait = rect(0, 0, 1200, 1920);
        for transform in [
            Transform::_90,
            Transform::_270,
            Transform::Flipped90,
            Transform::Flipped270,
        ] {
            assert_eq!(panel_size(portrait, transform), (1920, 1200).into());
        }
        for transform in [Transform::_180, Transform::Flipped, Transform::Flipped180] {
            assert_eq!(panel_size(portrait, transform), (1200, 1920).into());
        }
    }

    /// The point of the whole mapping, for every transform: a finger lands on what is drawn
    /// under it. The renderer puts logical point `l` at `transform.invert()` of it within the
    /// output; mapping that panel point back has to give `l`.
    #[test]
    fn a_finger_lands_on_what_is_drawn_under_it() {
        let samples = [
            point(0.0, 0.0),
            point(100.0, 50.0),
            point(1199.0, 10.0),
            point(600.0, 1919.0),
        ];
        for transform in ALL {
            let size = transform.transform_size(Size::<i32, Logical>::from((1920, 1200)));
            let geometry = rect(1920, 200, size.w, size.h);
            for local in samples.iter().map(|p| {
                point(
                    p.x.min(geometry.size.w as f64),
                    p.y.min(geometry.size.h as f64),
                )
            }) {
                let drawn_at = transform
                    .invert()
                    .transform_point_in(local, &geometry.size.to_f64());
                assert_eq!(
                    to_desktop(drawn_at, geometry, transform),
                    local + geometry.loc.to_f64(),
                    "{transform:?} at {local:?}"
                );
            }
        }
    }

    /// The concrete case worth stating outright: a panel's top-left corner, rotated a quarter
    /// turn, is a corner of the portrait output -- the one smithay draws it at.
    #[test]
    fn the_corner_of_a_quarter_turned_panel() {
        let geometry = rect(0, 0, 1200, 1920);
        assert_eq!(
            to_desktop(point(0.0, 0.0), geometry, Transform::_90),
            point(1200.0, 0.0)
        );
        assert_eq!(
            to_desktop(point(0.0, 0.0), geometry, Transform::_270),
            point(0.0, 1920.0)
        );
    }

    #[test]
    fn the_touchscreen_is_the_configured_output_then_udevs_then_the_built_in_panel() {
        let names = ["DP-1", "eDP-1", "HDMI-A-1"];
        assert_eq!(pick_output(&names, Some("HDMI-A-1"), Some("DP-1")), Some(2));
        assert_eq!(pick_output(&names, None, Some("DP-1")), Some(0));
        assert_eq!(pick_output(&names, None, None), Some(1));
        // A configured output that is not connected is ignored, not obeyed into nothing.
        assert_eq!(pick_output(&names, Some("DP-9"), None), Some(1));
        assert_eq!(pick_output(&["DP-1", "DSI-1"], None, None), Some(1));
        assert_eq!(pick_output(&["DP-1", "LVDS-1"], None, None), Some(1));
        // No built-in panel: the first output, which is the nested backend's one window.
        assert_eq!(pick_output(&["winit"], None, None), Some(0));
        assert_eq!(pick_output(&[], Some("eDP-1"), None), None);
    }

    #[test]
    fn a_finger_on_a_touch_client_is_touch() {
        assert_eq!(route(Some(true), false, false, false), Route::Client);
        // More fingers on touch clients are touch too.
        assert_eq!(route(Some(true), false, true, false), Route::Client);
    }

    #[test]
    fn a_finger_on_chrome_or_a_pointer_only_client_is_the_pointer() {
        assert_eq!(route(None, false, false, false), Route::Pointer);
        assert_eq!(route(Some(false), false, false, false), Route::Pointer);
    }

    /// An open menu, an icon being moved, or a client's popup grab owns the pointer, and a
    /// finger goes to it even over a touch client -- so a tap outside a menu dismisses it.
    #[test]
    fn a_busy_pointer_takes_the_finger_even_over_a_touch_client() {
        assert_eq!(route(Some(true), true, false, false), Route::Pointer);
    }

    /// There is one pointer, so only one finger can be it.
    #[test]
    fn the_pointer_is_one_finger() {
        assert_eq!(route(Some(true), false, false, true), Route::Ignore);
        assert_eq!(route(None, false, false, true), Route::Ignore);
        // A finger needing the pointer while others are on touch clients.
        assert_eq!(route(None, false, true, false), Route::Ignore);
        assert_eq!(route(Some(true), true, true, false), Route::Ignore);
    }

    #[test]
    fn nearness_is_a_radius() {
        assert!(near(point(0.0, 0.0), point(6.0, 8.0), SLOP));
        assert!(!near(point(0.0, 0.0), point(8.0, 8.0), SLOP));
    }
}
