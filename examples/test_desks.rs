// SPDX-License-Identifier: GPL-3.0-or-later
//! A throwaway client for exercising the `wlrix-desks` protocol against the compositor.
//!
//! There is no Desks app yet, so this stands in: it binds the desks manager, prints the
//! desks and windows the compositor advertises (re-printing whenever a `done` batch lands,
//! so live geometry and state changes show), and can issue one command to prove the request
//! path.
//!
//! Usage, with `WAYLAND_DISPLAY` pointing at the compositor under test:
//!   cargo run --example test_desks                     # list, then watch for updates
//!   cargo run --example test_desks -- create           # create a desk
//!   cargo run --example test_desks -- activate <n>     # switch to desk #n (from the list)
//!   cargo run --example test_desks -- remove <n>       # delete desk #n
//!   cargo run --example test_desks -- maximize <n>     # maximize window #n
//!   cargo run --example test_desks -- minimize <n>     # minimize window #n
//!   cargo run --example test_desks -- move <win> <desk> # move window #win to desk #desk
//! Not part of the compositor; a dev tool only.

use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};

// Client-side bindings, generated from the same XML the compositor's server side uses.
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    clippy::all,
    clippy::pedantic
)]
mod wlrix_desks {
    use wayland_client::backend as wayland_backend;
    use wayland_client::{self, protocol::*};

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("src/protocols/wlrix-desks.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("src/protocols/wlrix-desks.xml");
}

use wlrix_desks::{
    wlrix_desk_v1::{self, WlrixDeskV1},
    wlrix_desks_manager_v1::{self, WlrixDesksManagerV1},
    wlrix_toplevel_v1::{self, WlrixToplevelV1},
};

/// One command to run once, parsed from argv.
enum Command {
    List,
    Create,
    Activate(usize),
    Remove(usize),
    Maximize(usize),
    Minimize(usize),
    Move(usize, usize),
}

#[derive(Default)]
struct Desk {
    object: Option<WlrixDeskV1>,
    id: u32,
    name: String,
    global: bool,
    active: bool,
}

#[derive(Default)]
struct Toplevel {
    object: Option<WlrixToplevelV1>,
    app_id: String,
    title: String,
    geometry: (i32, i32, i32, i32),
    minimized: bool,
    maximized: bool,
    activated: bool,
    desk: Option<WlrixDeskV1>,
}

struct App {
    desks: Vec<Desk>,
    toplevels: Vec<Toplevel>,
    command: Command,
    command_done: bool,
}

impl App {
    fn desk_mut(&mut self, object: &WlrixDeskV1) -> Option<&mut Desk> {
        self.desks
            .iter_mut()
            .find(|d| d.object.as_ref() == Some(object))
    }

    fn toplevel_mut(&mut self, object: &WlrixToplevelV1) -> Option<&mut Toplevel> {
        self.toplevels
            .iter_mut()
            .find(|t| t.object.as_ref() == Some(object))
    }

    fn print(&self) {
        println!("--- desks ---");
        for (i, desk) in self.desks.iter().enumerate() {
            println!(
                "  [{i}] id={} {}{}{}",
                desk.id,
                desk.name,
                if desk.global { " (global)" } else { "" },
                if desk.active { " *active*" } else { "" },
            );
        }
        println!("--- windows ---");
        for (i, top) in self.toplevels.iter().enumerate() {
            let (x, y, w, h) = top.geometry;
            let desk = top
                .desk
                .as_ref()
                .and_then(|d| self.desks.iter().position(|k| k.object.as_ref() == Some(d)))
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let mut flags = Vec::new();
            if top.minimized {
                flags.push("min");
            }
            if top.maximized {
                flags.push("max");
            }
            if top.activated {
                flags.push("active");
            }
            println!(
                "  [{i}] {:?} \"{}\" desk={desk} {x},{y} {w}x{h} [{}]",
                top.app_id,
                top.title,
                flags.join(","),
            );
        }
        println!();
    }

    fn run_command(&mut self, manager: &WlrixDesksManagerV1) {
        match self.command {
            Command::List => {}
            Command::Create => manager.create_desk(),
            Command::Activate(n) => {
                if let Some(desk) = self.desks.get(n).and_then(|d| d.object.as_ref()) {
                    desk.activate();
                }
            }
            Command::Remove(n) => {
                if let Some(desk) = self.desks.get(n).and_then(|d| d.object.as_ref()) {
                    desk.remove();
                }
            }
            Command::Maximize(n) => {
                if let Some(top) = self.toplevels.get(n).and_then(|t| t.object.as_ref()) {
                    top.maximize();
                }
            }
            Command::Minimize(n) => {
                if let Some(top) = self.toplevels.get(n).and_then(|t| t.object.as_ref()) {
                    top.minimize();
                }
            }
            Command::Move(win, desk) => {
                let target = self.desks.get(desk).and_then(|d| d.object.clone());
                if let (Some(top), Some(target)) = (
                    self.toplevels.get(win).and_then(|t| t.object.as_ref()),
                    target,
                ) {
                    top.move_to_desk(&target);
                }
            }
        }
    }
}

fn main() {
    let command = parse_command();

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<App>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut app = App {
        desks: Vec::new(),
        toplevels: Vec::new(),
        command,
        command_done: false,
    };

    // First roundtrip binds the manager (via the registry) and receives the initial batch.
    queue.roundtrip(&mut app).expect("initial roundtrip");

    // Keep dispatching so live updates (geometry, state, new windows) keep printing.
    println!("(watching for updates; Ctrl+C to quit)");
    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
}

fn parse_command() -> Command {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let index = |s: Option<&String>| s.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    match args.first().map(String::as_str) {
        Some("create") => Command::Create,
        Some("activate") => Command::Activate(index(args.get(1))),
        Some("remove") => Command::Remove(index(args.get(1))),
        Some("maximize") => Command::Maximize(index(args.get(1))),
        Some("minimize") => Command::Minimize(index(args.get(1))),
        Some("move") => Command::Move(index(args.get(1)), index(args.get(2))),
        _ => Command::List,
    }
}

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()> for App {
    fn event(
        _app: &mut Self,
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        if let Event::Global {
            name, interface, ..
        } = event
            && interface == "wlrix_desks_manager_v1"
        {
            registry.bind::<WlrixDesksManagerV1, _, _>(name, 1, qh, ());
        }
    }
}

impl Dispatch<WlrixDesksManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        manager: &WlrixDesksManagerV1,
        event: wlrix_desks_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wlrix_desks_manager_v1::Event::Desk { desk } => app.desks.push(Desk {
                object: Some(desk),
                ..Default::default()
            }),
            wlrix_desks_manager_v1::Event::Toplevel { toplevel } => app.toplevels.push(Toplevel {
                object: Some(toplevel),
                ..Default::default()
            }),
            wlrix_desks_manager_v1::Event::Done => {
                app.print();
                if !app.command_done {
                    app.command_done = true;
                    app.run_command(manager);
                }
            }
            wlrix_desks_manager_v1::Event::Finished => println!("(manager finished)"),
        }
    }

    event_created_child!(App, WlrixDesksManagerV1, [
        wlrix_desks_manager_v1::EVT_DESK_OPCODE => (WlrixDeskV1, ()),
        wlrix_desks_manager_v1::EVT_TOPLEVEL_OPCODE => (WlrixToplevelV1, ()),
    ]);
}

impl Dispatch<WlrixDeskV1, ()> for App {
    fn event(
        app: &mut Self,
        desk: &WlrixDeskV1,
        event: wlrix_desk_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = app.desk_mut(desk) else {
            return;
        };
        match event {
            wlrix_desk_v1::Event::Id { id } => entry.id = id,
            wlrix_desk_v1::Event::Name { name } => entry.name = name,
            wlrix_desk_v1::Event::Global => entry.global = true,
            wlrix_desk_v1::Event::Activated => entry.active = true,
            wlrix_desk_v1::Event::Deactivated => entry.active = false,
            wlrix_desk_v1::Event::Removed => {
                app.desks.retain(|d| d.object.as_ref() != Some(desk));
            }
        }
    }
}

impl Dispatch<WlrixToplevelV1, ()> for App {
    fn event(
        app: &mut Self,
        toplevel: &WlrixToplevelV1,
        event: wlrix_toplevel_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Closed removes the entry; the rest update it. Each arm does its own lookup so the
        // whole enum is covered without a catch-all (the generated Event enum is exhaustive).
        match event {
            wlrix_toplevel_v1::Event::Closed => {
                app.toplevels
                    .retain(|t| t.object.as_ref() != Some(toplevel));
            }
            wlrix_toplevel_v1::Event::AppId { app_id } => {
                if let Some(entry) = app.toplevel_mut(toplevel) {
                    entry.app_id = app_id;
                }
            }
            wlrix_toplevel_v1::Event::Title { title } => {
                if let Some(entry) = app.toplevel_mut(toplevel) {
                    entry.title = title;
                }
            }
            wlrix_toplevel_v1::Event::Geometry {
                x,
                y,
                width,
                height,
            } => {
                if let Some(entry) = app.toplevel_mut(toplevel) {
                    entry.geometry = (x, y, width, height);
                }
            }
            wlrix_toplevel_v1::Event::State { state } => {
                let has = |flag: wlrix_toplevel_v1::State| {
                    state
                        .chunks_exact(4)
                        .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == flag as u32)
                };
                if let Some(entry) = app.toplevel_mut(toplevel) {
                    entry.minimized = has(wlrix_toplevel_v1::State::Minimized);
                    entry.maximized = has(wlrix_toplevel_v1::State::Maximized);
                    entry.activated = has(wlrix_toplevel_v1::State::Activated);
                }
            }
            wlrix_toplevel_v1::Event::Desk { desk } => {
                if let Some(entry) = app.toplevel_mut(toplevel) {
                    entry.desk = Some(desk);
                }
            }
        }
    }
}
