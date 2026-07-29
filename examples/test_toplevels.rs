// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `ext-foreign-toplevel-list-v1`: the read-only window list a taskbar reads.
//!
//! Advertising the global is not the same as populating it, so this binds the list and prints
//! every toplevel the compositor announces, then keeps running to show live title changes and
//! windows opening and closing.
//!
//! Usage: `cargo run --example test_toplevels` with `WAYLAND_DISPLAY` set to the compositor
//! under test. Not part of the compositor; a dev tool only.

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

#[derive(Default)]
struct Toplevel {
    identifier: String,
    title: String,
    app_id: String,
}

#[derive(Default)]
struct App {
    toplevels: Vec<(ExtForeignToplevelHandleV1, Toplevel)>,
}

impl App {
    fn entry(&mut self, handle: &ExtForeignToplevelHandleV1) -> Option<&mut Toplevel> {
        self.toplevels
            .iter_mut()
            .find(|(resource, _)| resource == handle)
            .map(|(_, toplevel)| toplevel)
    }

    fn print(&self) {
        println!("--- toplevels ({}) ---", self.toplevels.len());
        for (index, (_, toplevel)) in self.toplevels.iter().enumerate() {
            println!(
                "  [{index}] app_id={:?} title={:?} id={}",
                toplevel.app_id, toplevel.title, toplevel.identifier
            );
        }
    }
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        _app: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == "ext_foreign_toplevel_list_v1"
        {
            registry.bind::<ExtForeignToplevelListV1, _, _>(name, 1, qh, ());
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for App {
    fn event(
        app: &mut Self,
        _list: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                app.toplevels.push((toplevel, Toplevel::default()));
            }
            ext_foreign_toplevel_list_v1::Event::Finished => println!("(list finished)"),
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.identifier = identifier;
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.title = title;
                }
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(toplevel) = app.entry(handle) {
                    toplevel.app_id = app_id;
                }
            }
            // Everything since the last `done` has arrived: the list is consistent again.
            ext_foreign_toplevel_handle_v1::Event::Done => app.print(),
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                app.toplevels.retain(|(resource, _)| resource != handle);
                println!("(toplevel closed)");
                app.print();
            }
            _ => {}
        }
    }
}

fn main() {
    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<App>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut app = App::default();
    // Binds the list and receives the initial batch of toplevels.
    queue.roundtrip(&mut app).expect("initial roundtrip");
    queue.roundtrip(&mut app).expect("toplevel properties");
    app.print();

    println!("(watching for changes; Ctrl+C to quit)");
    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
}
