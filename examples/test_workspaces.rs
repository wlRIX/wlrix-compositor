// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `ext-workspace-v1`: the standard desk view a pager reads.
//!
//! Lists the workspace group and its workspaces, and with an argument issues one commit-staged
//! command, so the control half is covered as well as the listing.
//!
//! Usage, with `WAYLAND_DISPLAY` pointing at the compositor under test:
//!   cargo run --example test_workspaces              # list, then watch
//!   cargo run --example test_workspaces -- activate 1
//!   cargo run --example test_workspaces -- create
//!   cargo run --example test_workspaces -- remove 1
//!
//! Not part of the compositor; a dev tool only.

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

#[derive(Default, Clone)]
struct Workspace {
    id: String,
    name: String,
    active: bool,
}

enum Command {
    List,
    Activate(usize),
    Remove(usize),
    Create,
}

struct App {
    manager: Option<ExtWorkspaceManagerV1>,
    group: Option<ExtWorkspaceGroupHandleV1>,
    workspaces: Vec<(ExtWorkspaceHandleV1, Workspace)>,
    command: Command,
    done: bool,
}

impl App {
    fn entry(&mut self, handle: &ExtWorkspaceHandleV1) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|(resource, _)| resource == handle)
            .map(|(_, workspace)| workspace)
    }

    fn print(&self) {
        println!(
            "--- workspaces ({}) group={} ---",
            self.workspaces.len(),
            if self.group.is_some() { "yes" } else { "none" }
        );
        for (index, (_, workspace)) in self.workspaces.iter().enumerate() {
            println!(
                "  [{index}] id={} name={:?}{}",
                workspace.id,
                workspace.name,
                if workspace.active { " *active*" } else { "" }
            );
        }
    }

    fn run_command(&mut self) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        match self.command {
            Command::List => return,
            Command::Create => match self.group.as_ref() {
                Some(group) => {
                    group.create_workspace("from-probe".into());
                    println!("create workspace");
                }
                None => println!("no group; cannot create"),
            },
            Command::Activate(index) => match self.workspaces.get(index) {
                Some((handle, _)) => {
                    handle.activate();
                    println!("activate #{index}");
                }
                None => println!("no workspace #{index}"),
            },
            Command::Remove(index) => match self.workspaces.get(index) {
                Some((handle, _)) => {
                    handle.remove();
                    println!("remove #{index}");
                }
                None => println!("no workspace #{index}"),
            },
        }
        // Staged requests only take effect here.
        manager.commit();
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
            && interface == "ext_workspace_manager_v1"
        {
            registry.bind::<ExtWorkspaceManagerV1, _, _>(name, 1, qh, ());
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        manager: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                app.group = Some(workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                app.workspaces.push((workspace, Workspace::default()));
            }
            // The batch is consistent: everything since the last `done` has arrived.
            ext_workspace_manager_v1::Event::Done => {
                app.manager = Some(manager.clone());
                app.print();
                if !app.done {
                    app.done = true;
                    app.run_command();
                }
            }
            ext_workspace_manager_v1::Event::Finished => println!("(manager finished)"),
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        _app: &mut Self,
        _group: &ExtWorkspaceGroupHandleV1,
        _event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_handle_v1::Event::Id { id } => {
                if let Some(workspace) = app.entry(handle) {
                    workspace.id = id;
                }
            }
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some(workspace) = app.entry(handle) {
                    workspace.name = name;
                }
            }
            ext_workspace_handle_v1::Event::State { state } => {
                let active = matches!(
                    state.into_result(),
                    Ok(ext_workspace_handle_v1::State::Active)
                );
                if let Some(workspace) = app.entry(handle) {
                    workspace.active = active;
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                app.workspaces.retain(|(resource, _)| resource != handle);
                println!("(workspace removed)");
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
        Some("remove") => Command::Remove(index(args.get(1))),
        Some("create") => Command::Create,
        _ => Command::List,
    };

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<App>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut app = App {
        manager: None,
        group: None,
        workspaces: Vec::new(),
        command,
        done: false,
    };
    queue.roundtrip(&mut app).expect("initial roundtrip");
    queue.roundtrip(&mut app).expect("workspace properties");

    println!("(watching for changes; Ctrl+C to quit)");
    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
}
