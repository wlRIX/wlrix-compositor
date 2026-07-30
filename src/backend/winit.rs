// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use std::time::Duration;

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{ImportDma, ImportEgl, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState},
};
use tracing::{info, warn};

use crate::Wlrix;

/// The nested backend, stored on [`Wlrix`].
pub type WinitBackend = winit::WinitGraphicsBackend<GlesRenderer>;

/// What this backend composites: desktop plus cursor.
type OutputElem = crate::render::OutputElem<GlesRenderer>;

/// Nested-window background.
use crate::render::DESKTOP_BACKGROUND as CLEAR_COLOR;

pub fn init_winit(
    event_loop: &mut EventLoop<Wlrix>,
    state: &mut Wlrix,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_handle = &mut state.display_handle;

    let (mut backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<Wlrix>(display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));
    // Give the output a layer map sized to it, for wlr-layer-shell clients.
    smithay::desktop::layer_map_for_output(&output).arrange();

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Expose GPU buffer sharing so hardware-accelerated clients (alacritty, vkcube,
    // toolkits) work in the nested dev loop too, not just on real hardware. Prefer
    // dmabuf feedback when we can identify a render node, else fall back to dmabuf v3.
    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    let mut dmabuf_state = DmabufState::new();
    match render_node {
        Ok(Some(node)) => {
            let feedback =
                DmabufFeedbackBuilder::new(node.dev_id(), backend.renderer().dmabuf_formats())
                    .build()
                    .map_err(|err| format!("failed to build dmabuf feedback: {err}"))?;
            let _global = dmabuf_state
                .create_global_with_default_feedback::<Wlrix>(display_handle, &feedback);
        }
        other => {
            if let Err(err) = other {
                warn!(
                    ?err,
                    "no render node for the winit display; using dmabuf v3"
                );
            }
            let _global = dmabuf_state
                .create_global::<Wlrix>(display_handle, backend.renderer().dmabuf_formats());
        }
    }
    state.dmabuf_state = Some(dmabuf_state);

    // wl_drm, which Mesa's EGL needs to find a DRM device.
    if backend.renderer().bind_wl_display(display_handle).is_ok() {
        info!("EGL hardware acceleration enabled");
    }

    // Damage-driven rendering, same trigger as the DRM backend: a ping asks winit for
    // a redraw. Keeping both backends on this path means the scheduling logic is
    // exercised nested, where it can actually be tested.
    let (redraw_ping, redraw_source) = smithay::reexports::calloop::ping::make_ping()?;
    state.redraw_ping = Some(redraw_ping);
    state.winit = Some(backend);
    event_loop
        .handle()
        .insert_source(redraw_source, |_, _, state| {
            if let Some(backend) = state.winit.as_ref() {
                backend.window().request_redraw();
            }
        })?;

    // NOTE: WAYLAND_DISPLAY is set centrally in `main` once the backend is up — it
    // must not be set before `winit::init()`, which needs the *host* display.

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| {
            match event {
                WinitEvent::Resized { size, .. } => {
                    output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        None,
                    );
                }
                WinitEvent::Input(event) => state.process_input_event(event),
                WinitEvent::Redraw => {
                    // The nested backend cannot reprogram a mode, and the only mode it
                    // advertises is the one it already has, so drop any queued request
                    // rather than letting the queue grow without bound.
                    state.pending_mode_changes.clear();

                    // Taken out of the state for the frame: the renderer borrows from
                    // the backend, while the render helpers need the state as a whole,
                    // and those two borrows cannot overlap. Put back below; nothing
                    // returns early in between.
                    let Some(mut backend) = state.winit.take() else {
                        return;
                    };

                    let size = backend.window_size();
                    let damage = Rectangle::from_size(size);

                    {
                        let (renderer, mut framebuffer) = backend.bind().unwrap();

                        // Serve any waiting screen capture while the renderer is here.
                        crate::screencopy::take_pending(state, renderer);
                        crate::image_capture::take_pending(state, renderer);
                        // And snapshot any freshly minimized windows for their icons.
                        state.capture_pending_thumbnails(renderer, &output);

                        // Cursor on top of the desktop.
                        let elements: Vec<OutputElem> =
                            crate::render::output_elements(state, renderer, &output, true);

                        damage_tracker
                            .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
                            .unwrap();
                    }
                    backend.submit(Some(&[damage])).unwrap();
                    state.winit = Some(backend);

                    // A locked frame is now on screen, so the lock can be confirmed.
                    crate::session_lock::after_render(state);

                    let now = state.start_time.elapsed();
                    state.space.elements().for_each(|window| {
                        window.send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                            Some(output.clone())
                        })
                    });
                    // Layer surfaces need frame callbacks too, or they never redraw.
                    let map = smithay::desktop::layer_map_for_output(&output);
                    for layer in map.layers() {
                        layer.send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                            Some(output.clone())
                        });
                    }
                    drop(map);

                    // space.refresh / popups.cleanup / flush_clients happen centrally
                    // in main's event-loop callback, for both backends.

                    // No unconditional request_redraw: the next frame is driven by a
                    // redraw ping when something actually changes.
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                _ => (),
            };
        })?;

    Ok(())
}
