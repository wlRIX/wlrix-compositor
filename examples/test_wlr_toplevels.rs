// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `zwlr_foreign_toplevel_management_v1`: the window list taskbars drive.
//!
//! Lists what the compositor announces and, with an argument, issues one command so the
//! control half is covered too -- listing is easy to get right while `activate` silently does
//! nothing.
//!
//! Usage, with `WAYLAND_DISPLAY` pointing at the compositor under test:
//!   cargo run --example test_wlr_toplevels                # list, then watch
//!   cargo run --example test_wlr_toplevels -- minimize 0  # minimize window #0
//!   cargo run --example test_wlr_toplevels -- activate 0
//!   cargo run --example test_wlr_toplevels -- maximize 0
//!   cargo run --example test_wlr_toplevels -- close 0
//!
//! Not part of the compositor; a dev tool only.

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, State, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

#[derive(Default, Clone)]
struct Toplevel {
    title: String,
    app_id: String,
    states: Vec<u32>,
}

enum Command {
    List,
    Activate(usize),
    Minimize(usize),
    Maximize(usize),
    Close(usize),
}

struct App {
    toplevels: Vec<(ZwlrForeignToplevelHandleV1, Toplevel)>,
    seat: Option<wayland_client::protocol::wl_seat::WlSeat>,
    command: Command,
    done: bool,
}

impl App {
    fn entry(&mut self, handle: &ZwlrForeignToplevelHandleV1) -> Option<&mut Toplevel> {
        self.toplevels
            .iter_mut()
            .find(|(resource, _)| resource == handle)
            .map(|(_, toplevel)| toplevel)
    }

    fn print(&self) {
        println!("--- toplevels ({}) ---", self.toplevels.len());
        for (index, (_, toplevel)) in self.toplevels.iter().enumerate() {
            let mut flags = Vec::new();
            for state in &toplevel.states {
                match State::try_from(*state) {
                    Ok(State::Minimized) => flags.push("min"),
                    Ok(State::Maximized) => flags.push("max"),
                    Ok(State::Activated) => flags.push("active"),
                    _ => {}
                }
            }
            println!(
                "  [{index}] app_id={:?} title={:?} [{}]",
                toplevel.app_id,
                toplevel.title,
                flags.join(",")
            );
        }
    }

    fn run_command(&mut self) {
        let index = match self.command {
            Command::List => return,
            Command::Activate(n)
            | Command::Minimize(n)
            | Command::Maximize(n)
            | Command::Close(n) => n,
        };
        let Some((handle, _)) = self.toplevels.get(index) else {
            println!("no toplevel #{index}");
            return;
        };
        match self.command {
            Command::Activate(_) => match self.seat.as_ref() {
                Some(seat) => {
                    handle.activate(seat);
                    println!("activate #{index}");
                }
                None => println!("no seat; cannot activate"),
            },
            Command::Minimize(_) => {
                handle.set_minimized();
                println!("minimize #{index}");
            }
            Command::Maximize(_) => {
                handle.set_maximized();
                println!("maximize #{index}");
            }
            Command::Close(_) => {
                handle.close();
                println!("close #{index}");
            }
            Command::List => {}
        }
    }
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "zwlr_foreign_toplevel_manager_v1" => {
                    registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, 1, qh, ());
                }
                "wl_seat" => {
                    app.seat = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for App {
    fn event(
        _app: &mut Self,
        _seat: &wayland_client::protocol::wl_seat::WlSeat,
        _event: wayland_client::protocol::wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _manager: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                app.toplevels.push((toplevel, Toplevel::default()));
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => println!("(manager finished)"),
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.title = title;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.app_id = app_id;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                let states = state
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect();
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.states = states;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                app.print();
                if !app.done {
                    app.done = true;
                    app.run_command();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                app.toplevels.retain(|(resource, _)| resource != handle);
                println!("(toplevel closed)");
                app.print();
            }
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let index = |value: Option<&String>| {
        value
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    };
    let command = match args.first().map(String::as_str) {
        Some("activate") => Command::Activate(index(args.get(1))),
        Some("minimize") => Command::Minimize(index(args.get(1))),
        Some("maximize") => Command::Maximize(index(args.get(1))),
        Some("close") => Command::Close(index(args.get(1))),
        _ => Command::List,
    };

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<App>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut app = App {
        toplevels: Vec::new(),
        seat: None,
        command,
        done: false,
    };
    queue.roundtrip(&mut app).expect("initial roundtrip");
    queue.roundtrip(&mut app).expect("toplevel properties");

    println!("(watching for changes; Ctrl+C to quit)");
    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
}
