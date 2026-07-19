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
//! Single-GPU for now (one `GlesRenderer` per DRM device). Deferred: client-buffer
//! dmabuf import + scanout feedback (`DmabufHandler`) for true client zero-copy,
//! cursor planes, and multi-GPU. See Smithay's `anvil` example for those.

use std::{collections::HashMap, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{Color32F, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        Window,
        space::{SpaceRenderElements, space_render_elements},
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{EventLoop, LoopHandle, RegistrationToken},
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
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
const CLEAR_COLOR: Color32F = Color32F::new(0.16, 0.18, 0.27, 1.0);

/// Render element type for the space on a DRM output.
type RenderElem = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Identifies which output a `wl_output` corresponds to (device + crtc).
#[derive(Debug, PartialEq, Eq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

/// All udev-backend state. Stored in [`CalloopData`] so event sources can reach it.
pub struct UdevState {
    session: LibSeatSession,
    /// Reserved for dmabuf feedback / multi-GPU (client zero-copy) in the next increment.
    #[allow(dead_code)]
    primary_gpu: DrmNode,
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
    renderer: GlesRenderer,
    /// Reserved for dmabuf feedback / framebuffer-export target in the next increment.
    #[allow(dead_code)]
    render_node: Option<DrmNode>,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    registration_token: RegistrationToken,
}

/// Per-connector (crtc) output state.
struct SurfaceData {
    /// Reserved (identifies the owning device); used once feedback/scanout lands.
    #[allow(dead_code)]
    device_id: DrmNode,
    global: Option<GlobalId>,
    drm_output:
        DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
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
        primary_gpu,
        loop_handle: loop_handle.clone(),
        backends: HashMap::new(),
    });
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
                    for crtc in backend.surfaces.keys() {
                        to_render.push((*node, *crtc));
                    }
                }
            }
            for (node, crtc) in to_render {
                schedule_render(data, node, crtc);
            }
        }
    })?;

    // udev device hotplug (GPU add/change/remove).
    let udev_backend = UdevBackend::new(&seat_name)?;
    let devices: Vec<_> = udev_backend
        .device_list()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();
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
        let udev = data.udev.as_mut().ok_or("udev state missing")?;

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
        let renderer = unsafe { GlesRenderer::new(egl_context)? };
        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();

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

    let mode_id = connector
        .modes()
        .iter()
        .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let drm_mode = connector.modes()[mode_id];
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
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some(position));
    state.space.map_output(&output, position);
    output.user_data().insert_if_missing(|| UdevOutputId {
        device_id: node,
        crtc,
    });

    let planes = device.drm_output_manager.device().planes(&crtc).ok();
    let drm_output = match device
        .drm_output_manager
        .initialize_output::<_, RenderElem>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            planes,
            &mut device.renderer,
            &DrmOutputRenderElements::default(),
        ) {
        Ok(drm_output) => drm_output,
        Err(err) => {
            warn!(?err, "failed to initialize drm output");
            return;
        }
    };

    device.surfaces.insert(
        crtc,
        SurfaceData {
            device_id: node,
            global: Some(global),
            drm_output,
        },
    );

    loop_handle.insert_idle(move |data| render_surface(data, node, crtc));
}

fn connector_disconnected(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    let Some(udev) = data.udev.as_mut() else {
        return;
    };
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };
    let Some(mut surface) = device.surfaces.remove(&crtc) else {
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
    }
    schedule_render(data, node, crtc);
}

/// Queue a render of `crtc` on the next event-loop idle turn.
fn schedule_render(data: &mut CalloopData, node: DrmNode, crtc: crtc::Handle) {
    if let Some(udev) = data.udev.as_ref() {
        udev.loop_handle
            .insert_idle(move |data| render_surface(data, node, crtc));
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

    let elements: Vec<RenderElem> =
        space_render_elements::<_, Window, _>(renderer, [&state.space], &output, 1.0)
            .unwrap_or_default();

    match surface
        .drm_output
        .render_frame(renderer, &elements, CLEAR_COLOR, FrameFlags::DEFAULT)
    {
        Ok(frame_result) => {
            let rendered = !frame_result.is_empty;
            if rendered && let Err(err) = surface.drm_output.queue_frame(()) {
                warn!(?err, "queue_frame failed");
            }

            // Let clients draw their next frame.
            let now = state.start_time.elapsed();
            state.space.elements().for_each(|window| {
                window.send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                })
            });

            // No damage this turn: poll again shortly so new commits get drawn.
            if !rendered {
                let _ = udev.loop_handle.insert_source(
                    smithay::reexports::calloop::timer::Timer::from_duration(
                        Duration::from_millis(16),
                    ),
                    move |_, _, data| {
                        render_surface(data, node, crtc);
                        smithay::reexports::calloop::timer::TimeoutAction::Drop
                    },
                );
            }
        }
        Err(err) => {
            warn!(?err, "render_frame failed");
        }
    }
}
