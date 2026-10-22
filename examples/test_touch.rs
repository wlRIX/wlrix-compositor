// SPDX-License-Identifier: GPL-3.0-or-later
//! Checks that the compositor's seat offers `wl_touch`, which is what makes a toolkit send touch
//! rather than nothing.
//!
//! A toolkit only creates a `wl_touch` when `wl_seat.capabilities` has the touch bit, and it reads
//! that once, when it binds the seat. So an app that started before the seat had touch never gets
//! any, which is why `crate::touch`'s seat advertises it whether or not a touchscreen is plugged
//! in -- and why this is worth a check that does not need one.
//!
//! Nothing on the development machine can synthesize a touch (there is no virtual-touch protocol,
//! and the nested backend's host has no touchscreen), so where a finger *lands* is covered by the
//! unit tests in `src/touch.rs`, and what it does by hand on the laptop.
//!
//! Usage: `cargo run --example test_touch`, with `WAYLAND_DISPLAY` set to the compositor under
//! test. Exits non-zero if the seat has no touch. Not part of the compositor; a dev tool only.

use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_registry::{self, WlRegistry},
        wl_seat::{self, Capability, WlSeat},
        wl_touch::WlTouch,
    },
};

#[derive(Default)]
struct State {
    seat: Option<WlSeat>,
    capabilities: Option<Capability>,
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
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == "wl_seat"
            && state.seat.is_none()
        {
            state.seat = Some(registry.bind(name, 7, queue, ()));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.capabilities = Some(capabilities);
        }
    }
}

delegate_noop!(State: ignore WlTouch);

fn main() {
    let connection = Connection::connect_to_env().expect("no Wayland display");
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let mut state = State::default();
    queue.roundtrip(&mut state).expect("registry roundtrip");
    queue.roundtrip(&mut state).expect("seat roundtrip");

    let seat = state
        .seat
        .clone()
        .expect("compositor advertised no wl_seat");
    let capabilities = state.capabilities.expect("the seat sent no capabilities");
    println!("seat capabilities: {capabilities:?}");
    if !capabilities.contains(Capability::Touch) {
        eprintln!("FAIL: the seat does not offer touch");
        std::process::exit(1);
    }

    // And a client can actually have one: the request a toolkit makes next.
    let touch = seat.get_touch(&handle, ());
    queue.roundtrip(&mut state).expect("touch roundtrip");
    touch.release();
    println!("OK: wl_touch offered and created");
}
