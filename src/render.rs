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
        element::{
            AsRenderElements, Kind, memory::MemoryRenderBufferRenderElement,
            surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::{LayerSurface, Window, layer_map_for_output, space::SpaceRenderElements},
    output::Output,
    render_elements,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::shell::wlr_layer::Layer,
};

use crate::{Wlrix, cursor::PointerRenderElement, decoration, text::TextRenderer};

// `E` (the space's element type) stays a free parameter: bounds like
// `E: RenderElement<R>` can then be satisfied at the use site with concrete types,
// which is not possible if it is pinned here.
render_elements! {
    pub OutputElement<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Pointer = PointerRenderElement<R>,
    Surface = WaylandSurfaceRenderElement<R>,
    Solid = smithay::backend::renderer::element::solid::SolidColorRenderElement,
    Memory = MemoryRenderBufferRenderElement<R>,
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

    // Snapshot the visible windows first (front-to-back) so the render loop can borrow the
    // text renderer mutably without also holding the space borrowed.
    let focused = state.focused_window();
    let draws: Vec<WindowDraw> = state
        .space
        .elements()
        .rev()
        .filter_map(|window| {
            let geometry = state.space.element_geometry(window)?;
            if !output_geo.overlaps(geometry) {
                return None;
            }
            // The surface origin is the geometry origin less the window's geometry inset (zero
            // for our server-side windows, but nonzero for any client that keeps a CSD margin).
            let surface_origin = geometry.loc - window.geometry().loc;
            let render_loc = (surface_origin - output_geo.loc).to_physical_precise_round(scale);
            let style = crate::frame::frame_style(window);
            let active = focused.as_ref() == Some(window);
            let title = if style.is_some_and(|s| s.titlebar) {
                crate::frame::window_title(window)
            } else {
                String::new()
            };
            Some(WindowDraw {
                window: window.clone(),
                geometry,
                render_loc,
                style,
                active,
                maximized: crate::desks::window_state(window).borrow().maximized,
                pressed: state
                    .decoration_pressed
                    .as_ref()
                    .filter(|(w, _)| w == window)
                    .map(|(_, part)| *part),
                title,
            })
        })
        .collect();

    for draw in &draws {
        // Client surface.
        elements.extend(draw.window.render_elements::<OutputElem<R>>(
            renderer,
            draw.render_loc,
            scale,
            1.0,
        ));

        let Some(style) = draw.style else {
            continue;
        };
        // The window title, left-aligned in the titlebar (cosmic-text lays RTL runs the other
        // way, so RTL titles sit to the right on their own). Pushed *before* the frame quads so
        // it renders in front of the opaque titlebar background rather than behind it.
        if style.titlebar
            && let Some(element) = title_element(
                &mut state.text_renderer,
                renderer,
                &draw.title,
                draw.geometry,
                style,
                draw.active,
                viewport,
            )
        {
            elements.push(element);
        }
        // The 4Dwm frame wrapping this window's client rectangle.
        elements.extend(
            decoration::decoration_elements(
                draw.geometry,
                style,
                draw.active,
                draw.maximized,
                draw.pressed,
                viewport,
            )
            .into_iter()
            .map(OutputElement::Solid),
        );
    }

    for surface in lower {
        elements.extend(layer_elements(renderer, surface));
    }

    elements
}

/// One window's render inputs, snapshotted from the space.
struct WindowDraw {
    window: Window,
    geometry: Rectangle<i32, Logical>,
    render_loc: Point<i32, Physical>,
    style: Option<decoration::FrameStyle>,
    active: bool,
    maximized: bool,
    pressed: Option<decoration::FramePart>,
    title: String,
}

/// The title-text render element for a titlebar, left-aligned and vertically centred, cropped
/// to the space between the menu and the right-hand buttons.
fn title_element<R>(
    text: &mut TextRenderer,
    renderer: &mut R,
    title: &str,
    client: Rectangle<i32, Logical>,
    style: decoration::FrameStyle,
    active: bool,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let color = if active {
        decoration::TITLE_TEXT_ACTIVE
    } else {
        decoration::TITLE_TEXT_INACTIVE
    };
    // Rasterize at physical pixels so the text stays crisp at fractional scale.
    let rasterized = text.rasterize(title, crate::text::TITLE_PX * viewport.scale as f32, color)?;

    let area = viewport.rect(decoration::title_text_area(client, style));
    if area.size.w <= 0 {
        return None;
    }
    // Left edge of the area, vertically centred; crop the buffer to the area width.
    let y = area.loc.y + (area.size.h - rasterized.height) / 2;
    let location = Point::<i32, Physical>::from((area.loc.x, y));
    let visible = rasterized.width.min(area.size.w);
    // `src`/`size` are logical; the buffer is scale-1, so its logical extent equals its pixels.
    let src = Rectangle::new(
        Point::from((0.0, 0.0)),
        Size::from((visible as f64, rasterized.height as f64)),
    );
    let dst = Size::from((
        (visible as f64 / viewport.scale).round() as i32,
        (rasterized.height as f64 / viewport.scale).round() as i32,
    ));

    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location.to_f64(),
        &rasterized.buffer,
        None,
        Some(src),
        Some(dst),
        Kind::Unspecified,
    )
    .ok()
    .map(OutputElement::Memory)
}
