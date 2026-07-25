// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
            KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        session::Session,
    },
    input::{
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    utils::SERIAL_COUNTER,
};
use tracing::{info, warn};

use crate::state::Wlrix;

/// A compositor-level key combo intercepted before it reaches clients.
enum KeyAction {
    /// Switch to virtual terminal `n` (Ctrl+Alt+F`n`).
    SwitchVt(i32),
    /// Quit the compositor (Ctrl+Alt+Backspace).
    Quit,
    /// Cycle to the next configured keyboard layout (Super+Space). Only does anything
    /// when the config lists more than one layout, e.g. `layout = "jp,us"`.
    CycleLayout,
    /// Temporary: switch to the desk at this index (Super+1..9). Retired once the
    /// `wlrix-desks` protocol drives desk switching.
    SwitchDesk(usize),
    /// Temporary: create a desk and switch to it (Super+Shift+Up).
    CreateDesk,
    /// Temporary: delete the active desk (Super+Shift+Down).
    DeleteDesk,
    /// Temporary: maximize/unmaximize the focused window (Super+F).
    MaximizeToggle,
    /// Temporary: minimize the focused window (Super+M).
    Minimize,
    /// Temporary: restore every minimized window (Super+Shift+M).
    RestoreAll,
    /// Temporary: lower the focused window (Super+L).
    Lower,
    /// Temporary: move the focused window to the desk at this index (Super+Ctrl+1..9).
    MoveToDesk(usize),
}

impl Wlrix {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Any input at all means the user is here, so idle notifications reset. Done
        // once at the top rather than per event kind, so a new kind cannot forget to.
        // Device add/remove is not activity, but is rare enough not to matter.
        crate::idle::notify_activity(self);

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;
                let keyboard = self.seat.get_keyboard().unwrap();

                // Intercept compositor-level combos (VT switch, quit) before clients.
                let action = keyboard.input::<KeyAction, _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, mods, handle| {
                        if !pressed {
                            return FilterResult::Forward;
                        }
                        let sym = handle.modified_sym().raw();
                        if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                            .contains(&sym)
                        {
                            let vt = (sym - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                            return FilterResult::Intercept(KeyAction::SwitchVt(vt));
                        }
                        if mods.ctrl && mods.alt && sym == keysyms::KEY_BackSpace {
                            return FilterResult::Intercept(KeyAction::Quit);
                        }
                        // Super+Space cycles keyboard layouts. The compositor's own
                        // toggle, complementing any `grp:` xkb option -- one works from
                        // a keybind, the other from a modifier held down.
                        if mods.logo && sym == keysyms::KEY_space {
                            return FilterResult::Intercept(KeyAction::CycleLayout);
                        }
                        // Temporary desk keybinds, for exercising desks before the
                        // wlrix-desks protocol lands: Super+1..9 switch, Super+Shift+Up/Down
                        // create/delete.
                        if mods.logo
                            && !mods.shift
                            && !mods.ctrl
                            && (keysyms::KEY_1..=keysyms::KEY_9).contains(&sym)
                        {
                            let index = (sym - keysyms::KEY_1) as usize;
                            return FilterResult::Intercept(KeyAction::SwitchDesk(index));
                        }
                        if mods.logo && mods.shift && sym == keysyms::KEY_Up {
                            return FilterResult::Intercept(KeyAction::CreateDesk);
                        }
                        if mods.logo && mods.shift && sym == keysyms::KEY_Down {
                            return FilterResult::Intercept(KeyAction::DeleteDesk);
                        }
                        // Temporary window-op keybinds, exercising the ops before the
                        // wlrix-desks protocol drives them.
                        if mods.logo && !mods.shift && sym == keysyms::KEY_f {
                            return FilterResult::Intercept(KeyAction::MaximizeToggle);
                        }
                        if mods.logo && !mods.shift && sym == keysyms::KEY_m {
                            return FilterResult::Intercept(KeyAction::Minimize);
                        }
                        if mods.logo && mods.shift && sym == keysyms::KEY_M {
                            return FilterResult::Intercept(KeyAction::RestoreAll);
                        }
                        if mods.logo && !mods.shift && sym == keysyms::KEY_l {
                            return FilterResult::Intercept(KeyAction::Lower);
                        }
                        if mods.logo
                            && mods.ctrl
                            && (keysyms::KEY_1..=keysyms::KEY_9).contains(&sym)
                        {
                            let index = (sym - keysyms::KEY_1) as usize;
                            return FilterResult::Intercept(KeyAction::MoveToDesk(index));
                        }
                        FilterResult::Forward
                    },
                );

                match action {
                    Some(KeyAction::SwitchVt(vt)) => {
                        if let Some(session) = self.session.as_mut() {
                            info!(vt, "switching VT");
                            if let Err(err) = session.change_vt(vt) {
                                warn!(?err, "failed to switch VT");
                            }
                        }
                    }
                    Some(KeyAction::Quit) => {
                        info!("quit requested (Ctrl+Alt+Backspace)");
                        self.loop_signal.stop();
                    }
                    Some(KeyAction::CycleLayout) => {
                        // `keyboard` is an owned handle, so this borrow of `self` is free
                        // to be the `&mut D` `with_xkb_state` needs. smithay notifies
                        // clients of the layout change via the modifiers it sends.
                        info!("cycling keyboard layout (Super+Space)");
                        keyboard.with_xkb_state(self, |mut context| context.cycle_next_layout());
                    }
                    Some(KeyAction::SwitchDesk(index)) => {
                        info!(index, "switching desk (temporary keybind)");
                        self.switch_desk_index(index);
                    }
                    Some(KeyAction::CreateDesk) => {
                        info!("creating desk (temporary keybind)");
                        let id = self.create_desk();
                        self.switch_desk(id);
                    }
                    Some(KeyAction::DeleteDesk) => {
                        info!("deleting desk (temporary keybind)");
                        self.delete_active_desk();
                    }
                    Some(KeyAction::MaximizeToggle) => {
                        if let Some(window) = self.focused_window() {
                            self.toggle_maximize_window(&window);
                        }
                    }
                    Some(KeyAction::Minimize) => {
                        if let Some(window) = self.focused_window() {
                            self.minimize_window(&window);
                        }
                    }
                    Some(KeyAction::RestoreAll) => {
                        self.restore_all_minimized();
                    }
                    Some(KeyAction::Lower) => {
                        if let Some(window) = self.focused_window() {
                            self.lower_window(&window);
                        }
                    }
                    Some(KeyAction::MoveToDesk(index)) => {
                        if let (Some(window), Some(&id)) =
                            (self.focused_window(), self.desks.order().get(index))
                        {
                            self.move_window_to_desk(&window, id);
                        }
                    }
                    None => {}
                }
            }
            // Relative motion: what a physical mouse sends through libinput. Absolute
            // motion (below) comes from tablets, touchscreens and the nested backend,
            // so handling only that leaves a real mouse unable to move the cursor.
            InputEvent::PointerMotion { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();

                let delta = event.delta();
                let location = crate::placement::clamp_to_outputs(
                    &self.space,
                    pointer.current_location() + delta,
                );
                let under = self.surface_under(location);

                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location,
                        serial,
                        time: event.time_msec(),
                    },
                );
                // Also report the raw delta, for clients that track pointer movement
                // rather than position.
                pointer.relative_motion(
                    self,
                    under,
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                pointer.frame(self);
                self.request_redraw();
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                // The cursor moved, so the screen changed.
                self.request_redraw();
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    // Click-to-focus. Pointer focus, where this would instead happen on
                    // motion, is a configurable alternative later; see `crate::focus`.
                    let clicked = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(window, _)| window.clone());
                    match clicked {
                        Some(window) => crate::focus::focus_window(self, &window),
                        None => crate::focus::clear_focus(self),
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                // Clicking can raise or re-focus a window.
                self.request_redraw();
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
