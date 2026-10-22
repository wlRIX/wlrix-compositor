// SPDX-License-Identifier: GPL-3.0-or-later
//! Set every enabled output to one scale through `wlr-output-management`, as the Displays panel
//! does -- for trying layout changes on a nested compositor, where there is no panel to click.
//!
//! ```sh
//! WAYLAND_DISPLAY=wayland-2 cargo run --example set_scale -- 1.5
//! ```
//!
//! Exits non-zero if the compositor refuses the configuration. Not part of the program.

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};

#[derive(Default)]
struct State {
    manager: Option<ZwlrOutputManagerV1>,
    heads: Vec<(ZwlrOutputHeadV1, bool)>,
    serial: Option<u32>,
    outcome: Option<bool>,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == ZwlrOutputManagerV1::interface().name
        {
            state.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push((head, false)),
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_head_v1::Event::Enabled { enabled } = event
            && let Some(entry) = state.heads.iter_mut().find(|(known, _)| known == head)
        {
            entry.1 = enabled != 0;
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => state.outcome = Some(true),
            zwlr_output_configuration_v1::Event::Failed
            | zwlr_output_configuration_v1::Event::Cancelled => state.outcome = Some(false),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore ZwlrOutputModeV1);
wayland_client::delegate_noop!(State: ignore ZwlrOutputConfigurationHeadV1);

fn main() {
    let scale: f64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .expect("usage: set_scale <scale>");
    let conn = Connection::connect_to_env().expect("no Wayland display");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    while state.serial.is_none() {
        queue.blocking_dispatch(&mut state).expect("dispatch");
    }
    let manager = state.manager.clone().expect("no wlr-output-management");
    let configuration = manager.create_configuration(state.serial.unwrap(), &qh, ());
    for (head, enabled) in &state.heads {
        if *enabled {
            configuration.enable_head(head, &qh, ()).set_scale(scale);
        } else {
            configuration.disable_head(head);
        }
    }
    configuration.apply();
    while state.outcome.is_none() {
        queue.blocking_dispatch(&mut state).expect("dispatch");
    }
    if state.outcome == Some(true) {
        println!("scale {scale} applied");
    } else {
        eprintln!("the compositor refused scale {scale}");
        std::process::exit(1);
    }
}
