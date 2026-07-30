// SPDX-License-Identifier: GPL-3.0-or-later
//! wlr-layer-shell: desktop components that anchor to an output rather than behaving
//! like ordinary toplevels — the wlRIX toolchest and desks, plus backgrounds.
//!
//! Layer surfaces live in a per-output [`layer_map_for_output`] map, not in the `Space`.
//! Smithay's `space_render_elements` already draws them in the right z-order (background
//! and bottom below windows, top and overlay above), so no render changes are needed —
//! but they do need the same frame-callback and dmabuf-feedback treatment as windows.

use smithay::{
    desktop::{LayerSurface, Space, Window, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::{wl_output, wl_surface::WlSurface},
    wayland::{
        compositor::with_states,
        shell::wlr_layer::{
            Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
            WlrLayerShellState,
        },
    },
};
use tracing::warn;

use crate::Wlrix;

impl WlrLayerShellHandler for Wlrix {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Honor the client's requested output, else fall back to the first one.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());

        let Some(output) = output else {
            warn!(%namespace, "no output available to map layer surface onto");
            return;
        };

        tracing::info!(%namespace, output = output.name(), "layer surface mapped");
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            warn!(?err, "failed to map layer surface");
        }
        drop(map);
        self.request_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            // Bind first so the borrow of `map` ends before we move it into the tuple.
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (map, layer))
        });

        if let Some((mut map, layer)) = found {
            map.unmap_layer(&layer);
            drop(map);
            self.request_redraw();
        }
    }
}

/// Handle a commit on a layer surface: (re)arrange the output's layer map and send the
/// initial configure. Called from the compositor commit handler.
pub fn handle_commit(space: &Space<Window>, surface: &WlSurface) {
    let Some(output) = space.outputs().find(|output| {
        layer_map_for_output(output)
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some()
    }) else {
        return;
    };

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .map(|data| data.lock().unwrap().initial_configure_sent)
            .unwrap_or(false)
    });

    let mut map = layer_map_for_output(output);
    // Arrange before the initial configure, so we respect any size the client asked for.
    map.arrange();

    if !initial_configure_sent
        && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
    {
        layer.layer_surface().send_configure();
    }
}
