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
            DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmNode, DrmSurface, VrrSupport,
            compositor::{FrameError, FrameFlags, RenderFrameError},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind, ImportDma, ImportEgl,
            damage::OutputDamageTracker,
            element::{
                Element, RenderElementStates, default_primary_scanout_output_compare,
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

use crate::Wlrix;

/// Formats the primary framebuffer may use, preferring 10-bit then 8-bit.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// wlRIX desktop clear color (Indigo Magic-ish blue-gray). Placeholder.
use crate::render::desktop_background;

/// What a DRM output composites: desktop plus cursor.
type RenderElem = crate::render::OutputElem<GlesRenderer>;

/// Identifies which output a `wl_output` corresponds to (device + crtc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

/// All udev-backend state. Stored on [`Wlrix`] so event sources can reach it.
pub struct UdevState {
    session: LibSeatSession,
    loop_handle: LoopHandle<'static, Wlrix>,
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
    /// Set once the GPU has been reset out from under this device's GL context. Every
    /// texture, shader and framebuffer on it is gone, so rendering will keep failing --
    /// the flag exists so it is reported once rather than at the refresh rate.
    context_lost: bool,
    /// The color-conversion shaders, compiled against this device's GL context. `None` if they
    /// would not build, in which case every output on this card stays SDR and PQ content is not
    /// tone-mapped.
    color_pipeline: Option<crate::hdr_render::ColorPipeline>,
    /// How many times in a row [`reset_device_state`] has run without a frame landing since.
    ///
    /// The reset is the recovery for a driver that refuses the screen configuration, and a card
    /// that refuses the reset as well would otherwise be reset once per frame for the rest of
    /// the session. Cleared by the first frame that reaches the screen.
    resets: u8,
}

/// How many times running a card may be reset before the compositor stops trying.
///
/// Three is enough for the case this exists for -- a single divergence after a VT switch, which
/// one reset clears -- and small enough that a card which cannot be recovered says so quickly
/// instead of flashing every output at the refresh rate.
const MAX_CONSECUTIVE_RESETS: u8 = 3;

/// Per-surface dmabuf feedback: which formats a client should allocate for, depending
/// on whether its buffer can be scanned out directly or has to be composited.
///
/// Cheap to clone -- two refcounted handles -- which the render path relies on to use
/// the feedback while the backend state is no longer borrowed.
#[derive(Clone)]
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

/// The mode to light a connector up with, given an optional configured mode string.
///
/// A configured `WIDTHxHEIGHT@HZ` is honored only when the connector actually offers a
/// mode at that resolution; an exact refresh is preferred, otherwise the fastest at that
/// size. Anything unparseable or unavailable falls back to [`preferred_mode`] with a
/// warning, so a stale saved mode after a monitor swap degrades to a working picture
/// rather than a black screen.
fn configured_mode(
    connector: &connector::Info,
    wanted: Option<&str>,
) -> Option<drm::control::Mode> {
    let Some(spec) = wanted else {
        return preferred_mode(connector);
    };
    let Some((width, height, refresh)) = crate::outputs::parse_mode(spec) else {
        warn!(
            spec,
            "configured mode is not WIDTHxHEIGHT@HZ; using the preferred mode"
        );
        return preferred_mode(connector);
    };

    let size = (width as u16, height as u16);
    let at_size = || connector.modes().iter().filter(|mode| mode.size() == size);
    let chosen = match refresh {
        Some(hz) => at_size()
            .find(|mode| mode.vrefresh() == hz)
            .or_else(|| at_size().max_by_key(|mode| mode.vrefresh())),
        None => at_size().max_by_key(|mode| mode.vrefresh()),
    };

    match chosen {
        Some(mode) => Some(*mode),
        None => {
            warn!(
                spec,
                "configured mode is not offered by this display; using the preferred mode"
            );
            preferred_mode(connector)
        }
    }
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
            TrancheFlags::Scanout,
            plane_formats,
            4u32..=6,
        )
        .add_preference_tranche(
            render_node.dev_id(),
            TrancheFlags::Sampling,
            render_formats,
            4u32..=6,
        )
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

/// Why a frame did not reach the screen.
///
/// `why` is for the log. `config_rejected` is the one distinction that changes what happens
/// next, which is why the typed error is inspected here rather than formatted away at the call
/// site.
struct RenderFailure {
    why: String,
    /// The driver refused the screen configuration outright (`DrmError::TestFailed`).
    ///
    /// This is what a VT switch can leave behind: while the compositor was away, whoever held
    /// the card -- the console, or the session that had the VT before -- reprogrammed it, and
    /// the state smithay believes it is in no longer describes a configuration the driver will
    /// accept. Nothing about that heals on its own. Every later frame builds the same request
    /// and is refused the same way, so the compositor keeps running while the screen never
    /// updates again. See [`reset_device_state`], which is the way out.
    config_rejected: bool,
}

impl RenderFailure {
    /// A failure with no recovery of its own -- the message is all there is to act on.
    fn other(why: String) -> Self {
        Self {
            why,
            config_rejected: false,
        }
    }
}

/// Classify a `render_frame` error.
fn render_failure<A, B, F, R>(err: RenderFrameError<A, B, F, R>) -> RenderFailure
where
    A: std::error::Error + Send + Sync + 'static,
    B: std::error::Error + Send + Sync + 'static,
    F: std::error::Error + Send + Sync + 'static,
    R: std::error::Error,
{
    RenderFailure {
        config_rejected: matches!(
            err,
            RenderFrameError::PrepareFrame(FrameError::DrmError(DrmError::TestFailed(_)))
        ),
        why: format!("{err:?}"),
    }
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
    /// HDR, when the connector and the panel can both do it. `None` means this output is
    /// SDR and stays that way.
    hdr: Option<HdrSurface>,
    /// The offscreen the desktop is composited into while this output is in HDR. Allocated
    /// lazily on the first HDR frame and dropped when the mode changes under it.
    hdr_target: Option<crate::hdr_render::Target>,
}

/// What is needed to drive one connector's HDR, discovered when it comes up.
struct HdrSurface {
    props: HdrProps,
    /// The panel's colorimetry, from its EDID -- what goes into the metadata blob.
    mastering: crate::hdr::Mastering,
    /// The blob currently installed on the connector, so it can be freed when replaced.
    /// The kernel keeps its own reference while the property points at it, so this is
    /// destroyed only *after* a commit has moved the property elsewhere.
    blob: Option<u64>,
    /// Whether the connector has HDR *committed* right now.
    ///
    /// Not the same thing as `state.hdr.active()`, which is what the output is meant to be:
    /// a VT switch reprograms the connector behind our back, and the two deliberately
    /// disagree between then and the re-apply.
    active: bool,
}

/// The connector properties that switch a panel into HDR.
///
/// Smithay's `DrmSurface` builds its atomic requests from a fixed set of properties and has no
/// API for anything else, so these handles are looked up here and driven through the raw `drm`
/// crate -- the same way [`set_gamma`] reaches past smithay to the kernel.
struct HdrProps {
    colorspace: drm::control::property::Handle,
    /// Raw enum value for `BT2020_RGB`.
    bt2020_rgb: u64,
    /// Raw enum value for `Default`, to put the connector back to SDR.
    colorspace_default: u64,
    /// `HDR_OUTPUT_METADATA`, a blob property.
    metadata: drm::control::property::Handle,
    /// `max bpc`, if the connector has it. Optional because HDR is still worth having at
    /// 8 bpc, and on a bandwidth-limited link asking for 10 can cost the mode entirely.
    max_bpc: Option<drm::control::property::Handle>,
}

/// Bring up the udev/DRM backend and return `true` (it drives the event loop).
pub fn init_udev(
    event_loop: &mut EventLoop<'static, Wlrix>,
    state: &mut Wlrix,
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
    state.udev = Some(UdevState {
        session: session.clone(),
        loop_handle: loop_handle.clone(),
        backends: HashMap::new(),
    });
    // Damage-driven rendering: anything that changes the screen pings us, and we
    // queue redraws for the outputs. Without this an idle desktop would either
    // busy-render or never update.
    let (redraw_ping, redraw_source) = smithay::reexports::calloop::ping::make_ping()?;
    loop_handle.insert_source(redraw_source, |_, _, state| queue_redraw_all(state))?;
    state.redraw_ping = Some(redraw_ping);

    // Give the input handler a session handle for VT switching.
    state.session = Some(session.clone());
    info!("keybindings: Ctrl+Alt+F<n> switches VT, Ctrl+Alt+Backspace quits");

    // Input via libinput, tied to the session.
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.into());
    libinput_context.udev_assign_seat(&seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    loop_handle.insert_source(libinput_backend, move |event, _, state| {
        state.process_input_event(event);
    })?;

    // Session pause/resume (VT switch): suspend input + DRM, then reactivate.
    loop_handle.insert_source(notifier, move |event, _, state| match event {
        SessionEvent::PauseSession => {
            info!("session paused");
            libinput_context.suspend();
            if let Some(udev) = state.udev.as_mut() {
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
            if let Some(udev) = state.udev.as_mut() {
                for (node, backend) in udev.backends.iter_mut() {
                    let _ = backend.drm_output_manager.lock().activate(false);
                    for (crtc, surface) in backend.surfaces.iter_mut() {
                        // A frame may have been in flight when the session was paused,
                        // and its vblank will never arrive. Without resetting, the
                        // output would stay in WaitingForVBlank forever and the screen
                        // would be frozen after switching back.
                        surface.redraw_state = RedrawState::Idle;
                        // Taking the VT back reprograms the connector, and the blob the
                        // colorspace pointed at did not survive it. Forget it here so the
                        // re-apply below builds a fresh one rather than freeing an id the
                        // kernel has already reused.
                        if let Some(hdr) = surface.hdr.as_mut()
                            && hdr.active
                        {
                            hdr.blob = None;
                            hdr.active = false;
                        }
                        to_render.push((*node, *crtc));
                    }
                }
            }
            // Re-apply HDR before anything is drawn, for the same reason it is applied before
            // the first frame at connector-connect time: no flip is in flight yet. Without
            // this the panel silently drops back to SDR after a VT switch.
            let hdr_outputs: Vec<Output> = state
                .space
                .outputs()
                .chain(state.disabled_outputs.iter())
                .filter(|output| state.hdr.active(output))
                .cloned()
                .collect();
            for output in hdr_outputs {
                let _ = set_hdr(state, &output, true);
            }
            for (node, crtc) in to_render {
                queue_redraw(state, node, crtc);
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
    loop_handle.insert_source(udev_backend, move |event, _, state| match event {
        UdevEvent::Added { device_id, path } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id)
                && let Err(err) = device_added(state, node, &path)
            {
                warn!(?err, "failed to add drm device");
            }
        }
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_changed(state, node);
            }
        }
        UdevEvent::Removed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_removed(state, node);
            }
        }
    })?;

    // Bring up the devices that already exist.
    for (device_id, path) in devices {
        if let Ok(node) = DrmNode::from_dev_id(device_id)
            && let Err(err) = device_added(state, node, &path)
        {
            warn!(?err, "failed to add drm device at startup");
        }
    }

    Ok(true)
}

fn device_added(
    state: &mut Wlrix,
    node: DrmNode,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    {
        // Cloned up front so the handle stays usable while `udev` is borrowed.
        let display_handle = state.display_handle.clone();
        let udev = state.udev.as_mut().ok_or("udev state missing")?;

        // Open the DRM device through the session and wrap it.
        let fd = udev.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        // The first device to come up is the primary GPU -- the startup list is sorted to
        // put it there, and hotplugged secondaries can only arrive afterwards. It alone
        // backs the globals that tell clients where to allocate.
        let is_primary = state.dmabuf_state.is_none();

        // EGL + GLES renderer on this GPU.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let render_node = EGLDevice::device_for_display(&egl_display)
            .ok()
            .and_then(|device| device.try_get_render_node().ok().flatten());
        let egl_context = crate::backend::robust_context::create_context(&egl_display)?;
        let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();

        // Binding the wl_display exposes wl_drm/EGL to clients (Mesa needs this for
        // some hardware-buffer paths alongside linux-dmabuf).
        //
        // Primary GPU only, for the same reason as the dmabuf global and the capture
        // constraints below: wl_drm names exactly one render node, and a secondary card's
        // node is not somewhere a client of this compositor can be told to allocate. Left
        // unguarded this lands on whichever card happens to *have* the extension rather
        // than whichever card drives the screens -- Mesa dropped EGL_WL_bind_wayland_display,
        // so on a Mesa-primary/NVIDIA-secondary machine the bind silently fails on the card
        // doing the compositing and succeeds on the idle one, pointing clients at a GPU
        // whose buffers then have to be imported across the PCIe bus every frame.
        if is_primary && renderer.bind_wl_display(&display_handle).is_ok() {
            info!(%node, "EGL hardware acceleration enabled");
        }
        let dmabuf_formats = renderer.dmabuf_formats();
        // The color shaders belong to this device's GL context, so they are built here and
        // shared by every output on the card. Compiled up front rather than on the first HDR
        // frame: if they cannot be built, HDR is unavailable on this GPU whatever the config
        // says, and that is worth one line in the log at startup rather than a surprise the
        // first time someone turns it on.
        let color_pipeline = crate::hdr_render::ColorPipeline::new(&mut renderer);
        let renderer = Rc::new(RefCell::new(renderer));

        // Advertise linux-dmabuf-v1 once, backed by the first (preferably primary)
        // GPU, so clients can hand us GPU buffers instead of shared memory.
        if is_primary {
            let default_feedback = DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
                .build()
                .map_err(|err| format!("failed to build dmabuf feedback: {err}"))?;
            let mut dmabuf_state = DmabufState::new();
            let _global = dmabuf_state
                .create_global_with_default_feedback::<Wlrix>(&display_handle, &default_feedback);
            state.dmabuf_state = Some(dmabuf_state);
            state.renderer = Some(renderer.clone());
            info!(%node, "linux-dmabuf-v1 global created");

            // And what screen capture may be allocated as, on the same GPU. `render_formats`
            // rather than the import formats above: a capture is drawn *into* the client's
            // buffer, so a format this renderer can only sample from would negotiate fine and
            // then fail at `bind` on every frame.
            //
            // Only for the primary GPU, alongside the dmabuf global, and for the same reason:
            // a secondary card's render node is not somewhere a client of this compositor can
            // be told to allocate.
            state.capture_dmabuf = render_node.map(|render_node| {
                crate::image_capture::dmabuf_constraints(render_node, &render_formats)
            });
        }

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());
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
                .insert_source(drm_notifier, move |event, _, state| match event {
                    DrmEvent::VBlank(crtc) => frame_finish(state, node, crtc),
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
                context_lost: false,
                color_pipeline,
                resets: 0,
            },
        );
    }

    device_changed(state, node);
    Ok(())
}

fn device_changed(state: &mut Wlrix, node: DrmNode) {
    // Scan connectors, collecting events so we can drop the borrow before acting.
    let events: Vec<DrmScanEvent> = {
        let Some(udev) = state.udev.as_mut() else {
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
            } => connector_connected(state, node, connector, crtc),
            DrmScanEvent::Disconnected {
                connector: _,
                crtc: Some(crtc),
            } => connector_disconnected(state, node, crtc),
            _ => {}
        }
    }
}

fn device_removed(state: &mut Wlrix, node: DrmNode) {
    let crtcs: Vec<crtc::Handle> = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&node))
        .map(|device| device.surfaces.keys().copied().collect())
        .unwrap_or_default();

    for crtc in crtcs {
        connector_disconnected(state, node, crtc);
    }

    if let Some(udev) = state.udev.as_mut()
        && let Some(device) = udev.backends.remove(&node)
    {
        udev.loop_handle.remove(device.registration_token);
        info!(%node, "drm device removed");
    }
}

fn connector_connected(
    state: &mut Wlrix,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let display_handle = state.display_handle.clone();

    // The connector name is the stable key into the saved display settings. Look them up
    // (a cheap clone) before the backend's mutable borrow of `state` begins, so the rest
    // of the function can consult them freely.
    let output_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
    let out_cfg = state.display_config.get(&output_name).cloned();

    let Some(udev) = state.udev.as_mut() else {
        return;
    };
    let loop_handle = udev.loop_handle.clone();
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    info!(output = %output_name, ?crtc, "connector connected");

    let Some(drm_mode) =
        configured_mode(&connector, out_cfg.as_ref().and_then(|c| c.mode.as_deref()))
    else {
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
            // EDID parsing is off (see the smithay-drm-extras note in Cargo.toml), so there is
            // no real serial to report.
            serial_number: "Unknown".into(),
        },
    );
    let global = output.create_global::<Wlrix>(&display_handle);

    // Position: honor a saved layout, else lay outputs left-to-right.
    let position = match out_cfg.as_ref().and_then(|c| c.position) {
        Some([x, y]) => (x, y).into(),
        None => {
            let x = state.space.outputs().fold(0, |acc, o| {
                acc + state.space.output_geometry(o).unwrap().size.w
            });
            (x, 0).into()
        }
    };
    let transform = out_cfg
        .as_ref()
        .and_then(|c| c.transform())
        .unwrap_or(Transform::Normal);
    let scale = out_cfg.as_ref().and_then(|c| c.scale());
    // Register every mode the connector reports, so clients can enumerate and choose
    // between them; without this only the current mode is ever visible.
    for mode in connector.modes() {
        output.add_mode(WlMode::from(*mode));
    }
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), Some(transform), scale, Some(position));
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
        .lock()
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

    // Whether this screen can do adaptive sync at all is a property of the monitor,
    // the connector and the driver, and only the backend can ask. Cached so the
    // protocol code can report it and refuse what cannot work.
    let vrr_support = drm_output
        .with_compositor(|compositor| compositor.vrr_supported(connector.handle()))
        .unwrap_or(VrrSupport::NotSupported);
    let vrr_enabled = drm_output.with_compositor(|compositor| compositor.vrr_enabled());
    info!(
        output = %output.name(),
        ?vrr_support,
        vrr_enabled,
        "adaptive sync capability"
    );
    state
        .vrr
        .set_supported(&output, vrr_support != VrrSupport::NotSupported);
    state.vrr.set_enabled(&output, vrr_enabled);

    // HDR, likewise: the connector has to carry the properties *and* the panel has to say it
    // does PQ. Both come from the hardware, so both are cached for the protocol and render
    // paths. Logged either way -- a monitor that is silently not HDR-capable is exactly the
    // thing that is otherwise impossible to tell from a bug in the encode.
    let drm = device.drm_output_manager.device();
    let hdr_surface = hdr_props(drm, connector.handle()).and_then(|props| {
        let mastering = connector_edid(drm, connector.handle())
            .as_deref()
            .and_then(crate::hdr::edid_hdr_static_metadata)?;
        Some(HdrSurface {
            props,
            mastering,
            blob: None,
            active: false,
        })
    });
    match hdr_surface.as_ref() {
        Some(hdr) => info!(
            output = %output.name(),
            max_nits = hdr.mastering.max_luminance,
            min_nits = hdr.mastering.min_luminance,
            max_fall = hdr.mastering.max_frame_average,
            "HDR capable (PQ / BT.2020)"
        ),
        None => info!(output = %output.name(), "no HDR: connector or panel does not do PQ"),
    }
    state.hdr.set_supported(&output, hdr_surface.is_some());
    if let Some(hdr) = hdr_surface.as_ref() {
        state.hdr.set_mastering(&output, hdr.mastering);
    }
    if let Some(nits) = out_cfg.as_ref().and_then(|c| c.sdr_white_nits) {
        state.hdr.set_sdr_white(&output, nits);
    }
    if let Some(linear) = out_cfg.as_ref().and_then(|c| c.linear_blending) {
        state.hdr.set_linear_blending(&output, linear);
    }

    device.surfaces.insert(
        crtc,
        SurfaceData {
            global: Some(global),
            drm_output,
            dmabuf_feedback,
            connector: connector.clone(),
            redraw_state: RedrawState::Queued,
            hdr: hdr_surface,
            hdr_target: None,
        },
    );

    // Let wlr-output-management clients know the layout changed.
    state.advertise_outputs(&display_handle);

    // A saved-off monitor is still built in full -- so it can be advertised as a disabled
    // head and turned back on -- then switched straight off. `apply_pending_toggles`
    // updates `display_config` when a client toggles a head, so a later re-enable is not
    // undone here by a stale saved value.
    if out_cfg.as_ref().and_then(|c| c.enabled) == Some(false) {
        info!(output = %output.name(), "starting output disabled per saved config");
        disable_output(state, node, crtc);
        return;
    }

    // After the disabled check, so a monitor that starts off is not modeset into HDR only to
    // be switched straight back off -- but before the first render is queued, because nothing
    // has been drawn yet and that is the cheapest moment to take the modeset a colorspace
    // change costs.
    //
    // Asserted in both directions, not only when HDR is wanted. `Colorspace` and
    // `HDR_OUTPUT_METADATA` are connector state, and connector state outlives the DRM master
    // that set it: closing the device does not clear it, and the next master's modeset does not
    // either unless it names those properties. So a session that exited in HDR hands the panel
    // over still in PQ mode, and whoever comes next draws sRGB into it -- which is the
    // oversaturated greeter. [`restore_sdr`] cleans up on the way out, but a crash or a `kill
    // -9` never reaches it, so what a connector is found in is not trusted either way.
    if state.hdr.supported(&output) {
        let wanted = out_cfg.as_ref().and_then(|c| c.hdr) == Some(true);
        let _ = set_hdr(state, &output, wanted);
    }

    loop_handle.insert_idle(move |state| render_surface(state, node, crtc));
}

/// Drop everything the backend cached about a monitor that has been unplugged.
///
/// Both caches are keyed by *output name*, which is the connector name -- so plugging a different
/// monitor into `DP-4` produces an output with the same key. Without this it would inherit the
/// previous monitor's adaptive-sync capability and colorimetry, and be told it can do things it
/// cannot.
///
/// One function rather than two calls at each site: there are two disconnect paths, they are
/// easy to miss, and a third per-output cache added later should only have to be handled here.
fn forget_output(state: &mut Wlrix, output: &Output) {
    state.vrr.forget(output);
    state.hdr.forget(output);
}

fn connector_disconnected(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    let surface = {
        let Some(udev) = state.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        device.disabled.remove(&crtc);
        let surface = device.surfaces.remove(&crtc);
        // The connector is gone, so nothing references the metadata blob any more. Freed
        // while the device is still in hand -- after this block there is no fd to free it
        // through, and a leaked blob lives until the session ends.
        if let Some(blob) = surface.as_ref().and_then(|s| s.hdr.as_ref()?.blob) {
            use smithay::reexports::drm::control::Device as _;
            let _ = device
                .drm_output_manager
                .device()
                .destroy_property_blob(blob);
        }
        surface
    };

    // The cable was pulled while the output was switched off: drop the head we were
    // still advertising, since it can no longer be turned back on.
    //
    // This path returns below without reaching the enabled-output cleanup, so what the backend
    // cached about this monitor has to be dropped here too -- a disabled monitor that is
    // unplugged is just as gone as an enabled one.
    let disabled = state
        .disabled_outputs
        .iter()
        .find(|output| output_location(output) == Some((node, crtc)))
        .cloned();
    if let Some(output) = disabled {
        forget_output(state, &output);
        state
            .disabled_outputs
            .retain(|kept| output_location(kept) != Some((node, crtc)));
        info!(%node, ?crtc, "disabled connector unplugged");
        let display_handle = state.display_handle.clone();
        state.advertise_outputs(&display_handle);
    }

    let Some(mut surface) = surface else {
        return;
    };
    info!(%node, ?crtc, "connector disconnected");

    if let Some(global) = surface.global.take() {
        state.display_handle.remove_global::<Wlrix>(global);
    }
    // Remove the matching output from the space.
    let output = state.space.outputs().find(|o| {
        o.user_data().get::<UdevOutputId>()
            == Some(&UdevOutputId {
                device_id: node,
                crtc,
            })
    });
    if let Some(output) = output.cloned() {
        // Before it leaves the space: layer surfaces are anchored to this output and cannot
        // follow it anywhere, so their clients have to be told to build new ones.
        crate::handlers::layer_shell::close_layers_on(state, &output);
        forget_output(state, &output);
        state.space.unmap_output(&output);

        // Windows on that monitor are now at coordinates no output covers, so bring
        // them back onto a remaining one and re-anchor the shell components.
        let pointer = state.pointer_location();
        crate::placement::relocate_orphaned_windows(&mut state.space, pointer);

        let display_handle = state.display_handle.clone();
        state.advertise_outputs(&display_handle);

        state.request_redraw();
    }
}

/// vblank: the previous frame finished scanning out. Ack it and render the next.
fn frame_finish(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    {
        let Some(udev) = state.udev.as_mut() else {
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
    queue_redraw(state, node, crtc);
}

/// Queue a render of `crtc`, unless one is already pending.
fn queue_redraw(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    let Some(udev) = state.udev.as_mut() else {
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
            loop_handle.insert_idle(move |state| render_surface(state, node, crtc));
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
fn output_for(state: &Wlrix, node: DrmNode, crtc: crtc::Handle) -> Option<Output> {
    state
        .space
        .outputs()
        .find(|output| output_location(output) == Some((node, crtc)))
        .cloned()
}

/// Switch a connector off: tear down its DRM output and set the monitor aside.
fn disable_output(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    let global = {
        let Some(udev) = state.udev.as_mut() else {
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
        state.display_handle.remove_global::<Wlrix>(global);
    }

    if let Some(output) = output_for(state, node, crtc) {
        // Switched off by a client rather than unplugged, but a layer surface on it is just as
        // stranded either way; same reasoning as `connector_disconnected`.
        crate::handlers::layer_shell::close_layers_on(state, &output);
        state.space.unmap_output(&output);
        // Keep the output so it can still be advertised as a disabled head.
        state.disabled_outputs.push(output);
    }

    info!(%node, ?crtc, "output disabled");

    let pointer = state.pointer_location();
    crate::placement::relocate_orphaned_windows(&mut state.space, pointer);
    let display_handle = state.display_handle.clone();
    state.advertise_outputs(&display_handle);
}

/// Switch a connector back on, rebuilding the output from the connector we kept.
fn enable_output(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    let connector = state
        .udev
        .as_mut()
        .and_then(|udev| udev.backends.get_mut(&node))
        .and_then(|device| device.disabled.remove(&crtc));

    let Some(connector) = connector else {
        warn!(%node, ?crtc, "asked to enable an output that is not disabled");
        return;
    };

    state
        .disabled_outputs
        .retain(|output| output_location(output) != Some((node, crtc)));

    info!(%node, ?crtc, "output enabled");
    // Rebuilds the output, re-advertises and kicks off rendering.
    connector_connected(state, node, connector, crtc);
}

/// Carry out enable/disable requests accepted by the output-management protocol.
fn apply_pending_toggles(state: &mut Wlrix) {
    let toggles = std::mem::take(&mut state.pending_output_toggles);
    for (output, enable) in toggles {
        let Some((node, crtc)) = output_location(&output) else {
            continue;
        };
        // Record the client's intent against the connector name, so enabling a head does
        // not get undone when `enable_output` rebuilds it through `connector_connected`
        // and re-reads a stale saved `enabled = false`.
        let name = output.name();
        state
            .display_config
            .entry(name.clone())
            .or_insert_with(|| crate::outputs::OutputConfig {
                name,
                ..Default::default()
            })
            .enabled = Some(enable);
        if enable {
            enable_output(state, node, crtc);
        } else {
            disable_output(state, node, crtc);
        }
        state.outputs_dirty = true;
    }
}

/// Carry out mode changes accepted by the output-management protocol.
///
/// Reprogramming a DRM output can only happen here, where the backend state lives, so
/// the protocol side queues them and this drains the queue.
fn apply_pending_mode_changes(state: &mut Wlrix) {
    let changes = std::mem::take(&mut state.pending_mode_changes);

    for (queued, wl_mode) in changes {
        let Some((node, crtc)) = output_location(&queued) else {
            continue;
        };
        // Look the output up again: enabling it will have replaced the object.
        let Some(output) = output_for(state, node, crtc) else {
            continue;
        };

        let applied = {
            let Some(udev) = state.udev.as_mut() else {
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
        state.outputs_dirty = true;

        // The output changed size, so anything laid out against it has to follow.
        layer_map_for_output(&output).arrange();
        let pointer = state.pointer_location();
        crate::placement::relocate_orphaned_windows(&mut state.space, pointer);

        let display_handle = state.display_handle.clone();
        state.advertise_outputs(&display_handle);
    }
}

/// Turn adaptive sync on or off for outputs a client asked about.
///
/// Unlike a mode change this needs no modeset on most hardware, but the driver may say
/// otherwise (`VrrSupport::RequiresModeset`); either way the property is only settable
/// from here, where the DRM surface lives.
fn apply_pending_vrr_changes(state: &mut Wlrix) {
    let changes = std::mem::take(&mut state.pending_vrr_changes);

    for (output, wanted) in changes {
        let Some((node, crtc)) = output_location(&output) else {
            continue;
        };
        let applied = {
            let Some(udev) = state.udev.as_mut() else {
                continue;
            };
            let Some(device) = udev.backends.get_mut(&node) else {
                continue;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                continue;
            };
            match surface
                .drm_output
                .with_compositor(|compositor| compositor.use_vrr(wanted))
            {
                Ok(()) => true,
                Err(err) => {
                    warn!(output = %output.name(), wanted, ?err, "failed to set adaptive sync");
                    false
                }
            }
        };

        if !applied {
            continue;
        }
        info!(output = %output.name(), enabled = wanted, "adaptive sync set");
        state.vrr.set_enabled(&output, wanted);
        state.outputs_dirty = true;

        // Heads carry the adaptive sync state, so clients need re-advertising.
        let display_handle = state.display_handle.clone();
        state.advertise_outputs(&display_handle);
    }
}

pub fn queue_redraw_all(state: &mut Wlrix) {
    // Toggles first: enabling rebuilds the output that a mode change then targets.
    apply_pending_toggles(state);
    apply_pending_mode_changes(state);
    apply_pending_vrr_changes(state);

    // Persist the arrangement once, after the whole batch settled -- a client that moves
    // and re-modes several outputs at once writes the file a single time. A no-op unless
    // something above (or an earlier `apply_head`) marked the layout dirty.
    state.save_display_state_if_dirty();

    let Some(udev) = state.udev.as_ref() else {
        return;
    };
    let targets: Vec<(DrmNode, crtc::Handle)> = udev
        .backends
        .iter()
        .flat_map(|(node, device)| device.surfaces.keys().map(move |crtc| (*node, *crtc)))
        .collect();

    for (node, crtc) in targets {
        queue_redraw(state, node, crtc);
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
                None,
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
                None,
                states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

/// The surface driving `crtc` on `node`.
///
/// The render path reaches for this several times rather than holding it: the backend
/// state and the compositor state are fields of the same struct, so a borrow of one
/// rules out passing the other, and the render helpers need the whole of it.
/// Switch a monitor off or on at the hardware (DRM DPMS).
///
/// Off clears the surface, which drops the connector's power state and disables its planes.
/// On is implicit: smithay re-enables the connector when the next frame is queued, so this only
/// has to ask for a redraw -- [`crate::power`] has already cleared the "off" flag that was
/// making the render loop skip this output.
///
/// Only the udev backend can do this; under winit there is no connector to switch off.
pub fn set_output_power(state: &mut Wlrix, output: &Output, on: bool) {
    let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
        return;
    };
    if on {
        // Switching on is queueing a frame -- but only a frame with damage is queued, and
        // nothing need have changed on screen while the display was dark. Drop the buffers so
        // the next render is a full one and therefore actually reaches `queue_frame`, which is
        // what powers the connector back up. Same reasoning as the VT-resume path.
        if let Some(surface) = surface_for(state, id.device_id, id.crtc) {
            surface.drm_output.reset_buffers();
            surface.redraw_state = RedrawState::Idle;
        }
        queue_redraw(state, id.device_id, id.crtc);
        return;
    }
    let Some(surface) = surface_for(state, id.device_id, id.crtc) else {
        return;
    };
    if let Err(err) = surface
        .drm_output
        .with_compositor(|compositor| compositor.clear())
    {
        warn!(?err, "could not switch the output off");
    }
    // `clear` throws away the pending frame, so the vblank it was waiting for will never
    // arrive. Leaving the surface in `WaitingForVBlank` wedges it: the scheduler would only
    // mark it dirty and keep waiting for that vblank, so switching the display back on could
    // never draw and the screen would stay dark.
    surface.redraw_state = RedrawState::Idle;
}

/// How many entries this output's gamma table takes, or `0` if it has none.
///
/// Zero is the protocol's way of saying "this output cannot do gamma", which is the honest
/// answer under the nested backend (no CRTC) and for hardware without a gamma table.
pub fn gamma_size(state: &Wlrix, output: &Output) -> usize {
    let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
        return 0;
    };
    let Some(device) = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&id.device_id))
    else {
        return 0;
    };
    use smithay::reexports::drm::control::Device as _;
    device
        .drm_output_manager
        .device()
        .get_crtc(id.crtc)
        .map(|info| info.gamma_length() as usize)
        .unwrap_or(0)
}

/// Apply a color ramp to an output's CRTC, or reset it to linear with `None`.
///
/// Smithay has no gamma support, so this goes straight to the kernel through the `drm` crate.
pub fn set_gamma(
    state: &mut Wlrix,
    output: &Output,
    ramp: Option<(&[u16], &[u16], &[u16])>,
) -> Result<(), ()> {
    let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
        return Err(());
    };
    let size = gamma_size(state, output);
    if size == 0 {
        return Err(());
    }
    let Some(device) = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&id.device_id))
    else {
        return Err(());
    };

    // Resetting means a linear ramp: entry i scaled across the full 16-bit range, which is
    // what the CRTC starts with and what "no night light" looks like.
    let linear: Vec<u16> = (0..size)
        .map(|index| ((index as u64 * u16::MAX as u64) / (size.max(2) - 1) as u64) as u16)
        .collect();
    let (red, green, blue) = ramp.unwrap_or((&linear, &linear, &linear));

    use smithay::reexports::drm::control::Device as _;
    device
        .drm_output_manager
        .device()
        .set_gamma(id.crtc, red, green, blue)
        .map_err(|err| {
            warn!(?err, "could not set the gamma ramp");
        })
}

/// Look up the connector properties HDR needs, or `None` if this connector cannot do it.
///
/// All three conditions have to hold: the driver is on the atomic path (the legacy one cannot
/// set these at all), the connector offers `Colorspace` *with a `BT2020_RGB` entry*, and it has
/// `HDR_OUTPUT_METADATA`. A connector may well carry the properties without the monitor on the
/// end of the cable being able to use them, which is what the EDID check answers separately.
fn hdr_props(
    device: &smithay::backend::drm::DrmDevice,
    connector: connector::Handle,
) -> Option<HdrProps> {
    use smithay::reexports::drm::control::{Device as _, property};

    if !device.is_atomic() {
        return None;
    }

    let props = device.get_properties(connector).ok()?;
    let mut colorspace = None;
    let mut metadata = None;
    let mut max_bpc = None;
    for (handle, _) in props.iter() {
        let Ok(info) = device.get_property(*handle) else {
            continue;
        };
        match info.name().to_str() {
            Ok("Colorspace") => colorspace = Some((*handle, info)),
            Ok("HDR_OUTPUT_METADATA") => metadata = Some(*handle),
            Ok("max bpc") => max_bpc = Some(*handle),
            _ => {}
        }
    }

    let (colorspace, info) = colorspace?;
    let property::ValueType::Enum(values) = info.value_type() else {
        return None;
    };
    // The enum is not a fixed list -- amdgpu, i915 and the DP/HDMI paths each expose a
    // different subset -- so both entries are looked up by name rather than assumed.
    let (raw, entries) = values.values();
    let named = |wanted: &str| {
        entries
            .iter()
            .zip(raw)
            .find(|(entry, _)| entry.name().to_str() == Ok(wanted))
            .map(|(_, value)| *value)
    };

    Some(HdrProps {
        colorspace,
        bt2020_rgb: named("BT2020_RGB")?,
        colorspace_default: named("Default").unwrap_or(0),
        metadata: metadata?,
        max_bpc,
    })
}

/// The connector's EDID, from its `EDID` blob property.
fn connector_edid(
    device: &smithay::backend::drm::DrmDevice,
    connector: connector::Handle,
) -> Option<Vec<u8>> {
    use smithay::reexports::drm::control::Device as _;

    let props = device.get_properties(connector).ok()?;
    for (handle, value) in props.iter() {
        let Ok(info) = device.get_property(*handle) else {
            continue;
        };
        if info.name().to_str() == Ok("EDID") && *value != 0 {
            return device.get_property_blob(*value).ok();
        }
    }
    None
}

/// The bit depth to ask the link for, given what the primary framebuffer holds.
///
/// A 10-bit scanout buffer wants 10 bits on the wire; anything else the card hands out is 8.
fn scanout_bpc(format: Fourcc) -> u64 {
    match format {
        Fourcc::Abgr2101010 | Fourcc::Argb2101010 | Fourcc::Xbgr2101010 | Fourcc::Xrgb2101010 => 10,
        _ => 8,
    }
}

/// Switch an output into or out of HDR.
///
/// Goes straight to the kernel through the `drm` crate, as [`set_gamma`] does: smithay's atomic
/// surface builds its request from a fixed property set and has no way to carry these.
///
/// Changing `HDR_OUTPUT_METADATA` forces a full modeset on amdgpu, so this must not run against
/// a page flip in flight -- this checks for that. The request is tested before it is applied,
/// and `max bpc` is dropped and retried if it is what the driver refused: on a bandwidth-limited
/// link asking for 10 bpc can cost the mode, and HDR at 8 bpc beats no picture.
pub fn set_hdr(state: &mut Wlrix, output: &Output, on: bool) -> Result<(), ()> {
    let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
        return Err(());
    };
    let Some(surface) = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&id.device_id))
        .and_then(|device| device.surfaces.get(&id.crtc))
    else {
        return Err(());
    };
    // Changing the colorspace is a modeset, and committing one against a page flip already in
    // flight is how an output gets wedged or blacked out. Every caller here arranges to be in
    // that position -- before the first frame of a new connector, and after a VT switch has
    // reset every surface -- but the guard is what makes that a property of this function
    // rather than of remembering.
    //
    // `Queued` is fine and is what a fresh connector is in: it means a redraw is *scheduled*,
    // not that the kernel is holding a flip. Only `WaitingForVBlank` is the hazard.
    if matches!(surface.redraw_state, RedrawState::WaitingForVBlank { .. }) {
        warn!(
            output = %output.name(),
            "not switching HDR mode with a frame in flight"
        );
        return Err(());
    }
    commit_hdr(state, output, on)
}

/// The commit behind [`set_hdr`], without its in-flight guard.
///
/// Split out for [`restore_sdr`], which runs on the way out of the process: there the frame in
/// flight is the last one this session will ever draw, and refusing to modeset over it would
/// mean handing the panel to the next session still in PQ mode -- the failure this whole path
/// exists to prevent.
fn commit_hdr(state: &mut Wlrix, output: &Output, on: bool) -> Result<(), ()> {
    use smithay::reexports::drm::control::{
        AtomicCommitFlags, Device as _, atomic::AtomicModeReq, property,
    };

    let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
        return Err(());
    };
    let Some(device) = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&id.device_id))
    else {
        return Err(());
    };
    let Some(surface) = device.surfaces.get(&id.crtc) else {
        return Err(());
    };
    let Some(hdr) = surface.hdr.as_ref() else {
        // Asking for HDR on a display that cannot do it is a config mistake, not a crash.
        warn!(output = %output.name(), "this output cannot be driven in HDR");
        return Err(());
    };
    // What the link has to carry is what the scanout buffer holds, whether or not this output is
    // in HDR: the primary framebuffer is 10-bit wherever the card offers it (see
    // `SUPPORTED_FORMATS`), and asking for fewer bits than that would quantize the desktop on
    // the wire. Asking for more would only spend bandwidth on precision the buffer does not
    // have.
    let bpc = scanout_bpc(surface.drm_output.format());
    let connector = surface.connector.handle();
    let drm = device.drm_output_manager.device();

    // An empty blob is how the connector is told to stop sending metadata. The blob is created
    // before the request so its id can go into it, and cleaned up below if the commit fails.
    let new_blob = if on {
        let metadata = crate::hdr::HdrOutputMetadata::st2084(&hdr.mastering);
        match drm.create_property_blob(&metadata) {
            Ok(property::Value::Blob(id)) => Some(id),
            Ok(_) => return Err(()),
            Err(err) => {
                warn!(?err, "could not create the HDR metadata blob");
                return Err(());
            }
        }
    } else {
        None
    };

    let build = |with_max_bpc: bool| {
        let mut req = AtomicModeReq::new();
        req.add_property(
            connector,
            hdr.props.colorspace,
            property::Value::Unknown(if on {
                hdr.props.bt2020_rgb
            } else {
                hdr.props.colorspace_default
            }),
        );
        req.add_property(
            connector,
            hdr.props.metadata,
            property::Value::Blob(new_blob.unwrap_or(0)),
        );
        if with_max_bpc && let Some(handle) = hdr.props.max_bpc {
            req.add_property(connector, handle, property::Value::UnsignedRange(bpc));
        }
        req
    };

    // ALLOW_MODESET even for the test: switching output colorspace *is* a modeset, so a test
    // without it would be rejected for the wrong reason.
    let flags = AtomicCommitFlags::ALLOW_MODESET;
    let with_max_bpc = drm
        .atomic_commit(flags | AtomicCommitFlags::TEST_ONLY, build(true))
        .is_ok();
    if !with_max_bpc {
        info!(
            output = %output.name(),
            bpc,
            "driver refused that bit depth; keeping the current one"
        );
    }
    let result = drm.atomic_commit(flags, build(with_max_bpc));

    match result {
        Ok(()) => {
            // The property now points at the new blob (or at nothing), so the old one is safe
            // to free. Doing it in the other order would drop the blob out from under the
            // commit that is still referencing it.
            let old = hdr.blob;
            if let Some(surface) = state
                .udev
                .as_mut()
                .and_then(|udev| udev.backends.get_mut(&id.device_id))
                .and_then(|device| device.surfaces.get_mut(&id.crtc))
                .and_then(|surface| surface.hdr.as_mut())
            {
                surface.blob = new_blob;
                surface.active = on;
            }
            if let Some(old) = old
                && let Some(device) = state
                    .udev
                    .as_ref()
                    .and_then(|udev| udev.backends.get(&id.device_id))
            {
                let _ = device
                    .drm_output_manager
                    .device()
                    .destroy_property_blob(old);
            }
            state.hdr.set_active(output, on);
            // This output is now a different color than it was, and any client that asked to
            // be told needs to know before it draws its next frame.
            state.color_description_changed(output);
            info!(output = %output.name(), on, "HDR mode set");
            Ok(())
        }
        Err(err) => {
            warn!(output = %output.name(), ?err, "could not switch HDR mode");
            if let Some(blob) = new_blob {
                let _ = drm.destroy_property_blob(blob);
            }
            Err(())
        }
    }
}

/// Put every HDR output back to SDR, as the last thing this process does with the card.
///
/// `Colorspace` and `HDR_OUTPUT_METADATA` are connector state, and connector state outlives the
/// DRM master that set it: closing the device does not clear them, and the next master's modeset
/// does not touch properties it never names. So a session that exits in HDR hands the panel to
/// the greeter still expecting PQ / BT.2020, and the greeter -- which knows nothing about any of
/// this -- draws ordinary sRGB into it. That is the oversaturated login screen.
///
/// Best effort by design. A crash, a `kill -9` or a lost VT never reaches this, which is why
/// [`connector_connected`] asserts the colorspace it wants rather than trusting what it finds.
pub fn restore_sdr(state: &mut Wlrix) {
    // Disabled outputs too: a monitor switched off by a client still has a connector, and
    // whatever was last committed to it is still what the next session inherits.
    let outputs: Vec<Output> = state
        .space
        .outputs()
        .chain(state.disabled_outputs.iter())
        .filter(|output| state.hdr.active(output))
        .cloned()
        .collect();
    for output in outputs {
        // The session's last frame may still be in flight. A blocking atomic commit ordinarily
        // waits that out rather than failing, but there is no event loop left to service a
        // vblank in if a driver returns `EBUSY` instead -- so the flip is outwaited here. A few
        // short retries span a frame at any rate a monitor runs at, and cost nothing when the
        // first commit already went through.
        for attempt in 0..RESTORE_SDR_ATTEMPTS {
            if commit_hdr(state, &output, false).is_ok() {
                break;
            }
            if attempt + 1 < RESTORE_SDR_ATTEMPTS {
                std::thread::sleep(RESTORE_SDR_RETRY);
            } else {
                warn!(
                    output = %output.name(),
                    "could not put the connector back to SDR; the next session inherits PQ"
                );
            }
        }
    }
}

/// How many times [`restore_sdr`] retries a connector, and how long it waits between tries.
///
/// Three waits of eight milliseconds outlast a frame at any refresh rate a monitor runs at, and
/// cap what a shutdown can spend on a card that is not going to answer.
const RESTORE_SDR_ATTEMPTS: u32 = 4;
const RESTORE_SDR_RETRY: Duration = Duration::from_millis(8);

/// Put a DRM device back to a state the driver will accept, after it has refused one.
///
/// A VT switch hands the card to someone else, and they reprogram it. Smithay reads the mode and
/// the connector routing back when the session resumes, but not the rest of what a foreign
/// master may have left behind, so the first commit after the switch can be rejected outright --
/// and every commit after it is built the same way and rejected the same way. The compositor
/// goes on running, the screen never updates again.
///
/// The way out is the hammer smithay provides for exactly this: disable every connector and
/// plane on the card, which is a configuration no driver refuses, and let the next frame modeset
/// back up from a state both sides agree on. That blanks every output on this device, which is
/// why it happens only once the driver has actually said no.
fn reset_device_state(state: &mut Wlrix, node: DrmNode) {
    let crtcs = {
        let Some(device) = state
            .udev
            .as_mut()
            .and_then(|udev| udev.backends.get_mut(&node))
        else {
            return;
        };
        // Saturating: once the budget is spent this keeps being reached, once per refused
        // frame, and the counter is only ever compared against the budget.
        device.resets = device.resets.saturating_add(1);
        let attempt = device.resets;
        if attempt > MAX_CONSECUTIVE_RESETS {
            // Once, on the attempt that runs the budget out; after that this is silent, or a
            // card that cannot be recovered would log at the refresh rate.
            if attempt == MAX_CONSECUTIVE_RESETS + 1 {
                error!(%node, "drm state reset did not take; outputs on this device are stuck");
            }
            return;
        }
        if let Err(err) = device.drm_output_manager.device_mut().reset_state() {
            warn!(%node, ?err, "could not reset the drm device");
            return;
        }
        warn!(%node, attempt, "drm state reset after the driver refused a frame");

        let crtcs: Vec<crtc::Handle> = device.surfaces.keys().copied().collect();
        for crtc in &crtcs {
            let Some(surface) = device.surfaces.get_mut(crtc) else {
                continue;
            };
            // Resetting the device re-reads each *surface*'s state, but the compositors layered
            // on top also have to be told, or the next frame is built as a partial update
            // against damage the disable just invalidated -- and would be skipped as empty.
            surface.drm_output.with_compositor(|compositor| {
                if let Err(err) = compositor.reset_state() {
                    warn!(?err, "could not reset a drm compositor");
                }
            });
            // Nothing is in flight any more: the disable took any pending flip with it, and its
            // vblank is not coming.
            surface.redraw_state = RedrawState::Idle;
        }
        crtcs
    };

    // Every output on this card was just blanked, not only the one whose frame was refused, so
    // they all have to be drawn again.
    for crtc in crtcs {
        queue_redraw(state, node, crtc);
    }
}

fn surface_for(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) -> Option<&mut SurfaceData> {
    state
        .udev
        .as_mut()?
        .backends
        .get_mut(&node)?
        .surfaces
        .get_mut(&crtc)
}

fn render_surface(state: &mut Wlrix, node: DrmNode, crtc: crtc::Handle) {
    // Read once: `state` is borrowed several ways below, and this is a `Copy` color.
    let clear_color = desktop_background(state.palette);
    // While the session is paused -- the VT is switched away -- DRM is inactive, so a
    // render would only fail with `DeviceInactive`. Clients (a blinking caret, say) keep
    // committing frames the whole time, so without this the log fills with one warning
    // per attempted frame until the VT comes back. `ActivateSession` redraws everything.
    if !state
        .udev
        .as_ref()
        .is_some_and(|udev| udev.session.is_active())
    {
        return;
    }

    // Nothing to draw on if the surface has gone; checked before anything else so a
    // vanished crtc costs no work.
    if surface_for(state, node, crtc).is_none() {
        return;
    }

    // A device whose GL context died to a GPU reset cannot draw. Every frame would build a
    // full element list only to fail in the same place, so stop before doing the work; the
    // reason was logged once, where it was detected.
    if state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&node))
        .is_some_and(|device| device.context_lost)
    {
        return;
    }

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

    // A monitor that has been switched off (DPMS, by the idle timeout or a client) must not be
    // drawn to: queueing a frame is exactly what smithay uses to power a connector back on, so
    // rendering here would undo the blank a frame later.
    //
    // The surface is put back to `Idle` on the way out. Returning while it is still `Queued`
    // would wedge it -- the scheduler skips a surface that is already queued, so no later
    // request could schedule a render and the display could never come back on.
    if !state.output_powered(&output) {
        if let Some(surface) = surface_for(state, node, crtc) {
            surface.redraw_state = RedrawState::Idle;
        }
        return;
    }

    // The renderer is refcounted, so unlike the rest of the backend state it can be
    // held across calls that need the compositor state.
    let Some(renderer) = state
        .udev
        .as_ref()
        .and_then(|udev| udev.backends.get(&node))
        .map(|device| device.renderer.clone())
    else {
        return;
    };
    let renderer = &mut *renderer.borrow_mut();

    // Any screen capture waiting on the renderer is served first, so it reflects the
    // frame about to be shown rather than the previous one.
    crate::screencopy::take_pending(state, renderer);
    crate::image_capture::take_pending(state, renderer);
    // Snapshot any freshly minimized windows for their icons while the renderer is here.
    state.capture_pending_thumbnails(renderer, &output);

    // Cursor on top; DrmCompositor may promote it to the hardware cursor plane.
    let elements: Vec<RenderElem> = crate::render::output_elements(state, renderer, &output, true);

    // Read out of the compositor state before the backend is borrowed below.
    let hdr_active = state.hdr.active(&output);
    let sdr_white = state.hdr.sdr_white(&output);
    // Where alpha compositing happens on this output. Only meaningful with HDR: an SDR output
    // has no offscreen to composite into, and changing how the whole desktop blends there would
    // be a visible change with nothing to gain from it.
    let space = if state.hdr.linear_blending(&output) {
        crate::hdr_render::WorkingSpace::Linear
    } else {
        crate::hdr_render::WorkingSpace::Encoded
    };
    // Which elements carry PQ content rather than sRGB. Empty unless a client has actually
    // tagged a surface, which is the usual case, so this costs nothing on an ordinary desktop.
    // Needed on SDR outputs too: a window tagged PQ on the HDR monitor and dragged onto an SDR
    // one keeps sending PQ, because nothing tells a client its window moved.
    let pq_elements = state.color_management.pq_elements();
    // The logical extent of this output, and the physical pixels that corresponds to. Derived
    // in this direction on purpose: the encode element is sized logically, so making the
    // offscreen exactly what that logical size rasterizes to is what keeps the blit 1:1 and
    // stops the whole screen being resampled at a fractional scale.
    let logical = state
        .space
        .output_geometry(&output)
        .map(|geometry| geometry.size)
        .unwrap_or_default();
    let scale = output.current_scale().fractional_scale();
    let physical = logical.to_physical_precise_round(scale);

    let render_result = {
        let Some(device) = state
            .udev
            .as_mut()
            .and_then(|udev| udev.backends.get_mut(&node))
        else {
            return;
        };
        // Disjoint fields of the same device: the shader is shared by every output on the
        // card, the offscreen belongs to this one.
        let pipeline = device.color_pipeline.as_ref();
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        match pipeline.filter(|_| hdr_active) {
            // An HDR output composites into an offscreen and then encodes it, because the
            // scanout buffer has to hold PQ / BT.2020 and nothing between here and the panel
            // can do that conversion. See `crate::hdr_render`.
            Some(pipeline) => {
                if surface.hdr_target.as_ref().map(|target| target.size)
                    != Some((physical.w, physical.h).into())
                {
                    surface.hdr_target =
                        crate::hdr_render::Target::new(renderer, (physical.w, physical.h).into());
                }

                // Everything is wrapped, but only a surface a client has tagged as PQ gets a
                // decode shader in front of it; the rest pass straight through. That is what
                // lets one pass mix a PQ video with the sRGB desktop around it.
                let decoded: Vec<crate::hdr_render::Decoded<RenderElem>> = elements
                    .into_iter()
                    .map(|element| {
                        if pq_elements.iter().any(|(id, _)| id == element.id()) {
                            // The output's SDR white, not the content's: PQ is absolute, so an
                            // HDR output reproduces the content's luminance as authored and
                            // this only fixes where the *desktop's* white sits.
                            pipeline.decoded(element, sdr_white, space)
                        } else {
                            // Solid colors cannot be reached by a shader -- the renderer's
                            // solid program is not overridable -- so the element is rebuilt
                            // around a converted color instead. Everything else is a texture
                            // and is linearized as it is sampled.
                            match element {
                                RenderElem::Solid(solid) => {
                                    let converted =
                                        crate::hdr_render::ColorPipeline::solid(&solid, space);
                                    pipeline.plain(RenderElem::Solid(converted), space)
                                }
                                other => pipeline.plain(other, space),
                            }
                        }
                    })
                    .collect();

                // Pass 1: the desktop, exactly as an SDR output draws it -- same elements,
                // same blend space, no visual change. Transform::Normal because the encode
                // element goes through `render_frame`, which applies the output transform
                // itself; doing it here as well would rotate the screen twice.
                let pass = surface
                    .hdr_target
                    .as_mut()
                    .ok_or_else(|| {
                        RenderFailure::other("no HDR offscreen for this output".to_string())
                    })
                    .and_then(|target| {
                        let mut tracker =
                            OutputDamageTracker::new(physical, scale, Transform::Normal);
                        let mut framebuffer = renderer
                            .bind(target.texture())
                            .map_err(|err| RenderFailure::other(format!("{err:?}")))?;
                        tracker
                            .render_output(
                                renderer,
                                &mut framebuffer,
                                0,
                                &decoded,
                                crate::hdr_render::ColorPipeline::to_working(clear_color, space),
                            )
                            .map_err(|err| RenderFailure::other(format!("{err:?}")))
                            .map(|_| ())
                    });

                match pass {
                    // Pass 2: one full-screen element carrying the encode shader.
                    //
                    // `FrameFlags::empty()`, not DEFAULT: any plane promotion would put
                    // un-encoded content straight on the wire. That costs direct scanout and
                    // the hardware cursor on this output, which is the price of the encode.
                    Ok(()) => {
                        let target = surface.hdr_target.as_ref().expect("allocated above");
                        let encoded = pipeline.element(renderer, target, sdr_white, space);
                        surface
                            .drm_output
                            .render_frame(renderer, &[encoded], clear_color, FrameFlags::empty())
                            .map(|frame_result| (!frame_result.is_empty, frame_result.states))
                            .map_err(render_failure)
                    }
                    Err(err) => Err(err),
                }
            }
            // An SDR output. Ordinarily this is untouched -- straight to `render_frame`, with
            // FrameFlags::DEFAULT so a compatible client buffer can go to a plane without a
            // copy. Only when a PQ surface is present does anything wrap, and then only that
            // surface is converted; `Decoded` still forwards `underlying_storage` for the rest,
            // so the desktop keeps its planes.
            None => match pipeline.filter(|_| !pq_elements.is_empty()) {
                Some(pipeline) => {
                    let mapped: Vec<crate::hdr_render::Decoded<RenderElem>> = elements
                        .into_iter()
                        .map(|element| {
                            match pq_elements.iter().find(|(id, _)| id == element.id()) {
                                Some((_, reference)) => pipeline.tonemapped(element, *reference),
                                None => pipeline
                                    .plain(element, crate::hdr_render::WorkingSpace::Encoded),
                            }
                        })
                        .collect();
                    surface
                        .drm_output
                        .render_frame(renderer, &mapped, clear_color, FrameFlags::DEFAULT)
                        .map(|frame_result| (!frame_result.is_empty, frame_result.states))
                        .map_err(render_failure)
                }
                None => surface
                    .drm_output
                    .render_frame(renderer, &elements, clear_color, FrameFlags::DEFAULT)
                    .map(|frame_result| (!frame_result.is_empty, frame_result.states))
                    .map_err(render_failure),
            },
        }
    };

    // A locked frame has now been composited, so the lock can be confirmed.
    crate::session_lock::after_render(state);

    match render_result {
        Ok((rendered, states)) => {
            let mut config_rejected = false;
            {
                let Some(surface) = surface_for(state, node, crtc) else {
                    return;
                };
                if rendered {
                    match surface.drm_output.queue_frame(()) {
                        Ok(()) => {
                            surface.redraw_state = RedrawState::WaitingForVBlank { dirty: false }
                        }
                        Err(err) => {
                            // The same divergence `render_frame` can hit, one step later:
                            // whether the driver is asked to test the configuration here or
                            // there depends on whether a modeset is pending. See
                            // [`RenderFailure::config_rejected`].
                            config_rejected =
                                matches!(err, FrameError::DrmError(DrmError::TestFailed(_)));
                            warn!(?err, "queue_frame failed");
                            surface.redraw_state = RedrawState::Idle;
                        }
                    }
                } else {
                    // Nothing changed: go idle rather than polling. A damage source
                    // will ping us when there is something to draw.
                    surface.redraw_state = RedrawState::Idle;
                }
            }
            if config_rejected {
                reset_device_state(state, node);
                return;
            }
            // A frame reached the kernel, so whatever the card was refusing before is over and
            // the next divergence gets the full reset budget again.
            if let Some(device) = state
                .udev
                .as_mut()
                .and_then(|udev| udev.backends.get_mut(&node))
            {
                device.resets = 0;
            }

            // Record which output each surface was actually scanned out on; the
            // feedback below is chosen from this.
            update_scanout_outputs(state, &output, &states);

            // Let clients draw their next frame, and tell each one whether its buffer
            // is being scanned out directly (so it can allocate accordingly).
            // Cloned out because the loops below need the compositor state, which
            // cannot be borrowed alongside the backend.
            let feedback =
                surface_for(state, node, crtc).and_then(|surface| surface.dmabuf_feedback.clone());

            let now = state.start_time.elapsed();
            state.space.elements().for_each(|window| {
                window.send_frame(
                    &output,
                    now,
                    Some(Duration::ZERO),
                    surface_primary_scanout_output,
                );
                if let Some(feedback) = feedback.as_ref() {
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

            // A drag-and-drop icon lives in neither the space nor the layer map -- it is a
            // bare surface the source client hands us for the duration of a drag -- so neither
            // loop reaches it, and without this it draws one frame and then freezes mid-drag.
            // It is never a scanout candidate, so it just gets this output.
            if let Some(icon) = state.dnd_icon.as_ref() {
                smithay::desktop::utils::send_frames_surface_tree(
                    &icon.surface,
                    &output,
                    now,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }

            // Layer surfaces (toolchest, desks, background) need the same treatment.
            let map = layer_map_for_output(&output);
            for layer in map.layers() {
                layer.send_frame(
                    &output,
                    now,
                    Some(Duration::ZERO),
                    surface_primary_scanout_output,
                );
                if let Some(feedback) = feedback.as_ref() {
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
        Err(failure) => {
            warn!(err = failure.why, "render_frame failed");

            // A rejected command submission or a hung engine takes the GL context with it,
            // and every GL object on this device dies with it. Mesa would have aborted the
            // process right here if the context were not robust (see
            // `backend::robust_context`); it does not any more, so the session, the clients
            // and the VT switch all survive -- but nothing on this GPU will draw again, and
            // saying so once beats a warning per vblank.
            if let Some(cause) = crate::backend::robust_context::gpu_reset(renderer)
                && let Some(device) = state
                    .udev
                    .as_mut()
                    .and_then(|udev| udev.backends.get_mut(&node))
                && !device.context_lost
            {
                device.context_lost = true;
                error!(
                    %node,
                    cause,
                    "GPU reset: outputs on this device are frozen until the session restarts"
                );
            }

            if let Some(surface) = surface_for(state, node, crtc) {
                surface.redraw_state = RedrawState::Idle;
            }

            // The one failure that does not repair itself and does not need the session
            // restarted. Deliberately after the GPU-reset check: a lost context refuses
            // everything, and resetting the card would not give it back.
            if failure.config_rejected {
                reset_device_state(state, node);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bit depth asked of the link has to match the buffer actually being scanned out:
    /// asking for less quantizes the desktop on the wire, asking for more spends bandwidth on
    /// precision that is not there.
    ///
    /// Spelled out against `SUPPORTED_FORMATS` in order rather than checked one by one, so a
    /// format added to that list has to be given a depth here too instead of falling through
    /// the catch-all to 8.
    #[test]
    fn every_scanout_format_asks_the_link_for_the_depth_it_holds() {
        let depths: Vec<u64> = SUPPORTED_FORMATS.iter().copied().map(scanout_bpc).collect();
        assert_eq!(depths, vec![10, 10, 8, 8], "{SUPPORTED_FORMATS:?}");
    }
}
