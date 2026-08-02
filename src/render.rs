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

    // The wireframe of a pending non-opaque move or resize, above everything but the cursor.
    // It stands for a window that is not where it is drawn, so nothing should occlude it --
    // and it must be over the window it will replace, not behind it.
    if let Some(outline) = state.drag_outline {
        elements.extend(
            decoration::drag_outline_elements(outline, viewport)
                .into_iter()
                .map(OutputElement::Solid),
        );
    }

    // The window menu sits above the desktop and below the cursor. Drawn before the layer map is
    // locked below, and from its own already-clamped origin, so it needs no work-area lookup.
    if let Some(menu) = state.window_menu.as_ref()
        && output_geo.overlaps(menu.panel())
    {
        let rows: Vec<(Rectangle<i32, Logical>, &'static str, bool, bool)> = menu
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    menu.row(index),
                    entry.label,
                    entry.enabled,
                    menu.hovered == Some(index),
                )
            })
            .collect();
        let panel = menu.panel();
        let separators: Vec<Rectangle<i32, Logical>> = menu
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_separator())
            .map(|(index, _)| menu.row(index))
            .collect();

        // Front to back: labels, then the selection highlight, then separators, then the panel.
        for (row, label, enabled, _) in &rows {
            if label.is_empty() {
                continue;
            }
            let color = if *enabled {
                decoration::MENU_LABEL
            } else {
                decoration::MENU_LABEL_DISABLED
            };
            if let Some(element) = menu_label_element(
                &mut state.text_renderer,
                renderer,
                label,
                *row,
                color,
                viewport,
            ) {
                elements.push(element);
            }
        }
        for (row, _, _, hovered) in &rows {
            if *hovered {
                elements.extend(
                    decoration::menu_item_highlight(*row, viewport)
                        .into_iter()
                        .map(OutputElement::Solid),
                );
            }
        }
        for row in separators {
            elements.extend(
                decoration::menu_separator(row, viewport)
                    .into_iter()
                    .map(OutputElement::Solid),
            );
        }
        elements.extend(
            decoration::menu_panel(panel, viewport)
                .into_iter()
                .map(OutputElement::Solid),
        );
    }

    let layer_map = layer_map_for_output(output);
    // `LayerMap::layers()` hands surfaces back in **map order**, not layer order -- smithay
    // never sorts them -- so ordering has to be done here. Without it a wallpaper on the
    // background layer, started after a desktop-icons client on the bottom layer, is drawn
    // *over* the icons; restarting `swaybg` was enough to make the desktop vanish.
    //
    // This list is front-to-back, so the higher layer sorts first. `sort_by_key` is stable,
    // which keeps map order within one layer -- and `.rev()` has already put the newest
    // first there, which is the right way round for surfaces sharing a layer.
    let mut ordered: Vec<&LayerSurface> = layer_map.layers().rev().collect();
    ordered.sort_by_key(|surface| match surface.layer() {
        Layer::Overlay => 0,
        Layer::Top => 1,
        Layer::Bottom => 2,
        Layer::Background => 3,
    });
    let (upper, lower): (Vec<&LayerSurface>, Vec<&LayerSurface>) = ordered
        .into_iter()
        .partition(|surface| matches!(surface.layer(), Layer::Overlay | Layer::Top));

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

    // Minimized-window icons sit on the primary output's desktop, in front of the background but
    // behind the windows (pushed after the window loop, so further back in the front-to-back
    // list). Collected first so the text renderer can be borrowed without also holding the space.
    if state.space.outputs().next() == Some(output) {
        // Build the grid from the `layer_map` already locked at the top of this function.
        // Going through `state.icon_grid()` -> `work_area()` would call `layer_map_for_output`
        // again here, re-locking this output's layer-map mutex on the same thread and
        // deadlocking the whole compositor (black screen, frozen input).
        let mut area = layer_map.non_exclusive_zone();
        area.loc += output_geo.loc;
        let grid = crate::minimized::Grid::new(area);

        let dragged = state
            .icon_drag
            .as_ref()
            .filter(|drag| drag.is_drag())
            .map(|drag| (drag.window.clone(), drag.tile_origin()));
        let mut icons: Vec<IconDraw> = state
            .minimized_icons()
            .into_iter()
            .map(|(window, slot)| {
                let is_dragged = dragged.as_ref().is_some_and(|(w, _)| *w == window);
                let tile = match &dragged {
                    Some((_, origin)) if is_dragged => Rectangle::new(
                        *origin,
                        (decoration::ICON_TILE_W, decoration::ICON_TILE_H).into(),
                    ),
                    _ => grid.slot_rect(slot),
                };
                IconDraw {
                    title: crate::frame::window_title(&window),
                    tile,
                    is_dragged,
                    thumbnail: crate::desks::window_state(&window)
                        .borrow()
                        .thumbnail
                        .clone(),
                }
            })
            .collect();
        // The dragged tile draws in front of the rest (earliest in the front-to-back list).
        icons.sort_by_key(|draw| !draw.is_dragged);
        for draw in &icons {
            // Front to back: label, then the thumbnail, then the tile quads (whose backdrop
            // shows through where a window hasn't been snapshotted yet).
            if let Some(element) = icon_label_element(
                &mut state.text_renderer,
                renderer,
                &draw.title,
                draw.tile,
                viewport,
            ) {
                elements.push(element);
            }
            if let Some(thumbnail) = &draw.thumbnail
                && let Some(element) = thumbnail_element(
                    renderer,
                    thumbnail,
                    decoration::icon_image_area(draw.tile),
                    viewport,
                )
            {
                elements.push(element);
            }
            elements.extend(
                decoration::icon_tile_elements(draw.tile, viewport)
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

/// One minimized-window icon's render inputs.
struct IconDraw {
    title: String,
    tile: Rectangle<i32, Logical>,
    is_dragged: bool,
    thumbnail: Option<smithay::backend::renderer::element::memory::MemoryRenderBuffer>,
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
    // Vertically centred always; horizontally as the frame says. A title too wide to center
    // falls back to the left edge and is clipped from the right, as a left-aligned one is --
    // shifting it left of the run would clip the beginning of the name instead of the end.
    let y = area.loc.y + (area.size.h - rasterized.height) / 2;
    let x =
        decoration::title_text_start(area.loc.x, area.size.w, rasterized.width, style.title_align);
    let remaining = area.size.w - (x - area.loc.x);
    place_text(renderer, &rasterized, x, y, remaining, viewport)
}

/// Text height for a minimized-icon label, in logical pixels (the label bar is 20px).
const ICON_LABEL_PX: f32 = 13.0;

/// The thumbnail render element for a minimized icon: the captured snapshot drawn to fill the
/// tile's image `area`. The buffer was captured at the area's physical size, so drawing it at
/// the area's logical size scales it back 1:1 on this output.
fn thumbnail_element<R>(
    renderer: &mut R,
    thumbnail: &smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    area: Rectangle<i32, Logical>,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let physical = viewport.rect(area);
    if physical.size.w <= 0 || physical.size.h <= 0 {
        return None;
    }
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        physical.loc.to_f64(),
        thumbnail,
        None,
        None,
        Some(area.size),
        Kind::Unspecified,
    )
    .ok()
    .map(OutputElement::Memory)
}

/// A window-menu item's label: left-aligned at the menu's text inset, vertically centred in the
/// row and cropped to it.
fn menu_label_element<R>(
    text: &mut TextRenderer,
    renderer: &mut R,
    label: &str,
    row: Rectangle<i32, Logical>,
    color: smithay::backend::renderer::Color32F,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let rasterized = text.rasterize(label, crate::menu::LABEL_PX * viewport.scale as f32, color)?;
    let area = viewport.rect(row);
    let inset = (crate::menu::LABEL_INSET as f64 * viewport.scale).round() as i32;
    if area.size.w <= inset {
        return None;
    }
    let x = area.loc.x + inset;
    let y = area.loc.y + (area.size.h - rasterized.height) / 2;
    place_text(renderer, &rasterized, x, y, area.size.w - inset, viewport)
}

/// The centred label under a minimized-window icon, cropped to the tile width.
fn icon_label_element<R>(
    text: &mut TextRenderer,
    renderer: &mut R,
    title: &str,
    tile: Rectangle<i32, Logical>,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let rasterized = text.rasterize(
        title,
        ICON_LABEL_PX * viewport.scale as f32,
        decoration::ICON_LABEL_TEXT,
    )?;
    let area = viewport.rect(decoration::icon_label_rect(tile));
    if area.size.w <= 0 {
        return None;
    }
    // Centred horizontally and vertically in the label bar.
    let visible = rasterized.width.min(area.size.w);
    let x = area.loc.x + (area.size.w - visible) / 2;
    let y = area.loc.y + (area.size.h - rasterized.height) / 2;
    place_text(renderer, &rasterized, x, y, area.size.w, viewport)
}

/// Turn a rasterized line into a render element: crop it to `max_w` physical pixels and place its
/// top-left at (`x`, `y`) physical. `src`/`size` are logical; the buffer is scale-1, so its
/// logical extent equals its pixels, and `size` divides back out the output scale so the physical
/// result matches the buffer 1:1.
fn place_text<R>(
    renderer: &mut R,
    rasterized: &crate::text::Rasterized,
    x: i32,
    y: i32,
    max_w: i32,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let visible = rasterized.width.min(max_w);
    if visible <= 0 {
        return None;
    }
    let location = Point::<i32, Physical>::from((x, y));
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
