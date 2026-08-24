// SPDX-License-Identifier: GPL-3.0-or-later
//! What a single output composites: the desktop (windows and layer surfaces) plus the
//! pointer cursor on top. Shared by both backends so they render the same thing, and by
//! screencopy so a capture matches what is on screen.

/// The desktop background, drawn where nothing else covers an output.
///
/// Shared so a screenshot shows the same backdrop the screen does. Comes from
/// the generated palette so the compositor and the Avalonia apps agree.
/// The gray under everything, which is what an output with no wallpaper shows.
///
/// Also the clear color a screen capture composites against, so a captured frame matches
/// what is on the glass.
pub fn desktop_background(
    palette: &wlrix_ui::palette::Palette,
) -> smithay::backend::renderer::Color32F {
    smithay::backend::renderer::Color32F::from(palette.desktop.to_f32_array())
}

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem,
        element::{
            AsRenderElements, Kind, memory::MemoryRenderBufferRenderElement,
            surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::{
        LayerSurface, PopupManager, Window, layer_map_for_output, space::SpaceRenderElements,
    },
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

/// Whether a layer-shell surface on `layer` is drawn in front of the windows.
///
/// Normally overlay and top both are -- that is what those layers are for.
///
/// A fullscreen window is the exception, and takes the top layer down with it: a game filling
/// the screen with a panel still drawn across it is not fullscreen. The overlay layer stays in
/// front regardless, which is the line wlroots draws too and the reason the two exist as
/// separate layers at all -- a screen locker or an on-screen keyboard has to outrank the game.
///
/// Dropping the top layer behind *every* window rather than just the fullscreen one costs
/// nothing: the fullscreen window covers the output, so there is nothing to see under it either
/// way.
fn draws_in_front(layer: Layer, fullscreen_here: bool) -> bool {
    match layer {
        Layer::Overlay => true,
        Layer::Top => !fullscreen_here,
        Layer::Bottom | Layer::Background => false,
    }
}

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
        palette: state.palette,
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
        let rows: Vec<MenuRow> = menu
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| MenuRow {
                rect: menu.row(index),
                label: entry.label,
                accel: entry.accel.as_deref(),
                enabled: entry.enabled,
                hovered: menu.hovered == Some(index),
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
        for row in &rows {
            if row.label.is_empty() {
                continue;
            }
            // The accelerator is greyed with its item: a key that does nothing while the item
            // is unavailable should not be advertised as if it did.
            let color = if row.enabled {
                decoration::menu_label(state.palette)
            } else {
                decoration::menu_label_disabled(state.palette)
            };
            if let Some(element) = menu_label_element(
                &mut state.text_renderer,
                renderer,
                row.label,
                row.rect,
                color,
                viewport,
            ) {
                elements.push(element);
            }
            if let Some(accel) = row.accel
                && let Some(element) = menu_accel_element(
                    &mut state.text_renderer,
                    renderer,
                    accel,
                    row.rect,
                    color,
                    viewport,
                )
            {
                elements.push(element);
            }
        }
        for row in &rows {
            if row.hovered {
                elements.extend(
                    decoration::menu_item_highlight(row.rect, viewport)
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

    // Whether this output has a window covering the whole of it. Worked out *before* the layer
    // map is locked below -- that guard is non-reentrant, and the note on `placement::work_area`
    // is about exactly this stretch of code.
    let fullscreen_here = state.space.elements().any(|window| {
        crate::desks::window_state(window).borrow().fullscreen
            && state.space.outputs_for_element(window).contains(output)
    });

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
        .partition(|surface| draws_in_front(surface.layer(), fullscreen_here));

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
            // Culled on everything the window puts on screen, not on its client rectangle
            // alone: `Window::render_elements` draws its popups too, and the 4Dwm frame is
            // drawn around it below. Either can reach onto an output the client rectangle
            // does not touch, and a window dropped here is dropped whole -- which is how a
            // menu that crossed onto the next monitor came out as the sliver of itself that
            // happened to fall inside its own window's output.
            if !output_geo.overlaps(drawn_extent(window, geometry)) {
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

        let dragged = state.dragged_icon();
        let mut icons: Vec<IconDraw> = state
            .minimized_icons()
            .into_iter()
            .map(|(window, slot)| {
                let is_dragged = dragged.as_ref().is_some_and(|(w, _)| *w == window);
                let tile = match &dragged {
                    Some((_, tile)) if is_dragged => *tile,
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

/// Everything `window` causes to be drawn, in global logical coordinates: its client rectangle,
/// the 4Dwm frame around it, and any popups it has open.
///
/// Popups need looking up rather than measuring: an xdg popup is a surface of its own hung off
/// the toplevel, not part of its surface tree, so neither the window's geometry nor its bounding
/// box knows anything about one. Smithay draws them from the window's geometry origin plus the
/// offset the popup tree records, which is what this reconstructs.
fn drawn_extent(window: &Window, geometry: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let mut extent = geometry;
    if let Some(style) = crate::frame::frame_style(window) {
        let (left, top, right, bottom) = decoration::insets(style);
        extent.loc -= Point::from((left, top));
        extent.size += Size::from((left + right, top + bottom));
    }
    // `toplevel`, not `wl_surface`: only a Wayland window can have xdg popups. An X11 window's
    // menus are override-redirect windows of their own, in the space, culled on their own merits.
    if let Some(toplevel) = window.toplevel() {
        for (popup, offset) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
            extent = extent.merge(Rectangle::new(geometry.loc + offset, popup.geometry().size));
        }
    }
    extent
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
    let color = decoration::title_text(viewport.palette, active);
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

/// Text height for a minimized-icon label, in logical pixels. The line box comes out at 17px
/// tall, which is exactly the room [`decoration::icon_label_rect`] leaves below the groove.
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

/// One window-menu row, flattened out of the menu before drawing.
///
/// The menu is borrowed from `state` while `state.text_renderer` has to be borrowed mutably to
/// rasterize, so the rows are collected first. A named struct rather than the tuple this was:
/// five fields is past where positional destructuring reads as anything.
struct MenuRow<'a> {
    rect: Rectangle<i32, Logical>,
    label: &'a str,
    accel: Option<&'a str>,
    enabled: bool,
    hovered: bool,
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

/// A window-menu item's accelerator: the key combination bound to it, right-aligned at the same
/// inset from the row's right edge that the label keeps from its left, and vertically centred.
///
/// Right-aligned rather than tabbed to a column, which is what Motif did and what makes the
/// keys read as a column of their own without the menu having to agree on where that column
/// starts. The panel was measured wide enough for the longest of them (see
/// `menu::measure_width`), so the crop below is a backstop rather than the usual case -- it
/// matters on a fractional scale, where the logical measurement is a rounding off.
fn menu_accel_element<R>(
    text: &mut TextRenderer,
    renderer: &mut R,
    accel: &str,
    row: Rectangle<i32, Logical>,
    color: smithay::backend::renderer::Color32F,
    viewport: decoration::Viewport,
) -> Option<OutputElem<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let rasterized = text.rasterize(accel, crate::menu::LABEL_PX * viewport.scale as f32, color)?;
    let area = viewport.rect(row);
    let inset = (crate::menu::LABEL_INSET as f64 * viewport.scale).round() as i32;
    // Where the text would start if it were laid out from the right-hand inset. Clamped to the
    // left inset so an accelerator too long for its row is cropped at its tail by `place_text`
    // rather than sliding out past the label.
    let right = area.loc.x + area.size.w - inset;
    let x = (right - rasterized.width).max(area.loc.x + inset);
    if right <= x {
        return None;
    }
    let y = area.loc.y + (area.size.h - rasterized.height) / 2;
    place_text(renderer, &rasterized, x, y, right - x, viewport)
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
        decoration::menu_label(viewport.palette),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary desktop: panels and notification daemons draw over the windows, wallpaper
    /// and desktop icons draw under them.
    #[test]
    fn panels_draw_over_windows_and_wallpaper_draws_under() {
        for layer in [Layer::Overlay, Layer::Top] {
            assert!(draws_in_front(layer, false), "{layer:?}");
        }
        for layer in [Layer::Bottom, Layer::Background] {
            assert!(!draws_in_front(layer, false), "{layer:?}");
        }
    }

    /// The whole point of the flag: a game that filled the screen must not have the panel still
    /// sitting on top of it.
    #[test]
    fn a_fullscreen_window_covers_the_top_layer() {
        assert!(!draws_in_front(Layer::Top, true));
    }

    /// ...but not the overlay layer. A screen locker and an on-screen keyboard live there
    /// precisely so that a fullscreen client cannot get in front of them, which is a safety
    /// property rather than a cosmetic one.
    #[test]
    fn even_a_fullscreen_window_stays_under_the_overlay_layer() {
        assert!(draws_in_front(Layer::Overlay, true));
    }

    /// The lower layers were already behind the windows and have nothing to change.
    #[test]
    fn fullscreen_does_not_disturb_the_layers_already_underneath() {
        for layer in [Layer::Bottom, Layer::Background] {
            assert_eq!(
                draws_in_front(layer, true),
                draws_in_front(layer, false),
                "{layer:?}"
            );
        }
    }
}
