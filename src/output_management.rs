// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-output-management-v1`: lets clients enumerate and configure monitors.
//!
//! This is what `wlr-randr`, `kanshi` and friends speak, and what a wlRIX display
//! settings panel would eventually drive. Smithay has no implementation of this
//! protocol, so the server side is written out here.
//!
//! Currently the read side is complete: every bound manager is sent each output as a
//! head, with its modes, current mode, position, transform and scale, followed by
//! `done` carrying a serial. Whenever the outputs change the heads are torn down and
//! re-advertised with a fresh serial, which is also what invalidates any configuration
//! a client was still holding.
//!
//! Applying handles position, transform, scale, mode and enabling/disabling a head. The first three are output
//! state; a mode change has to reprogram the DRM output, so it is queued for the
//! backend to carry out when it next wakes, as is switching a head on or off.
//! Refusal applies to the whole configuration rather than letting the supported parts
//! through, since these are meant to be atomic.

use std::sync::Mutex;

use smithay::{
    output::{Mode, Output, Scale},
    reexports::{
        wayland_protocols_wlr::output_management::v1::server::{
            zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
            zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
            zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
            zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
            zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId,
        },
    },
    utils::{Logical, Point, Transform},
};
use tracing::warn;

use crate::Wlrix;

/// Protocol version we implement. v2 adds `adaptive_sync`, which we do not drive yet.
/// Version 4, for `set_adaptive_sync` (VRR). Everything a version adds is guarded at
/// the point it is sent, since a client binds at *its* version, not the one advertised,
/// and sending an event a client's version lacks kills its connection.
const VERSION: u32 = 4;

/// Server state for the output-management global.
pub struct OutputManagementState {
    /// Bumped on every change; clients must quote it when configuring, so a stale
    /// configuration can be rejected rather than applied to a layout that moved on.
    serial: u32,
    instances: Vec<ManagerInstance>,
}

/// One bound `zwlr_output_manager_v1`, with the heads advertised to it.
struct ManagerInstance {
    manager: ZwlrOutputManagerV1,
    heads: Vec<HeadInstance>,
}

struct HeadInstance {
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
}

impl OutputManagementState {
    pub fn new() -> Self {
        Self {
            serial: 0,
            instances: Vec::new(),
        }
    }

    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ZwlrOutputManagerV1, _>(VERSION, ())
    }

    /// Advertise the current layout to a manager that has just bound.
    fn advertise(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: ZwlrOutputManagerV1,
        enabled: &[Output],
        disabled: &[Output],
        vrr: &crate::vrr::VrrState,
    ) {
        let mut instance = ManagerInstance {
            manager,
            heads: Vec::new(),
        };
        for (output, on) in heads_of(enabled, disabled) {
            if let Some(head) = advertise_head(
                display,
                client,
                &instance.manager,
                output,
                on,
                vrr.enabled(output),
            ) {
                instance.heads.push(head);
            }
        }
        instance.manager.done(self.serial);
        self.instances.push(instance);
    }

    /// Re-advertise everything after the layout changed (hotplug, mode or position
    /// change). Heads are destroyed and rebuilt, and the new serial invalidates any
    /// configuration a client had in flight.
    pub fn outputs_changed(
        &mut self,
        display: &DisplayHandle,
        enabled: &[Output],
        disabled: &[Output],
        vrr: &crate::vrr::VrrState,
    ) {
        self.serial = self.serial.wrapping_add(1);

        // Drop dead managers rather than writing to them.
        self.instances
            .retain(|instance| instance.manager.is_alive());

        for instance in &mut self.instances {
            for head in instance.heads.drain(..) {
                for mode in head.modes {
                    mode.finished();
                }
                head.head.finished();
            }

            let Some(client) = instance.manager.client() else {
                continue;
            };
            for (output, on) in heads_of(enabled, disabled) {
                if let Some(head) = advertise_head(
                    display,
                    &client,
                    &instance.manager,
                    output,
                    on,
                    vrr.enabled(output),
                ) {
                    instance.heads.push(head);
                }
            }
            instance.manager.done(self.serial);
        }
    }

    /// Whether a configuration quoting `serial` is still valid.
    fn serial_is_current(&self, serial: u32) -> bool {
        self.serial == serial
    }

    fn forget_manager(&mut self, manager: &ZwlrOutputManagerV1) {
        self.instances
            .retain(|instance| &instance.manager != manager);
    }
}

/// Every head to advertise, paired with whether it is switched on.
fn heads_of<'a>(
    enabled: &'a [Output],
    disabled: &'a [Output],
) -> impl Iterator<Item = (&'a Output, bool)> {
    enabled
        .iter()
        .map(|output| (output, true))
        .chain(disabled.iter().map(|output| (output, false)))
}

/// Create a head for `output` and describe it to the client.
/// How a head reports its adaptive sync state.
fn adaptive_sync_state(enabled: bool) -> zwlr_output_head_v1::AdaptiveSyncState {
    if enabled {
        zwlr_output_head_v1::AdaptiveSyncState::Enabled
    } else {
        zwlr_output_head_v1::AdaptiveSyncState::Disabled
    }
}

fn advertise_head(
    display: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    output: &Output,
    enabled: bool,
    vrr: bool,
) -> Option<HeadInstance> {
    let head = client
        .create_resource::<ZwlrOutputHeadV1, _, Wlrix>(display, manager.version(), output.clone())
        .ok()?;
    manager.head(&head);

    head.name(output.name());
    head.description(output.description());
    let physical = output.physical_properties();
    head.physical_size(physical.size.w, physical.size.h);
    if head.version() >= 2 {
        head.make(physical.make);
        head.model(physical.model);
        // No `serial_number`: it comes from EDID, and `display-info` is disabled.
    }

    let current = output.current_mode();
    let preferred = output.preferred_mode();
    let mut modes = Vec::new();
    for mode in output.modes() {
        let Ok(mode_resource) =
            client.create_resource::<ZwlrOutputModeV1, _, Wlrix>(display, head.version(), mode)
        else {
            continue;
        };
        head.mode(&mode_resource);
        mode_resource.size(mode.size.w, mode.size.h);
        mode_resource.refresh(mode.refresh);
        if preferred == Some(mode) {
            mode_resource.preferred();
        }
        if current == Some(mode) {
            head.current_mode(&mode_resource);
        }
        modes.push(mode_resource);
    }

    head.enabled(enabled as i32);
    let location = output.current_location();
    head.position(location.x, location.y);
    head.transform(output.current_transform().into());
    head.scale(output.current_scale().fractional_scale());
    if head.version() >= 4 {
        head.adaptive_sync(adaptive_sync_state(vrr));
    }

    Some(HeadInstance { head, modes })
}

//
// Manager
//

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for Wlrix {
    fn bind(
        state: &mut Self,
        display: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        // Collect first: the advertise call needs the state mutably.
        let enabled: Vec<Output> = state.space.outputs().cloned().collect();
        let disabled = state.disabled_outputs.clone();
        state
            .output_management
            .advertise(display, client, manager, &enabled, &disabled, &state.vrr);
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(id, PendingConfiguration::new(serial));
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state.output_management.forget_manager(manager);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        manager: &ZwlrOutputManagerV1,
        _data: &(),
    ) {
        state.output_management.forget_manager(manager);
    }
}

//
// Head and mode: read-only, so only `release` is meaningful.
//

impl Dispatch<ZwlrOutputHeadV1, Output> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &Output,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputModeV1, Mode> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &Mode,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

//
// Configuration
//

/// What a client asked us to change for one head.
#[derive(Default, Clone, Copy)]
struct HeadConfig {
    enabled: Option<bool>,
    adaptive_sync: Option<bool>,
    position: Option<Point<i32, Logical>>,
    transform: Option<Transform>,
    scale: Option<f64>,
    mode: Option<Mode>,
}

/// A configuration a client is building up, applied atomically on `apply`.
pub struct PendingConfiguration {
    /// The layout serial this was built against.
    serial: u32,
    inner: Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    heads: Vec<(Output, HeadConfig)>,
    /// Set when the client asked for something we cannot do yet (a mode change or
    /// disabling an output). The whole configuration is then refused, rather than
    /// applied in part -- these are meant to be atomic.
    unsupported: bool,
}

impl PendingConfiguration {
    fn new(serial: u32) -> Self {
        Self {
            serial,
            inner: Mutex::new(PendingInner::default()),
        }
    }

    /// Record a change for `output`, creating its entry if needed.
    fn update(&self, output: &Output, edit: impl FnOnce(&mut HeadConfig)) {
        let mut inner = self.inner.lock().unwrap();
        if let Some((_, config)) = inner.heads.iter_mut().find(|(known, _)| known == output) {
            edit(config);
            return;
        }
        let mut config = HeadConfig::default();
        edit(&mut config);
        inner.heads.push((output.clone(), config));
    }

    fn mark_unsupported(&self) {
        self.inner.lock().unwrap().unsupported = true;
    }
}

/// Links a configuration head back to its output and parent configuration.
pub struct ConfigurationHeadData {
    output: Output,
    configuration: ZwlrOutputConfigurationV1,
}

impl Dispatch<ZwlrOutputConfigurationV1, PendingConfiguration> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        configuration: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &PendingConfiguration,
        display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(output) = head.data::<Output>().cloned() else {
                    return;
                };
                data.update(&output, |config| config.enabled = Some(true));
                data_init.init(
                    id,
                    ConfigurationHeadData {
                        output,
                        configuration: configuration.clone(),
                    },
                );
            }

            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let Some(output) = head.data::<Output>().cloned() else {
                    return;
                };
                data.update(&output, |config| config.enabled = Some(false));
            }

            zwlr_output_configuration_v1::Request::Test => {
                if !state.output_management.serial_is_current(data.serial) {
                    configuration.cancelled();
                    return;
                }
                if data.inner.lock().unwrap().unsupported {
                    configuration.failed();
                } else {
                    configuration.succeeded();
                }
            }

            zwlr_output_configuration_v1::Request::Apply => {
                if !state.output_management.serial_is_current(data.serial) {
                    // Built against a layout that has since changed.
                    configuration.cancelled();
                    return;
                }

                let inner = data.inner.lock().unwrap();
                if inner.unsupported {
                    warn!(
                        "refusing output configuration: mode changes and disabling are not implemented"
                    );
                    configuration.failed();
                    return;
                }

                // Reject a mode the output does not advertise before reporting
                // success, since the backend applies it asynchronously and could not
                // tell the client afterwards.
                if let Some((output, mode)) = inner.heads.iter().find_map(|(output, config)| {
                    config
                        .mode
                        .filter(|mode| !output.modes().contains(mode))
                        .map(|mode| (output, mode))
                }) {
                    warn!(output = output.name(), ?mode, "refusing unknown mode");
                    configuration.failed();
                    return;
                }

                // Refuse to switch off every display: that would leave nothing to
                // undo it with.
                let enabled_now = state.space.outputs().count();
                let turning_off = inner
                    .heads
                    .iter()
                    .filter(|(output, config)| {
                        config.enabled == Some(false)
                            && state.space.outputs().any(|known| known == output)
                    })
                    .count();
                let turning_on = inner
                    .heads
                    .iter()
                    .filter(|(_, config)| config.enabled == Some(true))
                    .count();
                if turning_off >= enabled_now && turning_on == 0 {
                    warn!("refusing to disable every output");
                    configuration.failed();
                    return;
                }

                for (output, config) in &inner.heads {
                    apply_head(state, output, config);
                }
                drop(inner);

                configuration.succeeded();

                // Moving an output can strand windows, and every client needs the new
                // layout.
                let pointer = state.pointer_location();
                crate::placement::relocate_orphaned_windows(&mut state.space, pointer);
                state.advertise_outputs(display);
                state.request_redraw();
            }

            _ => {}
        }
    }
}

/// Apply one head's changes. Position, transform and scale are all just output state.
fn apply_head(state: &mut Wlrix, output: &Output, config: &HeadConfig) {
    let scale = config.scale.map(Scale::Fractional);
    output.change_current_state(None, config.transform, scale, config.position);
    // Position, transform and scale take effect right here; the mode/enable/vrr changes
    // queued below land in the backend's next pass. Either way the layout changed and
    // wants saving -- the backend flushes this once the batch settles.
    state.outputs_dirty = true;

    // Both of these mean reprogramming DRM, which only the backend can do.
    if let Some(enabled) = config.enabled {
        let currently_on = state.space.outputs().any(|known| known == output);
        if enabled != currently_on {
            state.pending_output_toggles.push((output.clone(), enabled));
        }
    }
    if let Some(mode) = config.mode {
        state.pending_mode_changes.push((output.clone(), mode));
    }
    // Also the backend's job: VRR is a DRM property on the crtc.
    if let Some(adaptive_sync) = config.adaptive_sync {
        state
            .pending_vrr_changes
            .push((output.clone(), adaptive_sync));
    }

    if let Some(position) = config.position {
        // Keep the space's view of where this output sits in step.
        state.space.map_output(output, position);
    }

    tracing::info!(
        output = output.name(),
        ?config.position,
        ?config.transform,
        ?config.scale,
        ?config.mode,
        ?config.enabled,
        ?config.adaptive_sync,
        "applied output configuration"
    );
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigurationHeadData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(pending) = data.configuration.data::<PendingConfiguration>() else {
            return;
        };

        match request {
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                pending.update(&data.output, |config| {
                    config.position = Some(Point::from((x, y)));
                });
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                match transform.into_result() {
                    Ok(transform) => pending.update(&data.output, |config| {
                        config.transform = Some(transform.into());
                    }),
                    Err(_) => pending.mark_unsupported(),
                }
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                pending.update(&data.output, |config| {
                    config.scale = Some(scale);
                });
            }
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                match mode.data::<Mode>().copied() {
                    Some(mode) => pending.update(&data.output, |config| {
                        config.mode = Some(mode);
                    }),
                    None => pending.mark_unsupported(),
                }
            }
            // Only modes the connector actually advertises are supported.
            zwlr_output_configuration_head_v1::Request::SetCustomMode { .. } => {
                pending.mark_unsupported();
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state: requested } => {
                match requested.into_result() {
                    // Refused as a whole rather than silently ignored: a client asking
                    // for VRR on a screen that cannot do it should be told so, and
                    // these configurations are meant to apply atomically.
                    Ok(_) if !state.vrr.supported(&data.output) => pending.mark_unsupported(),
                    Ok(requested) => pending.update(&data.output, |config| {
                        config.adaptive_sync =
                            Some(requested == zwlr_output_head_v1::AdaptiveSyncState::Enabled);
                    }),
                    Err(_) => pending.mark_unsupported(),
                }
            }
            _ => {}
        }
    }
}
