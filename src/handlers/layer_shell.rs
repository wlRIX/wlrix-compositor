// SPDX-License-Identifier: GPL-3.0-or-later
//! wlr-layer-shell: desktop components that anchor to an output rather than behaving
//! like ordinary toplevels — the wlRIX toolchest and desks, plus backgrounds.
//!
//! Layer surfaces live in a per-output [`layer_map_for_output`] map, not in the [`Space`](smithay::desktop::Space).
//! Smithay's `space_render_elements` already draws them in the right z-order (background
//! and bottom below windows, top and overlay above), so no render changes are needed —
//! but they do need the same frame-callback and dmabuf-feedback treatment as windows.

use smithay::{
    desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output},
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

/// Which output a new layer surface goes on: the one it asked for, else the first one there is.
///
/// `requested` is the client's choice, already resolved — `None` when it left the decision to
/// the compositor. It is only honored if it is still in `live`, and that check is the whole
/// point of this function. [`Output::from_resource`] keeps resolving a `wl_output` long after
/// its global is gone, because the client's proxy stays valid until it releases it, so a
/// monitor unplugged a moment ago still answers. Mapping onto it would put the surface in the
/// layer map of an output that is no longer in the [`Space`](smithay::desktop::Space), and [`handle_commit`] only ever
/// looks at outputs that are: no configure would be sent, and the surface would hang unmapped
/// for the rest of its life with nothing to tell the client why.
///
/// That is not a rare race. A DisplayPort monitor entering power save drops its link, so the
/// outputs really are destroyed and re-advertised every time the session blanks — and a client
/// rebuilding its surface on the way past lands in exactly this window.
///
/// The flag is whether an output the client actually asked for had to be passed over, which is
/// worth a line in the log. Falling back because it asked for nothing is routine, and silent.
fn place_layer(requested: Option<Output>, live: &[Output]) -> (Option<Output>, bool) {
    let passed_over = requested
        .as_ref()
        .is_some_and(|output| !live.contains(output));
    let chosen = requested
        .filter(|_| !passed_over)
        .or_else(|| live.first().cloned());
    (chosen, passed_over)
}

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
        let live: Vec<Output> = self.space.outputs().cloned().collect();
        let requested = wl_output.as_ref().and_then(Output::from_resource);
        let requested_name = requested.as_ref().map(Output::name);
        let (output, passed_over) = place_layer(requested, &live);

        if passed_over {
            warn!(
                %namespace,
                requested = requested_name,
                "layer surface asked for an output that has gone; using another",
            );
        }
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
        // Keyboard interactivity is *not* checked here, and that is not an oversight. This
        // runs on `get_layer_surface`, before the client's first commit, so the cached state
        // still holds the default -- `KeyboardInteractivity::None` -- whatever the client is
        // about to ask for. `handle_commit` is where it becomes known; see
        // `take_exclusive_focus`.
        self.request_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // Scoped so the layer map's guard -- and the borrow of `self.space` it came from --
        // are both released before focus is touched below. `layer_map_for_output` is not
        // reentrant, and `focus_topmost` needs `self` mutably.
        let unmapped = {
            let found = self.space.outputs().find_map(|output| {
                let map = layer_map_for_output(output);
                // Bind first so the borrow of `map` ends before we move it into the tuple.
                let layer = map
                    .layers()
                    .find(|&layer| layer.layer_surface() == &surface)
                    .cloned();
                layer.map(|layer| (map, layer))
            });
            found.map(|(mut map, layer)| {
                map.unmap_layer(&layer);
                layer.wl_surface().clone()
            })
        };

        let Some(unmapped) = unmapped else {
            return;
        };

        // A layer surface can hold keyboard focus (see `focus::focus_layer_surface`), and this
        // one is gone. Left alone, focus would point at a dead surface and typing would go
        // nowhere -- the same reason `focus_topmost` exists for windows.
        let was_focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| focus == unmapped);
        if was_focused {
            crate::focus::focus_topmost(self);
        }
        self.request_redraw();
    }
}

/// Tell every layer surface on `output` that it is gone, and take them out of its map.
///
/// Called when an output is removed, before it leaves the
/// [`Space`](smithay::desktop::Space). A layer surface is
/// anchored to one output and cannot follow it: the moment that output is out of the space,
/// [`handle_commit`] stops arranging it, so it will never be configured or drawn again.
/// Nothing in the protocol lets a client work that out on its own, and `closed` is the event
/// that means "this one is finished, build another" — so sending it is the only honest answer.
///
/// This is not a corner case. A DisplayPort monitor entering power save drops its link, so an
/// ordinary idle blank destroys every output and re-advertises them on wake.
pub fn close_layers_on(state: &mut Wlrix, output: &Output) {
    let closed: Vec<WlSurface> = {
        let mut map = layer_map_for_output(output);
        // Collected up front: unmapping while iterating would borrow the map twice.
        let layers: Vec<LayerSurface> = map.layers().cloned().collect();
        for layer in &layers {
            layer.layer_surface().send_close();
            map.unmap_layer(layer);
        }
        layers
            .iter()
            .map(|layer| layer.wl_surface().clone())
            .collect()
    };

    // Same reasoning as `layer_destroyed`, which this path does not go through: a layer surface
    // can hold the keyboard, and focus left on one that has just been closed sends typing
    // nowhere. The layer map's guard is dropped above, because `focus_topmost` needs `state`.
    let was_focused = state
        .seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus())
        .is_some_and(|focus| closed.contains(&focus));
    if was_focused {
        crate::focus::focus_topmost(state);
    }
}

/// Handle a commit on a layer surface: (re)arrange the output's layer map, send the initial
/// configure, and hand over the keyboard if this surface has asked for it outright. Called
/// from the compositor commit handler for every surface; returns immediately for anything that
/// is not a mapped layer surface.
pub fn handle_commit(state: &mut Wlrix, surface: &WlSurface) {
    // Scoped, so the map guard -- and the borrow of the space it came from -- are both
    // released before focus is touched below. `layer_map_for_output` is not reentrant and
    // `take_exclusive_focus` takes guards of its own.
    let is_layer_surface = {
        let Some(output) = state.space.outputs().find(|output| {
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
        true
    };

    if is_layer_surface {
        take_exclusive_focus(state);
    }
}

/// Give the keyboard to a layer surface that asked for it outright.
///
/// `KeyboardInteractivity::Exclusive` is not click-to-focus. `on-demand` is -- that is what the
/// desktop icons ask for, and what makes clicking the desktop mean "I am talking to the desktop
/// now" -- but `exclusive` is a client saying every key is its until it goes away, which is what
/// a screen locker, a full-screen menu and `wlrix-screenshot`'s region overlay all need. The two
/// were treated alike until the screenshot overlay needed the difference: it covers the whole
/// screen, so Escape before the first click went to a window nobody could see and the overlay
/// looked frozen.
///
/// Called on **commit** rather than on creation, because that is when the client's requested
/// interactivity is applied -- and because a client may change it later, which this then picks
/// up for free.
///
/// Only ever *takes* focus. Giving it back is `layer_destroyed`'s job, which is the case that
/// actually happens; a surface that downgrades itself from exclusive while mapped is not
/// something any client does, and guessing where focus should go instead would risk taking it
/// from an on-demand layer surface that legitimately has it.
fn take_exclusive_focus(state: &mut Wlrix) {
    let Some(exclusive) = state.exclusive_layer() else {
        return;
    };
    let already = state
        .seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus())
        .is_some_and(|focus| focus == exclusive);
    if already {
        return;
    }
    crate::focus::focus_layer_surface(state, &exclusive);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};

    fn output(name: &str) -> Output {
        Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "wlRIX".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        )
    }

    #[test]
    fn an_output_the_client_asked_for_is_honored() {
        let (dp4, dp5) = (output("DP-4"), output("DP-5"));
        let live = vec![dp4.clone(), dp5.clone()];

        let (chosen, passed_over) = place_layer(Some(dp5.clone()), &live);

        assert_eq!(chosen, Some(dp5));
        assert!(!passed_over, "DP-5 is live, so nothing was passed over");
    }

    /// The bug this function exists for: a monitor that has gone still resolves, so without the
    /// liveness check the surface lands in a layer map nothing arranges and never gets a
    /// configure. The client is left holding a surface that will never be drawable, and has no
    /// way to find that out -- which is a wedged desktop, not a missing frame.
    #[test]
    fn an_output_that_has_gone_is_passed_over_for_one_that_has_not() {
        let unplugged = output("DP-4");
        let live = vec![output("DP-5")];

        let (chosen, passed_over) = place_layer(Some(unplugged), &live);

        assert_eq!(chosen, Some(live[0].clone()));
        assert!(
            passed_over,
            "the log needs to say the choice was overridden"
        );
    }

    /// A client that names no output is not making a mistake, so this must not warn.
    #[test]
    fn asking_for_no_output_takes_the_first_one_quietly() {
        let live = vec![output("DP-4"), output("DP-5")];

        let (chosen, passed_over) = place_layer(None, &live);

        assert_eq!(chosen, Some(live[0].clone()));
        assert!(!passed_over);
    }

    /// Every monitor asleep at once is the ordinary state of a blanked session, and the caller
    /// has to be handed `None` rather than the departed output.
    #[test]
    fn with_every_output_gone_there_is_nowhere_to_map() {
        let (chosen, passed_over) = place_layer(Some(output("DP-4")), &[]);

        assert_eq!(chosen, None);
        assert!(passed_over);
    }
}
