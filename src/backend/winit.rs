// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Smithay's `smallvil` example (MIT-licensed). See the NOTICE file.
use std::time::Duration;

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{
            ImportDma, ImportEgl, damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState},
};
use tracing::{info, warn};

use crate::{CalloopData, Wlrix};

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

    // NOTE: WAYLAND_DISPLAY is set centrally in `main` once the backend is up — it
    // must not be set before `winit::init()`, which needs the *host* display.

    event_loop
        .handle()
        .insert_source(winit, move |event, _, data| {
            let state = &mut data.state;

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
                        smithay::desktop::space::render_output::<
                            _,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.space],
                            &[],
                            &mut damage_tracker,
                            [0.1, 0.1, 0.1, 1.0],
                        )
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

                    // Ask for redraw to schedule new frame.
                    backend.window().request_redraw();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                _ => (),
            };
        })?;

    Ok(())
}
