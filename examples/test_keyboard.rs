// SPDX-License-Identifier: GPL-3.0-or-later
//! Types keys into the compositor through `zwp_virtual_keyboard_v1`.
//!
//! Nothing on the development machine can synthesize a *pointer* gesture -- no ydotool, no
//! wtype, and the compositor binds no `zwlr_virtual_pointer_v1` -- so pointer work ends with a
//! hand on the mouse. Keyboard work does not have to: the compositor implements the virtual
//! keyboard protocol for the on-screen keyboard's sake, and that is enough to drive a client
//! under test from a script. It is what established that `MenuItem.HotKey` never fires from an
//! unopened submenu while the same command on `Window.KeyBindings` does, and what has been
//! rebuilt from scratch twice since, which is why it now lives here.
//!
//! The keymap is the compositor's own, taken from the `wl_keyboard.keymap` it sends every
//! keyboard client and handed straight back to the virtual keyboard. So the keycodes below
//! mean exactly what the compositor thinks they mean, and no xkb dependency is needed to say
//! so. They are evdev codes, *not* offset by 8 the way an X11 keycode is; the compositor adds
//! that itself.
//!
//! **The one trap worth knowing before trusting a result.** Smithay's virtual-keyboard handler
//! sends `wl_keyboard.key` straight to the surface that currently holds focus, bypassing seat
//! grabs entirely -- where a real key goes through `keyboard.input` and therefore through any
//! grab. So after a grabbing popup is dismissed, a client looks keyboard-dead here in a way it
//! never is with a real keyboard, because the grab's "restore focus on the next key" never
//! runs. Do not report that as a compositor bug; it is this tool.
//!
//! Usage: `cargo run --example test_keyboard -- ctrl+a tab return`, one gesture per argument,
//! with `WAYLAND_DISPLAY` set to the compositor under test. Not part of the compositor; a dev
//! tool only.

use std::os::fd::{AsFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
    },
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

#[derive(Default)]
struct State {
    seat: Option<WlSeat>,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    /// The compositor's keymap, as `(format, fd, size)`, to hand back unchanged.
    keymap: Option<(u32, OwnedFd, u32)>,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_seat" => state.seat = Some(registry.bind(name, 7, queue, ())),
            "zwp_virtual_keyboard_manager_v1" => {
                state.manager = Some(registry.bind(name, 1, queue, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

/// evdev keycodes, by the name a gesture uses.
fn keycode(name: &str) -> Option<u32> {
    Some(match name {
        "ctrl" => 29,
        "shift" => 42,
        "alt" => 56,
        "tab" => 15,
        "space" => 57,
        "escape" | "esc" => 1,
        "return" | "enter" => 28,
        "backspace" => 14,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        "home" => 102,
        "end" => 107,
        "delete" => 111,
        "menu" | "apps" => 127,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        _ => return None,
    })
}

/// The xkb modifier mask that goes with a held modifier, for `modifiers`.
fn modifier_mask(name: &str) -> Option<u32> {
    Some(match name {
        "shift" => 1,
        "ctrl" => 4,
        "alt" => 8,
        _ => return None,
    })
}

fn main() {
    let gestures: Vec<String> = std::env::args().skip(1).collect();
    if gestures.is_empty() {
        eprintln!("usage: test_keyboard <gesture>...   e.g. ctrl+a tab return");
        std::process::exit(2);
    }

    let connection = Connection::connect_to_env().expect("no Wayland display");
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let mut state = State::default();
    queue.roundtrip(&mut state).expect("registry roundtrip");

    let seat = state
        .seat
        .clone()
        .expect("compositor advertised no wl_seat");
    let manager = state
        .manager
        .clone()
        .expect("compositor does not implement zwp_virtual_keyboard_manager_v1");

    // A keymap is mandatory before any key -- the compositor posts `no_keymap` otherwise -- and
    // the one to send is the compositor's own, so a keycode means the same on both sides.
    seat.get_keyboard(&handle, ());
    queue.roundtrip(&mut state).expect("keymap roundtrip");
    let (format, fd, size) = state.keymap.take().expect("compositor sent no keymap");

    let keyboard = manager.create_virtual_keyboard(&seat, &handle, ());
    keyboard.keymap(format, fd.as_fd(), size);
    queue
        .roundtrip(&mut state)
        .expect("virtual keyboard roundtrip");

    // Monotonic, and the units are milliseconds; only the ordering matters to a client.
    let mut time = 0u32;
    for gesture in &gestures {
        let parts: Vec<&str> = gesture.split('+').collect();
        let (modifiers, key) = parts.split_at(parts.len() - 1);

        let Some(code) = keycode(key[0]) else {
            eprintln!("test_keyboard: unknown key {}", key[0]);
            std::process::exit(2);
        };

        let mut mask = 0;
        let mut held = Vec::new();
        for name in modifiers {
            let Some(bit) = modifier_mask(name) else {
                eprintln!("test_keyboard: unknown modifier {name}");
                std::process::exit(2);
            };
            mask |= bit;
            held.push(keycode(name).expect("a modifier name has a keycode"));
        }

        // The mask and the key presses both go out: a client reads the modifier state from
        // `modifiers`, but one watching raw keys sees a combination only if the modifier was
        // pressed as a key too.
        if mask != 0 {
            keyboard.modifiers(mask, 0, 0, 0);
            for code in &held {
                time += 10;
                keyboard.key(time, *code, 1);
            }
        }

        time += 10;
        keyboard.key(time, code, 1);
        time += 10;
        keyboard.key(time, code, 0);

        // Released in reverse, as a hand would.
        if mask != 0 {
            for code in held.iter().rev() {
                time += 10;
                keyboard.key(time, *code, 0);
            }
            keyboard.modifiers(0, 0, 0, 0);
        }

        queue.roundtrip(&mut state).expect("key roundtrip");
        println!("sent {gesture}");
    }
}
