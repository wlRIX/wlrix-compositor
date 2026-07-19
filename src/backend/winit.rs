// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use std::time::Duration;

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{
            Color32F, ImportDma, ImportEgl, damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    desktop::{Window, space::space_render_elements},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Scale, Transform},
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState},
};
use tracing::{info, warn};

use crate::{CalloopData, Wlrix, render::OutputElement};

/// The winit backend, stored in [`crate::CalloopData`].
pub type WinitBackend = winit::WinitGraphicsBackend<GlesRenderer>;

/// What this backend composites: desktop plus cursor.
type OutputElem = OutputElement<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Nested-window background.
const CLEAR_COLOR: Color32F = Color32F::new(0.1, 0.1, 0.1, 1.0);

pub fn init_winit(
    event_loop: &mut EventLoop<CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_handle = &mut data.display_handle;
    let state = &mut data.state;

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
    data.winit = Some(backend);
    event_loop
        .handle()
        .insert_source(redraw_source, |_, _, data| {
            if let Some(backend) = data.winit.as_ref() {
                backend.window().request_redraw();
            }
        })?;

    // NOTE: WAYLAND_DISPLAY is set centrally in `main` once the backend is up — it
    // must not be set before `winit::init()`, which needs the *host* display.

    event_loop
        .handle()
        .insert_source(winit, move |event, _, data| {
            let CalloopData {
                state,
                winit: winit_backend,
                ..
            } = data;
            let Some(backend) = winit_backend.as_mut() else {
                return;
            };

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
                    let size = backend.window_size();
                    let damage = Rectangle::from_size(size);

                    {
                        let (renderer, mut framebuffer) = backend.bind().unwrap();

                        // Cursor first so it composites above the desktop.
                        let scale = Scale::from(output.current_scale().fractional_scale());
                        let time = state.start_time.elapsed();
                        let hotspot =
                            state
                                .pointer_renderer
                                .hotspot(&state.cursor_status, scale, time);
                        let cursor_location = state
                            .seat
                            .get_pointer()
                            .map(|pointer| pointer.current_location())
                            .unwrap_or_default()
                            .to_physical(scale)
                            .to_i32_round::<i32>()
                            - hotspot;
                        let mut elements: Vec<OutputElem> = state
                            .pointer_renderer
                            .render(renderer, &state.cursor_status, cursor_location, scale, time)
                            .into_iter()
                            .map(OutputElem::Pointer)
                            .collect();
                        elements.extend(
                            space_render_elements::<_, Window, _>(
                                renderer,
                                [&state.space],
                                &output,
                                1.0,
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .map(OutputElem::Space),
                        );

                        damage_tracker
                            .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
                            .unwrap();
                    }
                    backend.submit(Some(&[damage])).unwrap();

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
