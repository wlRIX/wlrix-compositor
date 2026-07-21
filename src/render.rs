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
    backend::renderer::{ImportAll, ImportMem, element::surface::WaylandSurfaceRenderElement},
    desktop::{Window, space::SpaceRenderElements, space::space_render_elements},
    output::Output,
    render_elements,
    utils::{Rectangle, Scale},
};

use crate::{Wlrix, cursor::PointerRenderElement};

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

    elements.extend(
        space_render_elements::<_, Window, _>(renderer, [&state.space], output, 1.0)
            .unwrap_or_default()
            .into_iter()
            .map(OutputElement::Space),
    );

    elements
}
