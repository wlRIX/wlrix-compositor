// SPDX-License-Identifier: GPL-3.0-or-later
//! udev / DRM-KMS backend — hardware backend.
//!
//! Takes over the GPU directly (no host compositor): a `libseat` session for
//! privileged device access + VT switching, `libinput` for input, and per-connector
//! `DrmOutput`s driven by DRM vblank for multi-monitor output. Rendering goes through
//! `DrmOutputManager`, which assigns buffers to DRM planes for direct scanout.
//!
//! Must be run from a free VT/TTY (it needs DRM master).
//!
//! Clients hand us GPU buffers via `linux-dmabuf-v1`, and each output advertises a
//! `Scanout` feedback tranche built from its DRM planes' formats, so a client buffer
//! can be handed straight to a plane without a copy.
//!
//! Single-GPU for now (one `GlesRenderer` per DRM device). Deferred: cursor planes,
//! damage-driven scheduling, and multi-GPU. See Smithay's `anvil` example for those.

use std::{cell::RefCell, collections::HashMap, path::Path, rc::Rc, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, DrmSurface,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportEgl,
            element::{
                RenderElementStates, default_primary_scanout_output_compare,
                utils::select_dmabuf_feedback,
            },
            gles::GlesRenderer,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        layer_map_for_output,
        utils::{surface_primary_scanout_output, update_surface_primary_scanout_output},
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{EventLoop, LoopHandle, RegistrationToken},
        drm::{
            self,
            control::{ModeTypeFlags, connector, crtc},
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufState},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{error, info, warn};

use crate::{CalloopData, Wlrix};

/// Formats the primary framebuffer may use, preferring 10-bit then 8-bit.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// wlRIX desktop clear color (Indigo Magic-ish blue-gray). Placeholder.
use crate::render::DESKTOP_BACKGROUND as CLEAR_COLOR;

/// What a DRM output composites: desktop plus cursor.
type RenderElem = crate::render::OutputElem<GlesRenderer>;

/// Identifies which output a `wl_output` corresponds to (device + crtc).
#[derive(Debug, PartialEq, Eq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

/// All udev-backend state. Stored in [`CalloopData`] so event sources can reach it.
pub struct UdevState {
    session: LibSeatSession,
    loop_handle: LoopHandle<'static, CalloopData>,
    backends: HashMap<DrmNode, DeviceData>,
}

/// Per-DRM-device state.
struct DeviceData {
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    /// Shared with [`crate::state::Wlrix`] so the dmabuf handler can test-import.
    renderer: Rc<RefCell<GlesRenderer>>,
    /// Render node for this device; used as the dmabuf feedback main device.
    render_node: Option<DrmNode>,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// Connectors switched off by a client. Still physically connected, so the scanner
    /// will not announce them again; kept here so they can be turned back on.
    disabled: HashMap<crtc::Handle, connector::Info>,
    registration_token: RegistrationToken,
}

/// Per-surface dmabuf feedback: which formats a client should allocate for, depending
/// on whether its buffer can be scanned out directly or has to be composited.
struct SurfaceDmabufFeedback {
    render_feedback: DmabufFeedback,
    scanout_feedback: DmabufFeedback,
}

/// Pick the mode to light a connector up with: its preferred resolution, at the
/// highest refresh rate that resolution offers.
///
/// The mode DRM flags as preferred is often only 60Hz even on a high-refresh panel,
/// so taking it verbatim leaves the display running far below what it can do.
fn preferred_mode(connector: &connector::Info) -> Option<drm::control::Mode> {
    let modes = connector.modes();
    let preferred = modes
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())?;

    let resolution = preferred.size();
    modes
        .iter()
        .filter(|mode| mode.size() == resolution)
        .max_by_key(|mode| mode.vrefresh())
        .copied()
}

/// Build dmabuf feedback for one DRM surface.
///
/// The scanout tranche advertises the formats this crtc's planes can scan out, flagged
/// `Scanout`, so clients allocate buffers we can hand straight to a plane — this is what
/// makes direct scanout (zero-copy) reliable rather than accidental. It is intersected
/// with the renderer's formats so there is always a composited fallback.
fn surface_dmabuf_feedback(
    render_node: DrmNode,
    render_formats: FormatSet,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let planes = surface.planes().clone();
    let plane_formats = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|plane| plane.formats))
        .collect::<FormatSet>()
        .intersection(&render_formats)
        .copied()
        .collect::<FormatSet>();

    let builder = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats.clone());
    let render_feedback = builder.clone().build().ok()?;
    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?,
            Some(TrancheFlags::Scanout),
            plane_formats,
        )
        .add_preference_tranche(render_node.dev_id(), None, render_formats)
        .build()
        .ok()?;

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

/// Where an output is in the render cycle.
///
/// Rendering is damage-driven: an idle output draws nothing at all until a damage
/// source calls [`crate::state::Wlrix::request_redraw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedrawState {
    /// Nothing to draw; waiting for damage.
    Idle,
    /// A render is already scheduled on the event loop.
    Queued,
    /// A frame was submitted; waiting for its vblank. `dirty` records damage that
    /// arrived while waiting, so we redraw again once the frame lands.
    WaitingForVBlank { dirty: bool },
}

/// Per-connector (crtc) output state.
struct SurfaceData {
    global: Option<GlobalId>,
    drm_output:
        DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
    /// Scanout/render feedback advertised to clients on this output.
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    /// The connector, kept so its modes can be mapped back to DRM and so the output
    /// can be rebuilt after being switched off.
    connector: connector::Info,
    redraw_state: RedrawState,
}

/// Bring up the udev/DRM backend and return `true` (it drives the event loop).
pub fn init_udev(
    event_loop: &mut EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Session for privileged device access + VT switching.
    let (session, notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    info!(seat = %seat_name, "libseat session acquired");

    // Primary GPU (fall back to the first GPU on the seat).
    let primary_gpu = match primary_gpu(&seat_name)? {
        Some(path) => DrmNode::from_path(path)?,
        None => {
            let path = all_gpus(&seat_name)?
                .into_iter()
                .next()
                .ok_or("no GPU found on this seat")?;
            DrmNode::from_path(path)?
        }
    };
    info!(%primary_gpu, "primary GPU");

    let loop_handle = event_loop.handle();
    data.udev = Some(UdevState {
        session: session.clone(),
        loop_handle: loop_handle.clone(),
        backends: HashMap::new(),
    });
    // Damage-driven rendering: anything that changes the screen pings us, and we
    // queue redraws for the outputs. Without this an idle desktop would either
    // busy-render or never update.
    let (redraw_ping, redraw_source) = smithay::reexports::calloop::ping::make_ping()?;
    loop_handle.insert_source(redraw_source, |_, _, data| queue_redraw_all(data))?;
    data.state.redraw_ping = Some(redraw_ping);

    // Give the input handler a session handle for VT switching.
    data.state.session = Some(session.clone());
    info!("keybindings: Ctrl+Alt+F<n> switches VT, Ctrl+Alt+Backspace quits");

    // Input via libinput, tied to the session.
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.into());
    libinput_context.udev_assign_seat(&seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    loop_handle.insert_source(libinput_backend, move |event, _, data| {
        data.state.process_input_event(event);
    })?;

    // Session pause/resume (VT switch): suspend input + DRM, then reactivate.
    loop_handle.insert_source(notifier, move |event, _, data| match event {
        SessionEvent::PauseSession => {
            info!("session paused");
            libinput_context.suspend();
            if let Some(udev) = data.udev.as_mut() {
                for backend in udev.backends.values_mut() {
                    backend.drm_output_manager.pause();
                }
            }
        }
        SessionEvent::ActivateSession => {
            info!("session resumed");
            if let Err(err) = libinput_context.resume() {
                error!(?err, "failed to resume libinput");
            }
            let mut to_render = Vec::new();
            if let Some(udev) = data.udev.as_mut() {
                for (node, backend) in udev.backends.iter_mut() {
                    let _ = backend.drm_output_manager.activate(false);
                    for (crtc, surface) in backend.surfaces.iter_mut() {
                        // A frame may have been in flight when the session was paused,
                        // and its vblank will never arrive. Without resetting, the
                        // output would stay in WaitingForVBlank forever and the screen
                        // would be frozen after switching back.
                        surface.redraw_state = RedrawState::Idle;
                        to_render.push((*node, *crtc));
                    }
                }
            }
            for (node, crtc) in to_render {
                queue_redraw(data, node, crtc);
            }
        }
    })?;

    // udev device hotplug (GPU add/change/remove).
    let udev_backend = UdevBackend::new(&seat_name)?;
    let mut devices: Vec<_> = udev_backend
        .device_list()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();
    // Initialize the primary GPU first so it -- not a secondary card -- backs the
    // dmabuf global and the shared renderer.
    devices.sort_by_key(|(id, _)| DrmNode::from_dev_id(*id).ok() != Some(primary_gpu));
    loop_handle.insert_source(udev_backend, move |event, _, data| match event {
        UdevEvent::Added { device_id, path } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id)
                && let Err(err) = device_added(data, node, &path)
            {
                warn!(?err, "failed to add drm device");
            }
        }
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_changed(data, node);
            }
        }
        UdevEvent::Removed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_removed(data, node);
            }
        }
    })?;

    // Bring up the devices that already exist.
    for (device_id, path) in devices {
        if let Ok(node) = DrmNode::from_dev_id(device_id)
            && let Err(err) = device_added(data, node, &path)
        {
            warn!(?err, "failed to add drm device at startup");
        }
    }

    Ok(true)
}

fn device_added(
    data: &mut CalloopData,
    node: DrmNode,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    {
        let CalloopData {
            state,
            udev,
            display_handle,
            ..
        } = data;
        let udev = udev.as_mut().ok_or("udev state missing")?;

        // Open the DRM device through the session and wrap it.
        let fd = udev.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        // EGL + GLES renderer on this GPU.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let render_node = EGLDevice::device_for_display(&egl_display)
            .ok()
            .and_then(|device| device.try_get_render_node().ok().flatten());
        let egl_context = EGLContext::new(&egl_display)?;
        let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();

        // Binding the wl_display exposes wl_drm/EGL to clients (Mesa needs this for
        // some hardware-buffer paths alongside linux-dmabuf).
        if renderer.bind_wl_display(display_handle).is_ok() {
            info!(%node, "EGL hardware acceleration enabled");
        }
        let dmabuf_formats = renderer.dmabuf_formats();
        let renderer = Rc::new(RefCell::new(renderer));

        // Advertise linux-dmabuf-v1 once, backed by the first (preferably primary)
        // GPU, so clients can hand us GPU buffers instead of shared memory.
        if state.dmabuf_state.is_none() {
            let default_feedback = DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
                .build()
                .map_err(|err| format!("failed to build dmabuf feedback: {err}"))?;
            let mut dmabuf_state = DmabufState::new();
            let _global = dmabuf_state
                .create_global_with_default_feedback::<Wlrix>(display_handle, &default_feedback);
            state.dmabuf_state = Some(dmabuf_state);
            state.renderer = Some(renderer.clone());
            info!(%node, "linux-dmabuf-v1 global created");
        }

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), render_node);
        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            exporter,
            Some(gbm),
            SUPPORTED_FORMATS.iter().copied(),
            render_formats,
        );

        // Drive rendering from vblank events on this device.
        let registration_token =
            udev.loop_handle
                .insert_source(drm_notifier, move |event, _, data| match event {
                    DrmEvent::VBlank(crtc) => frame_finish(data, node, crtc),
                    DrmEvent::Error(err) => error!(?err, "drm error"),
                })?;

        udev.backends.insert(
            node,
            DeviceData {
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                renderer,
                render_node,
                surfaces: HashMap::new(),
                disabled: HashMap::new(),
                registration_token,
            },
        );
    }

    device_changed(data, node);
    Ok(())
}

fn device_changed(data: &mut CalloopData, node: DrmNode) {
    // Scan connectors, collecting events so we can drop the borrow before acting.
    let events: Vec<DrmScanEvent> = {
        let Some(udev) = data.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(events) => events.into_iter().collect(),
            Err(err) => {
                warn!(?err, "failed to scan connectors");
                return;
            }
        }
    };

    for event in events {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(data, node, connector, crtc),
            DrmScanEvent::Disconnected {
                connector: _,
                crtc: Some(crtc),
            } => connector_disconnected(data, node, crtc),
            _ => {}
        }
    }
}

fn device_removed(data: &mut CalloopData, node: DrmNode) {
    let crtcs: Vec<crtc::Handle> = data
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&node))
        .map(|device| device.surfaces.keys().copied().collect())
        .unwrap_or_default();

    for crtc in crtcs {
        connector_disconnected(data, node, crtc);
    }

    if let Some(udev) = data.udev.as_mut()
        && let Some(device) = udev.backends.remove(&node)
    {
        udev.loop_handle.remove(device.registration_token);
        info!(%node, "drm device removed");
    }
}

fn connector_connected(
    data: &mut CalloopData,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let CalloopData {
        state,
        udev,
        display_handle,
        ..
    } = data;
    let Some(udev) = udev.as_mut() else {
        return;
    };
    let loop_handle = udev.loop_handle.clone();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let output_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
    info!(output = %output_name, ?crtc, "connector connected");

    let Some(drm_mode) = preferred_mode(&connector) else {
        warn!(output = %output_name, "connector reports no modes");
        return;
    };
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make: "wlRIX".into(),
            model: "DRM".into(),
        },
    );
    let global = output.create_global::<Wlrix>(display_handle);

    // Lay outputs left-to-right.
    let x = state.space.outputs().fold(0, |acc, o| {
        acc + state.space.output_geometry(o).unwrap().size.w
    });
    let position = (x, 0).into();
    // Register every mode the connector reports, so clients can enumerate and choose
    // between them; without this only the current mode is ever visible.
    for mode in connector.modes() {
        output.add_mode(WlMode::from(*mode));
    }
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some(position));
    info!(
        output = %output.name(),
        modes = connector.modes().len(),
        width = wl_mode.size.w,
        height = wl_mode.size.h,
        refresh_mhz = wl_mode.refresh,
        "output mode selected"
    );
    state.space.map_output(&output, position);
    output.user_data().insert_if_missing(|| UdevOutputId {
        device_id: node,
        crtc,
    });
    // Give the new output a layer map sized to it.
    layer_map_for_output(&output).arrange();

    let planes = device.drm_output_manager.device().planes(&crtc).ok();
    let drm_output = match device
        .drm_output_manager
        .initialize_output::<_, RenderElem>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            planes,
            &mut device.renderer.clone().borrow_mut(),
            &DrmOutputRenderElements::default(),
        ) {
        Ok(drm_output) => drm_output,
        Err(err) => {
            warn!(?err, "failed to initialize drm output");
            return;
        }
    };

    // Tell clients which formats this crtc's planes can scan out, so they can
    // allocate buffers we can hand straight to a plane.
    let render_formats = device.renderer.borrow().dmabuf_formats();
    let feedback_node = device.render_node.unwrap_or(node);
    let dmabuf_feedback = drm_output.with_compositor(|compositor| {
        surface_dmabuf_feedback(feedback_node, render_formats, compositor.surface())
    });
    if dmabuf_feedback.is_none() {
        warn!(
            ?crtc,
            "no dmabuf scanout feedback; direct scanout may not engage"
        );
    }

    device.surfaces.insert(
        crtc,
        SurfaceData {
            global: Some(global),
            drm_output,
            dmabuf_feedback,
            connector: connector.clone(),
            redraw_state: RedrawState::Queued,
        },
    );

    // Let wlr-output-management clients know the layout changed.
    state.advertise_outputs(display_handle);

    loop_handle.insert_idle(move |data| render_surface(data, node, crtc));
}

fn connector_disconnected(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let surface = {
        let Some(udev) = data.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        device.disabled.remove(&crtc);
        device.surfaces.remove(&crtc)
    };

    // The cable was pulled while the output was switched off: drop the head we were
    // still advertising, since it can no longer be turned back on.
    let was_disabled = data
        .state
        .disabled_outputs
        .iter()
        .any(|output| output_location(output) == Some((node, crtc)));
    if was_disabled {
        data.state
            .disabled_outputs
            .retain(|output| output_location(output) != Some((node, crtc)));
        info!(%node, ?crtc, "disabled connector unplugged");
        let display_handle = data.display_handle.clone();
        data.state.advertise_outputs(&display_handle);
    }

    let Some(mut surface) = surface else {
        return;
    };
    info!(%node, ?crtc, "connector disconnected");

    if let Some(global) = surface.global.take() {
        data.display_handle.remove_global::<Wlrix>(global);
    }
    // Remove the matching output from the space.
    let output = data.state.space.outputs().find(|o| {
        o.user_data().get::<UdevOutputId>()
            == Some(&UdevOutputId {
                device_id: node,
                crtc,
            })
    });
    if let Some(output) = output.cloned() {
        data.state.space.unmap_output(&output);

        // Windows on that monitor are now at coordinates no output covers, so bring
        // them back onto a remaining one and re-anchor the shell components.
        let pointer = data.state.pointer_location();
        crate::placement::relocate_orphaned_windows(&mut data.state.space, pointer);

        let display_handle = data.display_handle.clone();
        data.state.advertise_outputs(&display_handle);

        data.state.request_redraw();
    }
}

/// vblank: the previous frame finished scanning out. Ack it and render the next.
fn frame_finish(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    {
        let Some(udev) = data.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };
        if let Err(err) = surface.drm_output.frame_submitted() {
            warn!(?err, "frame_submitted failed");
        }

        // Only draw again if something changed while this frame was in flight.
        let dirty = matches!(
            surface.redraw_state,
            RedrawState::WaitingForVBlank { dirty: true }
        );
        surface.redraw_state = RedrawState::Idle;
        if !dirty {
            return;
        }
    }
    queue_redraw(data, node, crtc);
}

/// Queue a render of `crtc`, unless one is already pending.
fn queue_redraw(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let Some(udev) = data.udev.as_mut() else {
        return;
    };
    let loop_handle = udev.loop_handle.clone();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return;
    };

    match surface.redraw_state {
        RedrawState::Idle => {
            surface.redraw_state = RedrawState::Queued;
            loop_handle.insert_idle(move |data| render_surface(data, node, crtc));
        }
        // Already scheduled.
        RedrawState::Queued => {}
        // Mid-flight: remember to draw again once this frame lands.
        RedrawState::WaitingForVBlank { .. } => {
            surface.redraw_state = RedrawState::WaitingForVBlank { dirty: true };
        }
    }
}

/// Queue a redraw of every output. Driven by the redraw ping, which fires whenever
/// anything changed. Outputs with no actual damage render nothing and return to idle.
/// The device and crtc an output belongs to.
fn output_location(output: &Output) -> Option<(DrmNode, crtc::Handle)> {
    output
        .user_data()
        .get::<UdevOutputId>()
        .map(|id| (id.device_id, id.crtc))
}

/// The output currently driving `crtc`, looked up fresh rather than by identity: an
/// output that was switched off and on again is a different object.
fn output_for(data: &CalloopData, node: DrmNode, crtc: crtc::Handle) -> Option<Output> {
    data.state
        .space
        .outputs()
        .find(|output| output_location(output) == Some((node, crtc)))
        .cloned()
}

/// Switch a connector off: tear down its DRM output and set the monitor aside.
fn disable_output(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let global = {
        let Some(udev) = data.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(mut surface) = device.surfaces.remove(&crtc) else {
            return;
        };
        // Remember the connector: it stays plugged in, so the scanner will not
        // announce it again when the client asks for it back.
        device.disabled.insert(crtc, surface.connector.clone());
        surface.global.take()
        // Dropping the surface releases the crtc, via DrmOutput's Drop.
    };

    if let Some(global) = global {
        data.display_handle.remove_global::<Wlrix>(global);
    }

    if let Some(output) = output_for(data, node, crtc) {
        data.state.space.unmap_output(&output);
        // Keep the output so it can still be advertised as a disabled head.
        data.state.disabled_outputs.push(output);
    }

    info!(%node, ?crtc, "output disabled");

    let pointer = data.state.pointer_location();
    crate::placement::relocate_orphaned_windows(&mut data.state.space, pointer);
    let display_handle = data.display_handle.clone();
    data.state.advertise_outputs(&display_handle);
}

/// Switch a connector back on, rebuilding the output from the connector we kept.
fn enable_output(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let connector = data
        .udev
        .as_mut()
        .and_then(|udev| udev.backends.get_mut(&node))
        .and_then(|device| device.disabled.remove(&crtc));

    let Some(connector) = connector else {
        warn!(%node, ?crtc, "asked to enable an output that is not disabled");
        return;
    };

    data.state
        .disabled_outputs
        .retain(|output| output_location(output) != Some((node, crtc)));

    info!(%node, ?crtc, "output enabled");
    // Rebuilds the output, re-advertises and kicks off rendering.
    connector_connected(data, node, connector, crtc);
}

/// Carry out enable/disable requests accepted by the output-management protocol.
fn apply_pending_toggles(data: &mut CalloopData) {
    let toggles: Vec<(Output, bool)> = data.state.pending_output_toggles.drain(..).collect();
    for (output, enable) in toggles {
        let Some((node, crtc)) = output_location(&output) else {
            continue;
        };
        if enable {
            enable_output(data, node, crtc);
        } else {
            disable_output(data, node, crtc);
        }
    }
}

/// Carry out mode changes accepted by the output-management protocol.
///
/// Reprogramming a DRM output can only happen here, where the backend state lives, so
/// the protocol side queues them and this drains the queue.
fn apply_pending_mode_changes(data: &mut CalloopData) {
    let changes: Vec<(Output, WlMode)> = data.state.pending_mode_changes.drain(..).collect();

    for (queued, wl_mode) in changes {
        let Some((node, crtc)) = output_location(&queued) else {
            continue;
        };
        // Look the output up again: enabling it will have replaced the object.
        let Some(output) = output_for(data, node, crtc) else {
            continue;
        };

        let applied = {
            let Some(udev) = data.udev.as_mut() else {
                continue;
            };
            let Some(device) = udev.backends.get_mut(&node) else {
                continue;
            };
            let DeviceData {
                renderer, surfaces, ..
            } = device;
            let Some(surface) = surfaces.get_mut(&crtc) else {
                continue;
            };

            let Some(drm_mode) = surface
                .connector
                .modes()
                .iter()
                .copied()
                .find(|mode| WlMode::from(*mode) == wl_mode)
            else {
                warn!(output = %output.name(), ?wl_mode, "no matching DRM mode");
                continue;
            };

            let renderer = &mut *renderer.borrow_mut();
            match surface.drm_output.use_mode::<_, RenderElem>(
                drm_mode,
                renderer,
                &DrmOutputRenderElements::default(),
            ) {
                Ok(()) => true,
                Err(err) => {
                    warn!(output = %output.name(), ?err, "failed to set mode");
                    false
                }
            }
        };

        if !applied {
            continue;
        }

        info!(output = %output.name(), ?wl_mode, "mode set");
        output.change_current_state(Some(wl_mode), None, None, None);

        // The output changed size, so anything laid out against it has to follow.
        layer_map_for_output(&output).arrange();
        let pointer = data.state.pointer_location();
        crate::placement::relocate_orphaned_windows(&mut data.state.space, pointer);

        let display_handle = data.display_handle.clone();
        data.state.advertise_outputs(&display_handle);
    }
}

pub fn queue_redraw_all(data: &mut CalloopData) {
    // Toggles first: enabling rebuilds the output that a mode change then targets.
    apply_pending_toggles(data);
    apply_pending_mode_changes(data);

    let Some(udev) = data.udev.as_ref() else {
        return;
    };
    let targets: Vec<(DrmNode, crtc::Handle)> = udev
        .backends
        .iter()
        .flat_map(|(node, device)| device.surfaces.keys().map(move |crtc| (*node, *crtc)))
        .collect();

    for (node, crtc) in targets {
        queue_redraw(data, node, crtc);
    }
}

/// Record, per surface, which output it was primarily scanned out on. `select_dmabuf_feedback`
/// uses this to decide whether a client should get scanout or render formats.
fn update_scanout_outputs(state: &Wlrix, output: &Output, states: &RenderElementStates) {
    state.space.elements().for_each(|window| {
        window.with_surfaces(|surface, surface_states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_states,
                states,
                default_primary_scanout_output_compare,
            );
        });
    });

    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.with_surfaces(|surface, surface_states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_states,
                states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

fn render_surface(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let CalloopData { state, udev, .. } = data;
    let Some(udev) = udev.as_mut() else {
        return;
    };
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let DeviceData {
        renderer, surfaces, ..
    } = device;
    let Some(surface) = surfaces.get_mut(&crtc) else {
        return;
    };

    let Some(output) = state
        .space
        .outputs()
        .find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: node,
                    crtc,
                })
        })
        .cloned()
    else {
        return;
    };

    let renderer = &mut *renderer.borrow_mut();

    // Any screen capture waiting on the renderer is served first, so it reflects the
    // frame about to be shown rather than the previous one.
    crate::screencopy::take_pending(state, renderer);

    // Cursor on top; DrmCompositor may promote it to the hardware cursor plane.
    let elements: Vec<RenderElem> = crate::render::output_elements(state, renderer, &output, true);

    // FrameFlags::DEFAULT lets the DRM output assign buffers straight to planes,
    // so a compatible client buffer can be scanned out without a copy.
    let render_result = surface
        .drm_output
        .render_frame(renderer, &elements, CLEAR_COLOR, FrameFlags::DEFAULT)
        .map(|frame_result| (!frame_result.is_empty, frame_result.states));

    match render_result {
        Ok((rendered, states)) => {
            if rendered {
                match surface.drm_output.queue_frame(()) {
                    Ok(()) => surface.redraw_state = RedrawState::WaitingForVBlank { dirty: false },
                    Err(err) => {
                        warn!(?err, "queue_frame failed");
                        surface.redraw_state = RedrawState::Idle;
                    }
                }
            } else {
                // Nothing changed: go idle rather than polling. A damage source will
                // ping us when there is something to draw.
                surface.redraw_state = RedrawState::Idle;
            }

            // Record which output each surface was actually scanned out on; the
            // feedback below is chosen from this.
            update_scanout_outputs(state, &output, &states);

            // Let clients draw their next frame, and tell each one whether its buffer
            // is being scanned out directly (so it can allocate accordingly).
            let now = state.start_time.elapsed();
            state.space.elements().for_each(|window| {
                window.send_frame(
                    &output,
                    now,
                    Some(Duration::ZERO),
                    surface_primary_scanout_output,
                );
                if let Some(feedback) = surface.dmabuf_feedback.as_ref() {
                    window.send_dmabuf_feedback(
                        &output,
                        surface_primary_scanout_output,
                        |surf, _| {
                            select_dmabuf_feedback(
                                surf,
                                &states,
                                &feedback.render_feedback,
                                &feedback.scanout_feedback,
                            )
                        },
                    );
                }
            });

            // Layer surfaces (toolchest, desks, background) need the same treatment.
            let map = layer_map_for_output(&output);
            for layer in map.layers() {
                layer.send_frame(
                    &output,
                    now,
                    Some(Duration::ZERO),
                    surface_primary_scanout_output,
                );
                if let Some(feedback) = surface.dmabuf_feedback.as_ref() {
                    layer.send_dmabuf_feedback(
                        &output,
                        surface_primary_scanout_output,
                        |surf, _| {
                            select_dmabuf_feedback(
                                surf,
                                &states,
                                &feedback.render_feedback,
                                &feedback.scanout_feedback,
                            )
                        },
                    );
                }
            }
            drop(map);
        }
        Err(err) => {
            warn!(?err, "render_frame failed");
            surface.redraw_state = RedrawState::Idle;
        }
    }
}
