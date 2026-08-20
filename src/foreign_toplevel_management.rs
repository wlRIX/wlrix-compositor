// SPDX-License-Identifier: GPL-3.0-or-later
//! `zwlr_foreign_toplevel_management_v1`: the window list taskbars and docks actually drive.
//!
//! The read-only [`crate::foreign_toplevel`] list says what exists; this one can also act --
//! activate, minimize, maximize and close. Both are advertised, because clients pick whichever
//! they support, and the bespoke [`crate::desks_protocol`] carries the desk-aware view the
//! Desks Overview needs.
//!
//! Hand-written like [`crate::output_management`] and [`crate::desks_protocol`], since Smithay
//! has no implementation. Same shape as those: a bound manager is advertised a handle per
//! window, structural changes reconcile each client's handles against the current windows, and
//! per-dispatch updates diff title/app-id/state so nothing is re-sent for an idle desktop.
//!
//! **Version 2.** Version 2 is `set_fullscreen`/`unset_fullscreen` and the `fullscreen` state,
//! which wlRIX now has, so it is advertised and implemented. It stops there: version 3 adds a
//! `parent` event, and emitting that means handing a client another *handle* -- the parent
//! window's -- which needs the per-client handle map consulted at emit time and kept correct
//! when a parent is destroyed before its child. The rule the module was written to still holds:
//! everything advertised here is fully implemented, so a client that binds what we offer never
//! sends a request that silently does nothing.

use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{
                self, State as ToplevelState, ZwlrForeignToplevelHandleV1,
            },
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::{ClientId, GlobalId},
        },
    },
};

use crate::{Wlrix, desks};

/// Protocol version implemented. See the module note on why this is not 3.
const VERSION: u32 = 2;

/// The flags carried in the `state` event: (minimized, maximized, activated, fullscreen).
type StateFlags = (bool, bool, bool, bool);

/// What a taskbar is told about one window, snapshotted so the emit does not borrow the model.
pub struct ToplevelInfo {
    pub window: Window,
    pub title: String,
    pub app_id: String,
    pub states: StateFlags,
    pub outputs: Vec<Output>,
}

pub struct ForeignToplevelManagementState {
    instances: Vec<ManagerInstance>,
}

struct ManagerInstance {
    manager: ZwlrForeignToplevelManagerV1,
    handles: Vec<HandleResource>,
}

struct HandleResource {
    window: Window,
    resource: ZwlrForeignToplevelHandleV1,
    /// Last values sent, so the per-dispatch pass emits only real changes.
    last_title: String,
    last_app_id: String,
    last_states: StateFlags,
}

impl ForeignToplevelManagementState {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }

    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ZwlrForeignToplevelManagerV1, _>(VERSION, ())
    }

    fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Advertise every window to a manager that has just bound.
    fn advertise(
        &mut self,
        client: &Client,
        display: &DisplayHandle,
        manager: ZwlrForeignToplevelManagerV1,
        toplevels: &[ToplevelInfo],
    ) {
        let mut instance = ManagerInstance {
            manager,
            handles: Vec::new(),
        };
        for info in toplevels {
            add_handle(
                client,
                display,
                &instance.manager,
                info,
                &mut instance.handles,
            );
        }
        self.instances.push(instance);
    }

    /// Reconcile each client's handles with the current windows, then push any changes. Called
    /// once per event-loop dispatch; cheap when nothing moved.
    fn refresh(&mut self, display: &DisplayHandle, toplevels: &[ToplevelInfo]) {
        self.instances
            .retain(|instance| instance.manager.is_alive());

        for instance in &mut self.instances {
            let Some(client) = instance.manager.client() else {
                continue;
            };

            // Gone windows: tell the client, then drop the handle.
            instance.handles.retain(|handle| {
                let kept = toplevels.iter().any(|info| info.window == handle.window);
                if !kept {
                    handle.resource.closed();
                }
                kept
            });

            for info in toplevels {
                match instance
                    .handles
                    .iter_mut()
                    .find(|handle| handle.window == info.window)
                {
                    Some(handle) => refresh_handle(handle, info),
                    None => add_handle(
                        &client,
                        display,
                        &instance.manager,
                        info,
                        &mut instance.handles,
                    ),
                }
            }
        }
    }

    fn forget_manager(&mut self, manager: &ZwlrForeignToplevelManagerV1) {
        self.instances
            .retain(|instance| &instance.manager != manager);
    }

    fn forget_handle(&mut self, resource: &ZwlrForeignToplevelHandleV1) {
        for instance in &mut self.instances {
            instance
                .handles
                .retain(|handle| &handle.resource != resource);
        }
    }
}

impl Default for ForeignToplevelManagementState {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a handle for `info` on `manager` and send its full state.
fn add_handle(
    client: &Client,
    display: &DisplayHandle,
    manager: &ZwlrForeignToplevelManagerV1,
    info: &ToplevelInfo,
    handles: &mut Vec<HandleResource>,
) {
    let Ok(resource) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, Wlrix>(
        display,
        manager.version(),
        info.window.clone(),
    ) else {
        return;
    };
    manager.toplevel(&resource);

    resource.title(info.title.clone());
    resource.app_id(info.app_id.clone());
    for output in &info.outputs {
        for wl_output in output.client_outputs(client) {
            resource.output_enter(&wl_output);
        }
    }
    resource.state(encode_states(info.states));
    resource.done();

    handles.push(HandleResource {
        window: info.window.clone(),
        resource,
        last_title: info.title.clone(),
        last_app_id: info.app_id.clone(),
        last_states: info.states,
    });
}

/// Emit whatever changed for an existing handle.
fn refresh_handle(handle: &mut HandleResource, info: &ToplevelInfo) {
    let mut changed = false;
    if handle.last_title != info.title {
        handle.resource.title(info.title.clone());
        handle.last_title = info.title.clone();
        changed = true;
    }
    if handle.last_app_id != info.app_id {
        handle.resource.app_id(info.app_id.clone());
        handle.last_app_id = info.app_id.clone();
        changed = true;
    }
    if handle.last_states != info.states {
        handle.resource.state(encode_states(info.states));
        handle.last_states = info.states;
        changed = true;
    }
    if changed {
        handle.resource.done();
    }
}

/// The `state` event's array: 32-bit values, native endian, one per set flag.
///
/// `Fullscreen` is a version 2 value. Unlike an *event* added in a later version, a new entry in
/// an existing array is safe to send to a version 1 client: the protocol tells clients to ignore
/// values they do not know, and this array has always been variable-length.
fn encode_states((minimized, maximized, activated, fullscreen): StateFlags) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (set, flag) in [
        (minimized, ToplevelState::Minimized),
        (maximized, ToplevelState::Maximized),
        (activated, ToplevelState::Activated),
        (fullscreen, ToplevelState::Fullscreen),
    ] {
        if set {
            bytes.extend_from_slice(&(flag as u32).to_ne_bytes());
        }
    }
    bytes
}

impl Wlrix {
    /// What every taskbar-visible window looks like right now.
    ///
    /// Override-redirect X11 surfaces are menus and tooltips, not application windows, so they
    /// are left out -- the same rule the read-only list uses.
    fn toplevel_infos(&self) -> Vec<ToplevelInfo> {
        let focused = self.focused_window();
        self.space
            .elements()
            .cloned()
            .chain(self.desks.hidden().iter().cloned())
            .filter(|window| {
                !window
                    .x11_surface()
                    .is_some_and(|surface| surface.is_override_redirect())
            })
            .map(|window| {
                let (minimized, maximized, fullscreen) = {
                    let state = desks::window_state(&window).borrow();
                    (state.minimized, state.maximized, state.fullscreen)
                };
                ToplevelInfo {
                    title: crate::frame::window_title(&window),
                    app_id: crate::placement::app_id(&window).unwrap_or_default(),
                    states: (
                        minimized,
                        maximized,
                        focused.as_ref() == Some(&window),
                        fullscreen,
                    ),
                    outputs: self.space.outputs_for_element(&window),
                    window,
                }
            })
            .collect()
    }

    /// Push window changes to taskbars. Called once per event-loop dispatch.
    pub fn refresh_foreign_toplevel_management(&mut self) {
        if self.foreign_toplevel_management.is_empty() {
            return;
        }
        // Snapshotted first: `refresh` needs the protocol state mutably.
        let toplevels = self.toplevel_infos();
        let display = self.display_handle.clone();
        self.foreign_toplevel_management
            .refresh(&display, &toplevels);
    }
}

//
// Manager
//

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for Wlrix {
    fn bind(
        state: &mut Self,
        display: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let toplevels = state.toplevel_infos();
        state
            .foreign_toplevel_management
            .advertise(client, display, manager, &toplevels);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // The client is done watching: confirm, and it will destroy the manager.
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            resource.finished();
            state.foreign_toplevel_management.forget_manager(resource);
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state.foreign_toplevel_management.forget_manager(resource);
    }
}

//
// Toplevel handle
//

impl Dispatch<ZwlrForeignToplevelHandleV1, Window> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        window: &Window,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let window = window.clone();
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.maximize_window(&window)
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.unmaximize_window(&window)
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.minimize_window(&window)
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                state.restore_window(&window)
            }
            // Version 2. The output is optional here as it is in xdg-shell, and means the same
            // thing: fill the monitor the client names, or let the compositor choose.
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                let output = output.as_ref().and_then(Output::from_resource);
                state.fullscreen_window(&window, output)
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.unfullscreen_window(&window)
            }
            // Focus and raise, which is what a taskbar click means.
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.restore_window(&window);
                crate::focus::focus_window(state, &window);
                state.request_redraw();
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => state.close_window(&window),
            // Where the taskbar draws this window, for a minimize animation. wlRIX does not
            // animate minimizing, so there is nothing to remember.
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        _data: &Window,
    ) {
        state.foreign_toplevel_management.forget_handle(resource);
    }
}
