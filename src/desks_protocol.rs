// SPDX-License-Identifier: GPL-3.0-or-later
//! Server side of the bespoke `wlrix-desks` protocol (`src/protocols/wlrix-desks.xml`).
//!
//! Hand-written like [`crate::output_management`], since Smithay has no handler for a
//! wlRIX-specific protocol. A bound manager is advertised every desk and every window as
//! child objects, each with its properties, closed by `done`. On any structural change the
//! server reconciles each client's objects against the current model -- adding new ones,
//! sending `removed`/`closed` for gone ones, and re-emitting properties -- via
//! [`Wlrix::desks_changed`]. Requests route into the same [`Wlrix`] methods the temporary
//! keybinds and xdg requests use.

use smithay::{
    desktop::Window,
    reexports::wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        backend::{ClientId, GlobalId},
    },
    utils::{Logical, Rectangle},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    Wlrix,
    desks::{self, DeskId},
    protocols::wlrix_desks::{
        wlrix_desk_v1::{self, WlrixDeskV1},
        wlrix_desks_manager_v1::{self, WlrixDesksManagerV1},
        wlrix_toplevel_v1::{self, WlrixToplevelV1},
    },
};

/// Protocol version we implement. Every version-gated event must be guarded at the send
/// site, because a client binds at *its* version, not the one advertised.
const VERSION: u32 = 1;

/// Server state for the desks global: the managers clients have bound, each with the desk
/// and toplevel objects advertised to it.
pub struct DesksProtocolState {
    instances: Vec<ManagerInstance>,
}

struct ManagerInstance {
    manager: WlrixDesksManagerV1,
    desks: Vec<DeskResource>,
    toplevels: Vec<ToplevelResource>,
}

struct DeskResource {
    id: DeskId,
    resource: WlrixDeskV1,
}

struct ToplevelResource {
    window: Window,
    resource: WlrixToplevelV1,
    /// Last geometry/state emitted, so the per-dispatch update pass sends only real changes
    /// rather than re-emitting every dispatch.
    last_geometry: (i32, i32, i32, i32),
    last_flags: StateFlags,
}

/// The dynamic flags emitted in the `state` event: (minimized, maximized, activated).
type StateFlags = (bool, bool, bool);

/// A desk, snapshotted from the model so the protocol emit does not borrow it live.
struct DeskSnapshot {
    id: DeskId,
    name: String,
    global: bool,
    active: bool,
}

/// A window, snapshotted from the model.
struct WindowSnapshot {
    window: Window,
    app_id: String,
    title: String,
    geometry: Rectangle<i32, Logical>,
    minimized: bool,
    maximized: bool,
    activated: bool,
    desk: DeskId,
}

impl DesksProtocolState {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }

    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, WlrixDesksManagerV1, _>(VERSION, ())
    }

    fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Advertise the whole model to a manager that has just bound.
    fn advertise(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: WlrixDesksManagerV1,
        desks: &[DeskSnapshot],
        windows: &[WindowSnapshot],
    ) {
        let mut instance = ManagerInstance {
            manager,
            desks: Vec::new(),
            toplevels: Vec::new(),
        };
        for desk in desks {
            add_desk(
                display,
                client,
                &instance.manager,
                desk,
                &mut instance.desks,
            );
        }
        for window in windows {
            add_toplevel(
                display,
                client,
                &instance.manager,
                window,
                &instance.desks,
                &mut instance.toplevels,
            );
        }
        instance.manager.done();
        self.instances.push(instance);
    }

    /// Reconcile every client's objects with the current model after a structural change.
    fn reconcile(
        &mut self,
        display: &DisplayHandle,
        desks: &[DeskSnapshot],
        windows: &[WindowSnapshot],
    ) {
        self.instances
            .retain(|instance| instance.manager.is_alive());

        for instance in &mut self.instances {
            let Some(client) = instance.manager.client() else {
                continue;
            };

            // Desks: drop the gone, add the new, refresh the rest.
            instance.desks.retain(|desk| {
                let kept = desks.iter().any(|snap| snap.id == desk.id);
                if !kept {
                    desk.resource.removed();
                }
                kept
            });
            for snap in desks {
                match instance.desks.iter().find(|desk| desk.id == snap.id) {
                    Some(desk) => refresh_desk(&desk.resource, snap),
                    None => add_desk(
                        display,
                        &client,
                        &instance.manager,
                        snap,
                        &mut instance.desks,
                    ),
                }
            }

            // Toplevels: same shape.
            instance.toplevels.retain(|toplevel| {
                let kept = windows.iter().any(|snap| snap.window == toplevel.window);
                if !kept {
                    toplevel.resource.closed();
                }
                kept
            });
            for snap in windows {
                let desk = desk_resource(&instance.desks, snap.desk);
                match instance
                    .toplevels
                    .iter()
                    .find(|toplevel| toplevel.window == snap.window)
                {
                    // Existing: only the identity (app_id/title/desk) is re-emitted here;
                    // geometry and state are the per-dispatch update pass's job.
                    Some(toplevel) => refresh_toplevel(&toplevel.resource, snap, desk.as_ref()),
                    None => add_toplevel(
                        display,
                        &client,
                        &instance.manager,
                        snap,
                        &instance.desks,
                        &mut instance.toplevels,
                    ),
                }
            }

            instance.manager.done();
        }
    }

    fn forget_manager(&mut self, manager: &WlrixDesksManagerV1) {
        self.instances
            .retain(|instance| &instance.manager != manager);
    }

    fn forget_desk(&mut self, resource: &WlrixDeskV1) {
        for instance in &mut self.instances {
            instance.desks.retain(|desk| &desk.resource != resource);
        }
    }

    fn forget_toplevel(&mut self, resource: &WlrixToplevelV1) {
        for instance in &mut self.instances {
            instance
                .toplevels
                .retain(|toplevel| &toplevel.resource != resource);
        }
    }

    /// Emit live geometry and state, diffed against what was last sent so a dragged or
    /// resized window streams updates without flooding when nothing changed. Called once per
    /// event-loop dispatch. Only mapped windows (active/global desk) can change geometry, but
    /// **every** tracked window is checked for state changes -- minimizing unmaps a window, so
    /// looking only at mapped ones would never report the minimize that caused it.
    fn emit_updates(
        &mut self,
        geometries: &[(Window, Rectangle<i32, Logical>)],
        focused: Option<&Window>,
    ) {
        self.instances
            .retain(|instance| instance.manager.is_alive());

        for instance in &mut self.instances {
            let mut changed = false;
            for toplevel in &mut instance.toplevels {
                // Geometry: the window's rectangle on the desk, or its icon's tile while
                // minimized. A window with neither (on an inactive desk) keeps the last geometry
                // sent, which is where it will reappear.
                if let Some(geometry) = geometries
                    .iter()
                    .find(|(window, _)| window == &toplevel.window)
                    .map(|(_, geometry)| *geometry)
                {
                    let geometry = geometry_tuple(geometry);
                    if toplevel.last_geometry != geometry {
                        toplevel
                            .resource
                            .geometry(geometry.0, geometry.1, geometry.2, geometry.3);
                        toplevel.last_geometry = geometry;
                        changed = true;
                    }
                }

                // State: emitted whether or not the window is mapped. Minimizing is precisely
                // the case where it is not, so skipping unmapped windows here would leave a
                // client believing the window is still on screen -- the `reconcile` pass does
                // not re-send state either.
                let flags = {
                    let state = desks::window_state(&toplevel.window).borrow();
                    (
                        state.minimized,
                        state.maximized,
                        focused == Some(&toplevel.window),
                    )
                };
                if toplevel.last_flags != flags {
                    toplevel
                        .resource
                        .state(encode_states(flags.0, flags.1, flags.2, false));
                    toplevel.last_flags = flags;
                    changed = true;
                }
            }
            if changed {
                instance.manager.done();
            }
        }
    }
}

/// The desk object for `id` within one manager instance, for a toplevel's `desk` event.
fn desk_resource(desks: &[DeskResource], id: DeskId) -> Option<WlrixDeskV1> {
    desks
        .iter()
        .find(|desk| desk.id == id)
        .map(|desk| desk.resource.clone())
}

/// Create and describe a desk object.
fn add_desk(
    display: &DisplayHandle,
    client: &Client,
    manager: &WlrixDesksManagerV1,
    snap: &DeskSnapshot,
    into: &mut Vec<DeskResource>,
) {
    let Ok(resource) =
        client.create_resource::<WlrixDeskV1, _, Wlrix>(display, manager.version(), snap.id)
    else {
        return;
    };
    manager.desk(&resource);
    resource.id(snap.id.0);
    resource.name(snap.name.clone());
    if snap.global {
        resource.global();
    }
    if snap.active {
        resource.activated();
    }
    into.push(DeskResource {
        id: snap.id,
        resource,
    });
}

/// Re-emit a desk's mutable properties on a reconcile.
fn refresh_desk(resource: &WlrixDeskV1, snap: &DeskSnapshot) {
    resource.name(snap.name.clone());
    if snap.active {
        resource.activated();
    } else {
        resource.deactivated();
    }
}

/// Create and describe a toplevel object.
fn add_toplevel(
    display: &DisplayHandle,
    client: &Client,
    manager: &WlrixDesksManagerV1,
    snap: &WindowSnapshot,
    desks: &[DeskResource],
    into: &mut Vec<ToplevelResource>,
) {
    let Ok(resource) = client.create_resource::<WlrixToplevelV1, _, Wlrix>(
        display,
        manager.version(),
        snap.window.clone(),
    ) else {
        return;
    };
    manager.toplevel(&resource);
    send_toplevel_props(&resource, snap, desk_resource(desks, snap.desk).as_ref());
    into.push(ToplevelResource {
        window: snap.window.clone(),
        resource,
        last_geometry: geometry_tuple(snap.geometry),
        last_flags: (snap.minimized, snap.maximized, snap.activated),
    });
}

/// Re-emit a toplevel's identity properties on a reconcile (geometry and state are the
/// per-dispatch update pass's responsibility).
fn refresh_toplevel(resource: &WlrixToplevelV1, snap: &WindowSnapshot, desk: Option<&WlrixDeskV1>) {
    resource.app_id(snap.app_id.clone());
    resource.title(snap.title.clone());
    if let Some(desk) = desk {
        resource.desk(desk);
    }
}

fn geometry_tuple(geometry: Rectangle<i32, Logical>) -> (i32, i32, i32, i32) {
    (
        geometry.loc.x,
        geometry.loc.y,
        geometry.size.w,
        geometry.size.h,
    )
}

/// Emit a toplevel's properties (all of them; called on advertise and reconcile, not per
/// frame -- live geometry has its own cached path).
fn send_toplevel_props(
    resource: &WlrixToplevelV1,
    snap: &WindowSnapshot,
    desk: Option<&WlrixDeskV1>,
) {
    resource.app_id(snap.app_id.clone());
    resource.title(snap.title.clone());
    resource.geometry(
        snap.geometry.loc.x,
        snap.geometry.loc.y,
        snap.geometry.size.w,
        snap.geometry.size.h,
    );
    resource.state(encode_states(
        snap.minimized,
        snap.maximized,
        snap.activated,
        false,
    ));
    if let Some(desk) = desk {
        resource.desk(desk);
    }
}

/// Pack the set state flags into the `state` event's array, each a native-endian u32.
fn encode_states(minimized: bool, maximized: bool, activated: bool, fullscreen: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let flags = [
        (minimized, wlrix_toplevel_v1::State::Minimized),
        (maximized, wlrix_toplevel_v1::State::Maximized),
        (activated, wlrix_toplevel_v1::State::Activated),
        (fullscreen, wlrix_toplevel_v1::State::Fullscreen),
    ];
    for (set, flag) in flags {
        if set {
            bytes.extend_from_slice(&(flag as u32).to_ne_bytes());
        }
    }
    bytes
}

impl Wlrix {
    /// Re-advertise desks and windows to every `wlrix-desks` client after a structural
    /// change (desk added/removed/renamed/activated, window added/removed/moved, or a
    /// window's state changing). A no-op when no client is bound.
    pub fn desks_changed(&mut self) {
        // The standard `ext-workspace-v1` view of the same model, kept in step from here so a
        // caller cannot update one protocol and forget the other.
        self.workspaces_changed();
        if self.desks_protocol.is_empty() {
            return;
        }
        let display = self.display_handle.clone();
        let desks = self.snapshot_desks();
        let windows = self.snapshot_windows();
        self.desks_protocol.reconcile(&display, &desks, &windows);
    }

    /// Stream live geometry/state to `wlrix-desks` clients. Called once per event-loop
    /// dispatch; cheap when nothing moved (a diff against the last-sent values).
    pub fn emit_desk_updates(&mut self) {
        if self.desks_protocol.is_empty() {
            return;
        }
        let focused = self.focused_window();
        // Collected before the call: `emit_updates` borrows `desks_protocol` mutably, so it
        // cannot also hold a borrow of the rest of `self` to look these up itself.
        let geometries = self.reported_geometries();
        self.desks_protocol
            .emit_updates(&geometries, focused.as_ref());
    }

    /// Where each window is represented on screen: its rectangle on the desk, or -- while
    /// minimized -- the tile it occupies in the icon grid. Reporting the icon lets a client draw
    /// the minimized window where the user actually sees it, and act on it there.
    ///
    /// A window with neither (on an inactive desk, or minimized with its icon on another desk)
    /// is absent, and keeps whatever geometry was last sent for it.
    fn reported_geometries(&self) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let mut geometries: Vec<(Window, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|window| {
                self.space
                    .element_geometry(window)
                    .map(|geometry| (window.clone(), geometry))
            })
            .collect();
        if let Some(grid) = self.icon_grid() {
            geometries.extend(
                self.minimized_icons()
                    .into_iter()
                    .map(|(window, slot)| (window, grid.slot_rect(slot))),
            );
        }
        geometries
    }

    fn snapshot_desks(&self) -> Vec<DeskSnapshot> {
        let active = self.desks.active();
        let snapshot = |id: DeskId| DeskSnapshot {
            id,
            name: self.desks.name(id).unwrap_or_default().to_string(),
            global: id.is_global(),
            active: id == active,
        };
        std::iter::once(snapshot(DeskId::GLOBAL))
            .chain(self.desks.order().iter().map(|&id| snapshot(id)))
            .collect()
    }

    fn snapshot_windows(&self) -> Vec<WindowSnapshot> {
        let focused = self.focused_window();
        let geometries = self.reported_geometries();
        self.space
            .elements()
            .cloned()
            .chain(self.desks.hidden().iter().cloned())
            .map(|window| {
                let geometry = geometries
                    .iter()
                    .find(|(candidate, _)| candidate == &window)
                    .map(|(_, geometry)| *geometry);
                self.window_snapshot(window, focused.as_ref(), geometry)
            })
            .collect()
    }

    /// `geometry` is where the window is shown (from [`Self::reported_geometries`]); without one
    /// it falls back to where it would reappear.
    fn window_snapshot(
        &self,
        window: Window,
        focused: Option<&Window>,
        geometry: Option<Rectangle<i32, Logical>>,
    ) -> WindowSnapshot {
        // Read the state out before moving `window` into the snapshot: the `Ref` guard would
        // otherwise still borrow it.
        let (minimized, maximized, desk, last_pos) = {
            let state = desks::window_state(&window).borrow();
            (state.minimized, state.maximized, state.desk, state.last_pos)
        };
        let geometry = geometry.unwrap_or_else(|| Rectangle::new(last_pos, window.geometry().size));
        WindowSnapshot {
            app_id: window_app_id(&window),
            title: window_title(&window),
            geometry,
            minimized,
            maximized,
            activated: focused == Some(&window),
            desk,
            window,
        }
    }
}

/// A window's application id: the xdg app_id, or the X11 class.
fn window_app_id(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.class();
    }
    crate::placement::app_id(window).unwrap_or_default()
}

/// A window's title.
fn window_title(window: &Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.title();
    }
    let Some(toplevel) = window.toplevel() else {
        return String::new();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.title.clone()))
            .unwrap_or_default()
    })
}

//
// Manager
//

impl GlobalDispatch<WlrixDesksManagerV1, ()> for Wlrix {
    fn bind(
        state: &mut Self,
        display: &DisplayHandle,
        client: &Client,
        resource: New<WlrixDesksManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        // Snapshot first: `advertise` needs the protocol state mutably.
        let desks = state.snapshot_desks();
        let windows = state.snapshot_windows();
        state
            .desks_protocol
            .advertise(display, client, manager, &desks, &windows);
    }
}

impl Dispatch<WlrixDesksManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &WlrixDesksManagerV1,
        request: wlrix_desks_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wlrix_desks_manager_v1::Request::CreateDesk => {
                state.create_desk();
            }
            wlrix_desks_manager_v1::Request::Stop => {
                manager.finished();
                state.desks_protocol.forget_manager(manager);
            }
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, manager: &WlrixDesksManagerV1, _data: &()) {
        state.desks_protocol.forget_manager(manager);
    }
}

//
// Desk
//

impl Dispatch<WlrixDeskV1, DeskId> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlrixDeskV1,
        request: wlrix_desk_v1::Request,
        id: &DeskId,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wlrix_desk_v1::Request::Activate => state.switch_desk(*id),
            wlrix_desk_v1::Request::SetName { name } => {
                if state.desks.rename(*id, name) {
                    state.desks_dirty = true;
                    state.desks_changed();
                }
            }
            wlrix_desk_v1::Request::Remove => state.remove_desk(*id),
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &WlrixDeskV1, _id: &DeskId) {
        state.desks_protocol.forget_desk(resource);
    }
}

//
// Toplevel
//

impl Dispatch<WlrixToplevelV1, Window> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlrixToplevelV1,
        request: wlrix_toplevel_v1::Request,
        window: &Window,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let window = window.clone();
        match request {
            wlrix_toplevel_v1::Request::Minimize => state.minimize_window(&window),
            wlrix_toplevel_v1::Request::Restore => state.restore_window(&window),
            wlrix_toplevel_v1::Request::Maximize => state.maximize_window(&window),
            wlrix_toplevel_v1::Request::Unmaximize => state.unmaximize_window(&window),
            wlrix_toplevel_v1::Request::Raise => state.raise_window(&window),
            wlrix_toplevel_v1::Request::Lower => state.lower_window(&window),
            wlrix_toplevel_v1::Request::MoveToDesk { desk } => {
                if let Some(id) = desk.data::<DeskId>().copied() {
                    state.move_window_to_desk(&window, id);
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &WlrixToplevelV1,
        _window: &Window,
    ) {
        state.desks_protocol.forget_toplevel(resource);
    }
}
