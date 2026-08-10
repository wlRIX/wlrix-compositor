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
                // A minimized-icon drag follows the pointer, independent of client focus.
                self.drag_icon(location);
                self.hover_window_menu(location);
                self.update_frame_cursor(location);
                // Under the `pointer` focus policy, crossing onto a window gives it the
                // keyboard. Does nothing under `click`, which is the default.
                crate::focus::follow_pointer(self, location);
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
                self.drag_icon(pos);
                self.hover_window_menu(pos);
                self.update_frame_cursor(pos);
                crate::focus::follow_pointer(self, pos);
                // The cursor moved, so the screen changed.
                self.request_redraw();
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();
                let location = pointer.current_location();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    // A tile chosen for "Move" is following the pointer; this click puts it down
                    // and does nothing else.
                    if self.icon_move_awaits_click() {
                        self.drop_icon(location);
                        return;
                    }
                    // An open window menu takes the press first: on the panel it chooses an item,
                    // anywhere else it just takes the menu down and the press carries on to
                    // whatever is under it (so a second click on the menu button still reads as
                    // the double click that closes the window).
                    if let Some(menu) = self.window_menu.as_ref() {
                        let on_menu = menu.contains(location);
                        let chosen = menu.action_at(location);
                        let window = menu.window.clone();
                        self.close_window_menu();
                        if on_menu {
                            if let Some(action) = chosen {
                                self.activate_menu_action(&window, action, serial, location);
                            }
                            return;
                        }
                    }
                    // An overlay- or top-layer surface sits above every window, so the press
                    // is that client's: neither a frame under it nor a window beneath should
                    // react. It takes focus if it asked to.
                    if self.layer_covers_windows_at(location) {
                        if let Some(surface) = self.focusable_layer_under(location) {
                            crate::focus::focus_layer_surface(self, &surface);
                        }
                    } else if let Some((window, part)) = self.frame_under(location) {
                        // A press on a server-side frame moves/resizes the window or arms a
                        // button; it never reaches the client.
                        self.press_frame(&window, part, serial, button);
                        return;
                    } else {
                        // A press in the client area focuses, and raises too unless
                        // `[focus] raise_on_click` says otherwise. Pointer focus deliberately
                        // does not restack, so with raising on this is the only way to pull a
                        // buried window to the front; with it off, that is what the frame and
                        // the window menu's Raise are for. See `crate::focus`.
                        let clicked = self
                            .space
                            .element_under(location)
                            .map(|(window, _)| window.clone());
                        let raise = self.config.focus.raise_on_click;
                        match clicked {
                            // A press on an open menu or tooltip leaves focus exactly where it
                            // is: the window that opened it has to stay the active one, or the
                            // toolkit takes the menu down instead of acting on the click. See
                            // `crate::focus::focusable`. The press itself still reaches the
                            // client through `pointer.button` below.
                            Some(window) if !crate::focus::focusable(&window) => {}
                            Some(window) if raise => crate::focus::focus_window(self, &window),
                            Some(window) => crate::focus::focus_window_in_place(self, &window),
                            // No window here: a press may have landed on a minimized-window
                            // icon (left click restores, left drag rearranges, right posts its
                            // window menu); otherwise on a bottom- or background-layer surface,
                            // which is where the desktop icons live and which takes focus if it
                            // asked to; otherwise the desktop really is empty.
                            None => match self.icon_under(location) {
                                Some(window) => match button {
                                    crate::frame::BTN_LEFT => self.press_icon(&window, location),
                                    crate::frame::BTN_RIGHT => {
                                        self.open_window_menu(&window, location.to_i32_round())
                                    }
                                    _ => {}
                                },
                                None => match self.focusable_layer_under(location) {
                                    Some(surface) => {
                                        crate::focus::focus_layer_surface(self, &surface)
                                    }
                                    None => crate::focus::clear_focus(self),
                                },
                            },
                        }
                    }
                } else if ButtonState::Released == button_state {
                    // Complete an armed frame button (minimize/maximize) or icon press.
                    self.release_frame(location, button);
                    self.release_icon(location);
                }

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
                // A release that ended a move or resize gave the cursor back inside the call
                // above, and what it should be now is decided by whatever the pointer is over --
                // frame, client or bare desktop. Done here rather than in the grab's `unset`,
                // which runs while smithay holds the pointer's lock, and without waiting for the
                // next motion, which would leave the drag's cursor on screen until the mouse
                // twitched.
                self.update_frame_cursor(location);
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
