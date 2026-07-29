// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-workspace-v1`: the standard way a pager or bar sees and switches desks.
//!
//! The bespoke [`crate::desks_protocol`] carries what the Desks Overview needs (live window
//! geometry, per-window operations); this is the standard-protocol front end onto the same
//! model in [`crate::desks`], so third-party pagers work too. Hand-written like
//! [`crate::output_management`], since Smithay has no implementation.
//!
//! **Shape of the mapping.** wlRIX desks are global across outputs -- switching changes every
//! monitor at once -- so there is exactly one workspace *group*, which every output enters.
//! Each ordinary desk is a workspace in it, numbered by its position.
//!
//! The **Global desk is deliberately not exposed**. It is the sticky desk whose windows appear
//! everywhere, not somewhere the user switches to; a pager offering to activate it would be
//! offering something meaningless.
//!
//! Requests are **staged and applied on `commit`**, as the protocol requires: a pager that
//! deactivates one workspace and activates another must be seen to do it atomically.

use smithay::reexports::{
    wayland_protocols::ext::workspace::v1::server::{
        ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
        ext_workspace_handle_v1::{
            self, ExtWorkspaceHandleV1, State as WorkspaceState, WorkspaceCapabilities,
        },
        ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
    },
    wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        backend::{ClientId, GlobalId},
    },
};

use crate::{Wlrix, desks::DeskId};

const VERSION: u32 = 1;

/// A desk, snapshotted so the emit does not borrow the model.
pub struct WorkspaceSnapshot {
    pub id: DeskId,
    pub name: String,
    pub active: bool,
    /// Position in the desk list, sent as the workspace's 1D coordinate.
    pub index: u32,
    pub removable: bool,
}

/// What a client asked for, waiting for its `commit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staged {
    Activate(DeskId),
    Remove(DeskId),
    Create,
}

pub struct WorkspaceProtocolState {
    instances: Vec<ManagerInstance>,
}

struct ManagerInstance {
    manager: ExtWorkspaceManagerV1,
    group: Option<ExtWorkspaceGroupHandleV1>,
    workspaces: Vec<WorkspaceResource>,
    /// Requests received since the last `commit`, applied in order when it arrives.
    staged: Vec<Staged>,
}

struct WorkspaceResource {
    id: DeskId,
    resource: ExtWorkspaceHandleV1,
    last_name: String,
    last_active: bool,
}

impl WorkspaceProtocolState {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }

    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ExtWorkspaceManagerV1, _>(VERSION, ())
    }

    fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Advertise the group and every desk to a manager that has just bound.
    fn advertise(
        &mut self,
        client: &Client,
        display: &DisplayHandle,
        manager: ExtWorkspaceManagerV1,
        workspaces: &[WorkspaceSnapshot],
        outputs: &[smithay::output::Output],
    ) {
        let mut instance = ManagerInstance {
            manager,
            group: None,
            workspaces: Vec::new(),
            staged: Vec::new(),
        };
        add_group(client, display, &mut instance, outputs);
        for snapshot in workspaces {
            add_workspace(client, display, &mut instance, snapshot);
        }
        instance.manager.done();
        self.instances.push(instance);
    }

    /// Reconcile each client's workspaces with the desks that exist, and push any changes.
    fn refresh(&mut self, client_display: &DisplayHandle, workspaces: &[WorkspaceSnapshot]) {
        self.instances
            .retain(|instance| instance.manager.is_alive());

        for instance in &mut self.instances {
            let Some(client) = instance.manager.client() else {
                continue;
            };
            let mut changed = false;

            // Desks that are gone.
            let group = instance.group.clone();
            instance.workspaces.retain(|workspace| {
                let kept = workspaces.iter().any(|snap| snap.id == workspace.id);
                if !kept {
                    if let Some(group) = &group {
                        group.workspace_leave(&workspace.resource);
                    }
                    workspace.resource.removed();
                }
                kept
            });
            if instance.workspaces.len() != workspaces.len() {
                changed = true;
            }

            for snapshot in workspaces {
                match instance
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == snapshot.id)
                {
                    Some(workspace) => {
                        if workspace.last_name != snapshot.name {
                            workspace.resource.name(snapshot.name.clone());
                            workspace.last_name = snapshot.name.clone();
                            changed = true;
                        }
                        if workspace.last_active != snapshot.active {
                            workspace.resource.state(workspace_state(snapshot.active));
                            workspace.last_active = snapshot.active;
                            changed = true;
                        }
                    }
                    None => {
                        add_workspace(&client, client_display, instance, snapshot);
                        changed = true;
                    }
                }
            }

            if changed {
                instance.manager.done();
            }
        }
    }

    /// Take everything a manager staged, to apply on its `commit`.
    fn take_staged(&mut self, manager: &ExtWorkspaceManagerV1) -> Vec<Staged> {
        self.instances
            .iter_mut()
            .find(|instance| &instance.manager == manager)
            .map(|instance| std::mem::take(&mut instance.staged))
            .unwrap_or_default()
    }

    fn stage(&mut self, resource_client: Option<Client>, action: Staged) {
        // Staged against whichever manager belongs to the requesting client.
        for instance in &mut self.instances {
            if instance.manager.client() == resource_client {
                instance.staged.push(action);
            }
        }
    }

    fn forget_manager(&mut self, manager: &ExtWorkspaceManagerV1) {
        self.instances
            .retain(|instance| &instance.manager != manager);
    }
}

impl Default for WorkspaceProtocolState {
    fn default() -> Self {
        Self::new()
    }
}

/// One group, entered by every output: desks are global, not per-monitor.
fn add_group(
    client: &Client,
    display: &DisplayHandle,
    instance: &mut ManagerInstance,
    outputs: &[smithay::output::Output],
) {
    let Ok(group) =
        client.create_resource::<ExtWorkspaceGroupHandleV1, _, Wlrix>(display, VERSION, ())
    else {
        return;
    };
    instance.manager.workspace_group(&group);
    group.capabilities(GroupCapabilities::CreateWorkspace);
    for output in outputs {
        for wl_output in output.client_outputs(client) {
            group.output_enter(&wl_output);
        }
    }
    instance.group = Some(group);
}

fn add_workspace(
    client: &Client,
    display: &DisplayHandle,
    instance: &mut ManagerInstance,
    snapshot: &WorkspaceSnapshot,
) {
    let Ok(resource) =
        client.create_resource::<ExtWorkspaceHandleV1, _, Wlrix>(display, VERSION, snapshot.id)
    else {
        return;
    };
    instance.manager.workspace(&resource);
    if let Some(group) = &instance.group {
        group.workspace_enter(&resource);
    }

    resource.id(snapshot.id.0.to_string());
    resource.name(snapshot.name.clone());
    // Desks are a flat list, so a 1D coordinate: the position, with no geometry implied.
    resource.coordinates(snapshot.index.to_ne_bytes().to_vec());
    resource.state(workspace_state(snapshot.active));
    // `deactivate` is not offered: one desk is always active, so there is nothing it could
    // mean. `assign` is not offered either -- there is only one group to assign to.
    let mut capabilities = WorkspaceCapabilities::Activate;
    if snapshot.removable {
        capabilities |= WorkspaceCapabilities::Remove;
    }
    resource.capabilities(capabilities);

    instance.workspaces.push(WorkspaceResource {
        id: snapshot.id,
        resource,
        last_name: snapshot.name.clone(),
        last_active: snapshot.active,
    });
}

fn workspace_state(active: bool) -> WorkspaceState {
    if active {
        WorkspaceState::Active
    } else {
        WorkspaceState::empty()
    }
}

impl Wlrix {
    /// The desks a pager should see: the ordinary ones, in order. The Global desk is left out
    /// (see the module note).
    fn workspace_snapshots(&self) -> Vec<WorkspaceSnapshot> {
        let active = self.desks.active();
        self.desks
            .order()
            .iter()
            .enumerate()
            .map(|(index, &id)| WorkspaceSnapshot {
                id,
                name: self.desks.name(id).unwrap_or_default().to_string(),
                active: id == active,
                index: index as u32,
                removable: self.desks.deletable(id),
            })
            .collect()
    }

    /// Push desk changes to pagers. Called wherever the desk model changes.
    pub fn workspaces_changed(&mut self) {
        if self.workspace_protocol.is_empty() {
            return;
        }
        let workspaces = self.workspace_snapshots();
        let display = self.display_handle.clone();
        self.workspace_protocol.refresh(&display, &workspaces);
    }

    /// Apply what a client staged, in the order it asked.
    fn apply_staged_workspaces(&mut self, manager: &ExtWorkspaceManagerV1) {
        for action in self.workspace_protocol.take_staged(manager) {
            match action {
                Staged::Activate(id) => self.switch_desk(id),
                Staged::Remove(id) => self.remove_desk(id),
                Staged::Create => {
                    self.create_desk();
                }
            }
        }
    }
}

//
// Manager
//

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for Wlrix {
    fn bind(
        state: &mut Self,
        display: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let workspaces = state.workspace_snapshots();
        let outputs: Vec<_> = state.space.outputs().cloned().collect();
        state
            .workspace_protocol
            .advertise(client, display, manager, &workspaces, &outputs);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Everything staged since the last commit is applied together.
            ext_workspace_manager_v1::Request::Commit => {
                state.apply_staged_workspaces(resource);
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                state.workspace_protocol.forget_manager(resource);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        state.workspace_protocol.forget_manager(resource);
    }
}

//
// Group
//

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // The name a client suggests is ignored: desks are named by the compositor, and
        // `create_desk` picks the next "Desk N".
        if let ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } = request {
            state
                .workspace_protocol
                .stage(Some(client.clone()), Staged::Create);
        }
    }
}

//
// Workspace
//

impl Dispatch<ExtWorkspaceHandleV1, DeskId> for Wlrix {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        id: &DeskId,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let client = Some(client.clone());
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                state
                    .workspace_protocol
                    .stage(client, Staged::Activate(*id));
            }
            ext_workspace_handle_v1::Request::Remove => {
                state.workspace_protocol.stage(client, Staged::Remove(*id));
            }
            // Neither capability is advertised, so a well-behaved client will not ask: one desk
            // is always active, and there is only one group.
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Assign { .. } => {}
            _ => {}
        }
    }
}
