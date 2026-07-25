// SPDX-License-Identifier: GPL-3.0-or-later
//! What a single output composites: the desktop (windows and layer surfaces) plus the
//! pointer cursor on top. Shared by both backends so they render the same thing, and by
//! screencopy so a capture matches what is on screen.

/// The desktop background, drawn where nothing else covers an output.
///
/// Shared so a screenshot shows the same backdrop the screen does. Comes from
/// the generated palette so the compositor and the Avalonia apps agree.
pub use crate::palette::DESKTOP as DESKTOP_BACKGROUND;

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem,
        element::{AsRenderElements, surface::WaylandSurfaceRenderElement},
    },
    desktop::{LayerSurface, layer_map_for_output, space::SpaceRenderElements},
    output::Output,
    render_elements,
    utils::{Rectangle, Scale},
    wayland::shell::wlr_layer::Layer,
};

use crate::{Wlrix, cursor::PointerRenderElement, decoration};

// `E` (the space's element type) stays a free parameter: bounds like
// `E: RenderElement<R>` can then be satisfied at the use site with concrete types,
// which is not possible if it is pinned here.
render_elements! {
    pub OutputElement<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Pointer = PointerRenderElement<R>,
    Surface = WaylandSurfaceRenderElement<R>,
    Solid = smithay::backend::renderer::element::solid::SolidColorRenderElement,
}

/// The element type both backends composite.
pub type OutputElem<R> = OutputElement<R, WaylandSurfaceRenderElement<R>>;

/// Everything to draw for `output`, cursor first so it lands on top.
///
/// `include_cursor` is false for a capture that should not show the pointer.
pub fn output_elements<R>(
    state: &mut Wlrix,
    renderer: &mut R,
    output: &Output,
    include_cursor: bool,
) -> Vec<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let scale = Scale::from(output.current_scale().fractional_scale());
    let mut elements: Vec<OutputElem<R>> = Vec::new();

    // A locked session shows the locker and nothing else. Checked here rather than in
    // the backends so neither can forget it, and before the cursor so a capture of a
    // locked screen cannot pick up desktop content either.
    if state.lock.is_locked() {
        return crate::session_lock::lock_elements(state, renderer, output);
    }

    if include_cursor {
        let time = state.start_time.elapsed();
        let pointer = state.pointer_location();
        let geometry = state
            .space
            .output_geometry(output)
            .unwrap_or_else(|| Rectangle::from_size((0, 0).into()));
        elements.extend(
            state
                .pointer_renderer
                .render_for_output(
                    renderer,
                    &state.cursor_status,
                    geometry,
                    pointer,
                    scale,
                    time,
                )
                .into_iter()
                .map(OutputElement::Pointer),
        );
    }

    // The desktop, composited by hand rather than through `space_render_elements` so each
    // window's server-side frame can be drawn in that window's own z-slot — a titlebar must
    // occlude the windows below it and be occluded by the windows above it. Order matches
    // `space_render_elements`: overlay/top layer surfaces, then windows front-to-back (each
    // its client surface plus its 4Dwm frame), then bottom/background layer surfaces.
    let output_geo = state
        .space
        .output_geometry(output)
        .unwrap_or_else(|| Rectangle::from_size((0, 0).into()));
    let viewport = decoration::Viewport {
        origin: output_geo.loc,
        scale: output.current_scale().fractional_scale(),
    };

    let layer_map = layer_map_for_output(output);
    let (lower, upper): (Vec<&LayerSurface>, Vec<&LayerSurface>) = layer_map
        .layers()
        .rev()
        .partition(|surface| matches!(surface.layer(), Layer::Background | Layer::Bottom));

    let layer_elements = |renderer: &mut R, surface: &LayerSurface| {
        layer_map
            .layer_geometry(surface)
            .map(|geo| {
                surface.render_elements::<OutputElem<R>>(
                    renderer,
                    geo.loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                )
            })
            .unwrap_or_default()
    };

    for surface in upper {
        elements.extend(layer_elements(renderer, surface));
    }

    let focused = state.focused_window();
    for window in state.space.elements().rev() {
        let Some(geometry) = state.space.element_geometry(window) else {
            continue;
        };
        if !output_geo.overlaps(geometry) {
            continue;
        }

        // The client surface, positioned output-local like `space_render_elements` does.
        // The surface origin is the geometry origin less the window's geometry inset (zero for
        // our server-side windows, but nonzero for any client that keeps a CSD margin).
        let surface_origin = geometry.loc - window.geometry().loc;
        let render_loc = (surface_origin - output_geo.loc).to_physical_precise_round(scale);
        elements.extend(window.render_elements::<OutputElem<R>>(renderer, render_loc, scale, 1.0));

        // The 4Dwm frame wrapping this window's client rectangle.
        if let Some(style) = crate::frame::frame_style(window) {
            let active = focused.as_ref() == Some(window);
            let maximized = crate::desks::window_state(window).borrow().maximized;
            let pressed = state
                .decoration_pressed
                .as_ref()
                .filter(|(w, _)| w == window)
                .map(|(_, part)| *part);
            elements.extend(
                decoration::decoration_elements(
                    geometry, style, active, maximized, pressed, viewport,
                )
                .into_iter()
                .map(OutputElement::Solid),
            );
        }
    }

    for surface in lower {
        elements.extend(layer_elements(renderer, surface));
    }

    elements
}
