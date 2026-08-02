// SPDX-License-Identifier: GPL-3.0-or-later
//! 4dwm/Motif-style server-side window decorations: geometry, palette,
//! hit-testing, and solid-color render elements.
//!
//! Adapted from an earlier wlRIX compositor. Some helpers here (menu panels,
//! minimized-icon tiles, title-text geometry) are for stages not yet wired up, hence the
//! module-level `dead_code` allowance.
#![allow(dead_code, clippy::type_complexity)]
//!
//! The look is matched against `reference/window_decoration.png` (an IRIX 4dwm
//! screenshot): a full beveled frame on all four sides with visually separated
//! corner sections, a titlebar row of beveled pieces (menu button | title |
//! minimize | maximize), a gold palette for the active window and gray for
//! inactive, and *sunken* bevels while a part is pressed.
//!
//! All geometry is in logical coordinates. A window's *client* rectangle is
//! what the application draws into; the frame wraps around it.

use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

/// Maps the global logical desktop into one output's framebuffer: subtract the
/// output's logical origin, then scale to physical pixels. Every render element
/// for a head goes through this so the scene is output-local and rendered at the
/// output's (possibly fractional) scale. A scale-1 output at the origin is the
/// identity.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// The output's top-left in global logical coordinates.
    pub origin: Point<i32, Logical>,
    /// The output's scale factor (>= 1.0).
    pub scale: f64,
}

impl Viewport {
    /// Global logical point -> output-local physical (rounded).
    pub fn point(&self, p: Point<i32, Logical>) -> Point<i32, Physical> {
        (p - self.origin).to_physical_precise_round(self.scale)
    }

    /// Global logical point -> output-local physical (sub-pixel; for the cursor).
    pub fn point_f64(&self, p: Point<f64, Logical>) -> Point<f64, Physical> {
        (p - self.origin.to_f64()).to_physical(self.scale)
    }

    /// Global logical rect -> output-local physical (rounded).
    pub fn rect(&self, r: Rectangle<i32, Logical>) -> Rectangle<i32, Physical> {
        Rectangle::new(
            self.point(r.loc),
            r.size.to_physical_precise_round(self.scale),
        )
    }
}

/// Titlebar row height (and the size of the square titlebar buttons). 30 px
/// matches the original IRIX buttons, so glyph measurements taken from IRIX
/// screenshots apply 1:1 (single-pixel strokes included).
pub const TITLEBAR_HEIGHT: i32 = 30;
/// Full border thickness on each side (including outline and bevels).
pub const BORDER: i32 = 8;
/// Square titlebar button width.
pub const BUTTON_W: i32 = TITLEBAR_HEIGHT;
/// Bevel thickness of frame/titlebar pieces.
const BEVEL: i32 = 2;
/// 1px dark outline around the whole frame.
const OUTLINE: i32 = 1;

const fn c(r: u8, g: u8, b: u8) -> Color32F {
    Color32F::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// The Motif five-tone palette of a frame state (sampled from the reference).
#[derive(Clone, Copy)]
struct Palette {
    face: Color32F,
    light: Color32F,
    dark: Color32F,
    /// Face color of a pressed (armed) piece.
    press: Color32F,
}

/// Active window: gold/khaki.
const ACTIVE: Palette = Palette {
    face: c(165, 159, 128),
    light: c(217, 214, 201),
    dark: c(99, 95, 77),
    press: c(132, 127, 102),
};

/// Inactive window: gray.
const INACTIVE: Palette = Palette {
    face: c(128, 128, 128),
    light: c(201, 201, 201),
    dark: c(77, 77, 77),
    press: c(102, 102, 102),
};

const OUTLINE_COLOR: Color32F = c(0, 0, 0);

/// The wireframe shown while a window is moved or resized non-opaquely. From the generated
/// palette, so a theme can restyle it along with everything else.
pub const DRAG_OUTLINE: Color32F = crate::palette::DRAG_OUTLINE;

pub const TITLE_TEXT_ACTIVE: Color32F = c(10, 10, 10);
pub const TITLE_TEXT_INACTIVE: Color32F = c(38, 38, 38);

// Menus: Motif gray panel, black text, gold selection.
pub const MENU_BG: Color32F = c(168, 168, 168);
pub const MENU_HILITE: Color32F = c(165, 159, 128);
pub const MENU_TEXT: Color32F = c(10, 10, 10);
/// Menu panel face and bevel, from the generated palette so menus match the apps' chrome.
pub const MENU_FACE: Color32F = crate::palette::FACE;
pub const MENU_LIGHT: Color32F = crate::palette::TOP_SHADOW;
pub const MENU_DARK: Color32F = crate::palette::BOTTOM_SHADOW;
/// Label colors: black, and a dimmed gray for an item that cannot be chosen.
pub const MENU_LABEL: Color32F = crate::palette::FOREGROUND;
pub const MENU_LABEL_DISABLED: Color32F = crate::palette::BOTTOM_SHADOW;
/// Bevel thickness of the menu panel and of a highlighted row.
pub const MENU_BEVEL: i32 = 2;

// Minimized-icon tiles (matched against `reference/minimize_icons.png`).
pub const ICON_TILE_W: i32 = 104;
pub const ICON_IMAGE_H: i32 = 66;
pub const ICON_LABEL_H: i32 = 20;
pub const ICON_TILE_H: i32 = ICON_IMAGE_H + ICON_LABEL_H;
pub const ICON_GAP: i32 = 8;
pub const ICON_MARGIN: i32 = 8;
/// Backdrop behind the window thumbnail. Also the clear color when capturing a thumbnail, so
/// the letterboxing around an off-aspect window matches the tile.
pub const ICON_IMAGE_FACE: Color32F = c(70, 74, 82);
/// Label bar face.
const ICON_LABEL_FACE: Color32F = c(168, 168, 168);
pub const ICON_LABEL_TEXT: Color32F = c(10, 10, 10);

/// Which decoration pieces a window gets (from per-app rules; default all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStyle {
    pub titlebar: bool,
    pub border: bool,
    pub menu_btn: bool,
    pub min_btn: bool,
    pub max_btn: bool,
}

/// Which resize edge(s) a border position maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResizeEdge {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

/// A part of the window frame the pointer can interact with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePart {
    Titlebar,
    MenuButton,
    MinimizeButton,
    MaximizeButton,
    Resize(ResizeEdge),
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(Point::new(x, y), Size::from((w.max(0), h.max(0))))
}

/// Frame insets around the client rectangle: (left, top, right, bottom).
pub fn insets(style: FrameStyle) -> (i32, i32, i32, i32) {
    let side = if style.border { BORDER } else { 0 };
    let top = side + if style.titlebar { TITLEBAR_HEIGHT } else { 0 };
    (side, top, side, side)
}

/// The outer frame rectangle (decorations included) for a client area.
pub fn frame_rect(client: Rectangle<i32, Logical>, style: FrameStyle) -> Rectangle<i32, Logical> {
    let (l, t, r, b) = insets(style);
    rect(
        client.loc.x - l,
        client.loc.y - t,
        client.size.w + l + r,
        client.size.h + t + b,
    )
}

/// The titlebar row (between the side borders, above the client).
fn titlebar_rect(client: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    rect(
        client.loc.x,
        client.loc.y - TITLEBAR_HEIGHT,
        client.size.w,
        TITLEBAR_HEIGHT,
    )
}

/// The window-menu button (leftmost titlebar button), or `None` when the style
/// suppresses it (`NO_MENU_BUTTON` — a bare titlebar).
fn menu_button(
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
) -> Option<Rectangle<i32, Logical>> {
    if !style.menu_btn {
        return None;
    }
    let tb = titlebar_rect(client);
    Some(rect(tb.loc.x, tb.loc.y, BUTTON_W.min(tb.size.w), tb.size.h))
}

/// The x where the title content starts: right of the menu button if present,
/// else the titlebar's left edge.
fn title_start_x(client: Rectangle<i32, Logical>, style: FrameStyle) -> i32 {
    match menu_button(client, style) {
        Some(mb) => mb.loc.x + mb.size.w,
        None => titlebar_rect(client).loc.x,
    }
}

/// The right-side buttons in order from the right edge: maximize outermost,
/// then minimize. Returns (minimize, maximize) rects where enabled.
fn right_buttons(
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
) -> (
    Option<Rectangle<i32, Logical>>,
    Option<Rectangle<i32, Logical>>,
) {
    let tb = titlebar_rect(client);
    let mut x = tb.loc.x + tb.size.w;
    let maximize = if style.max_btn {
        x -= BUTTON_W;
        Some(rect(x, tb.loc.y, BUTTON_W, tb.size.h))
    } else {
        None
    };
    let minimize = if style.min_btn {
        x -= BUTTON_W;
        Some(rect(x, tb.loc.y, BUTTON_W, tb.size.h))
    } else {
        None
    };
    (minimize, maximize)
}

/// The titlebar area available for the title text.
pub fn title_text_area(
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
) -> Rectangle<i32, Logical> {
    let tb = titlebar_rect(client);
    let pad = 6;
    let x = title_start_x(client, style) + pad;
    let (minimize, maximize) = right_buttons(client, style);
    let right_edge = minimize
        .or(maximize)
        .map(|r| r.loc.x)
        .unwrap_or(tb.loc.x + tb.size.w)
        - pad;
    rect(x, tb.loc.y, right_edge - x, tb.size.h)
}

/// Which frame part (if any) a pointer position falls on. Returns `None` for
/// the client area or anywhere outside the frame.
pub fn hit_test(
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
    point: Point<f64, Logical>,
) -> Option<FramePart> {
    if style.titlebar {
        if menu_button(client, style).is_some_and(|mb| mb.to_f64().contains(point)) {
            return Some(FramePart::MenuButton);
        }
        let (minimize, maximize) = right_buttons(client, style);
        if minimize.is_some_and(|r| r.to_f64().contains(point)) {
            return Some(FramePart::MinimizeButton);
        }
        if maximize.is_some_and(|r| r.to_f64().contains(point)) {
            return Some(FramePart::MaximizeButton);
        }
        if titlebar_rect(client).to_f64().contains(point) {
            return Some(FramePart::Titlebar);
        }
    }

    if !style.border {
        return None;
    }
    let frame = frame_rect(client, style);
    let inner = rect(
        frame.loc.x + BORDER,
        frame.loc.y + BORDER,
        frame.size.w - 2 * BORDER,
        frame.size.h - 2 * BORDER,
    );
    if !frame.to_f64().contains(point) || inner.to_f64().contains(point) {
        return None;
    }

    // Corner arms (matching the visible corner sections): within the button
    // width of a side corner / the titlebar height of a top/bottom corner
    // grabs both axes.
    let arm_w = BORDER + BUTTON_W;
    let arm_h = BORDER
        + if style.titlebar {
            TITLEBAR_HEIGHT
        } else {
            BUTTON_W
        };
    let near_left = point.x < (frame.loc.x + arm_w) as f64;
    let near_right = point.x >= (frame.loc.x + frame.size.w - arm_w) as f64;
    let near_top = point.y < (frame.loc.y + arm_h) as f64;
    let near_bottom = point.y >= (frame.loc.y + frame.size.h - arm_h) as f64;

    let on_left = point.x < (frame.loc.x + BORDER) as f64;
    let on_right = point.x >= (frame.loc.x + frame.size.w - BORDER) as f64;
    let on_top = point.y < (frame.loc.y + BORDER) as f64;
    let on_bottom = point.y >= (frame.loc.y + frame.size.h - BORDER) as f64;

    let edge = ResizeEdge {
        top: on_top || ((on_left || on_right) && near_top),
        bottom: on_bottom || ((on_left || on_right) && near_bottom),
        left: on_left || ((on_top || on_bottom) && near_left),
        right: on_right || ((on_top || on_bottom) && near_right),
    };
    Some(FramePart::Resize(edge))
}

/// Build a single solid-color render element for an arbitrary logical rect,
/// mapped into the head's framebuffer by `vp`.
pub fn solid_quad(
    r: Rectangle<i32, Logical>,
    color: Color32F,
    vp: Viewport,
) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        vp.rect(r),
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

/// Where a window would land, drawn as a wireframe while it is moved or resized non-opaquely.
///
/// The *client* rectangle plus the frame it wears, rather than the outer rectangle already
/// worked out: the outline is drawn from the same [`frame_rect`] the real decoration uses, so
/// the wireframe and the window that replaces it on release cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragOutline {
    pub client: Rectangle<i32, Logical>,
    /// `None` for an undecorated window, which gets a single outline at its own edge.
    pub style: Option<FrameStyle>,
}

/// Thickness of the wireframe's strokes.
///
/// Two, not one: at the scales a modern display runs at, a single logical pixel of red over a
/// busy background is easy to lose track of mid-drag, which defeats the point of it.
const DRAG_OUTLINE_STROKE: i32 = 2;

/// The red wireframe for a pending move or resize.
///
/// Two concentric outlines when the window has a border -- the outer edge of the frame and the
/// inner edge where the border stops -- plus a rule across the bottom of the titlebar, so the
/// titlebar reads as its own closed box the way it does on the real frame. That is how IRIX
/// drew it: the frame's structural lines, without the buttons or the title text. An
/// undecorated window has only the one outline.
pub fn drag_outline_elements(outline: DragOutline, vp: Viewport) -> Vec<SolidColorRenderElement> {
    drag_outline_quads(outline)
        .into_iter()
        .map(|r| solid_quad(r, DRAG_OUTLINE, vp))
        .collect()
}

/// The wireframe's strokes as plain rectangles. Split from the render elements so the geometry
/// -- which has to agree with where the window actually lands -- can be tested on its own.
pub fn drag_outline_quads(outline: DragOutline) -> Vec<Rectangle<i32, Logical>> {
    let mut quads: Vec<(Rectangle<i32, Logical>, Color32F)> = Vec::new();
    let outer = match outline.style {
        Some(style) => frame_rect(outline.client, style),
        None => outline.client,
    };
    stroked_outline(&mut quads, outer, DRAG_OUTLINE, DRAG_OUTLINE_STROKE);

    // The inner edge, where the border stops. Skipped when it would meet the outer one --
    // a window dragged down to nothing should not read as a solid red block.
    if outline.style.is_some_and(|style| style.border) {
        let inner = rect(
            outer.loc.x + BORDER,
            outer.loc.y + BORDER,
            outer.size.w - 2 * BORDER,
            outer.size.h - 2 * BORDER,
        );
        if inner.size.w > 2 * DRAG_OUTLINE_STROKE && inner.size.h > 2 * DRAG_OUTLINE_STROKE {
            stroked_outline(&mut quads, inner, DRAG_OUTLINE, DRAG_OUTLINE_STROKE);
        }
    }

    // The rule under the titlebar. Taken from `titlebar_rect`, the same helper the drawn frame
    // uses, so the wireframe's division sits exactly where the real one will.
    //
    // Drawn *upwards* from the client area, inside the titlebar: the titlebar's other three
    // sides are the inner outline's top and upper flanks, all of which are drawn inward, so
    // this closes the box rather than straddling its edge.
    if outline.style.is_some_and(|style| style.titlebar) {
        let tb = titlebar_rect(outline.client);
        if tb.size.w > 0 && tb.size.h > 2 * DRAG_OUTLINE_STROKE {
            quads.push((
                rect(
                    tb.loc.x,
                    tb.loc.y + tb.size.h - DRAG_OUTLINE_STROKE,
                    tb.size.w,
                    DRAG_OUTLINE_STROKE,
                ),
                DRAG_OUTLINE,
            ));
        }
    }

    quads.into_iter().map(|(r, _)| r).collect()
}

/// Push the four strokes of a hollow rectangle outline, `stroke` px thick.
fn stroked_outline(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    r: Rectangle<i32, Logical>,
    color: Color32F,
    stroke: i32,
) {
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    let s = stroke.min(w / 2).min(h / 2).max(1);
    out.push((rect(x, y, w, s), color));
    out.push((rect(x, y + h - s, w, s), color));
    out.push((rect(x, y + s, s, h - 2 * s), color));
    out.push((rect(x + w - s, y + s, s, h - 2 * s), color));
}

/// Push the four 1px strokes of a hollow rectangle outline in `color`.
fn outline_quads(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    r: Rectangle<i32, Logical>,
    color: Color32F,
) {
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    out.push((rect(x, y, w, 1), color));
    out.push((rect(x, y + h - 1, w, 1), color));
    out.push((rect(x, y + 1, 1, h - 2), color));
    out.push((rect(x + w - 1, y + 1, 1, h - 2), color));
}

/// Push an IRIX button glyph: a 1px black outline of `r` with a solid black
/// drop shadow offset by `shadow` px (measured per glyph off the IRIX
/// originals) — down-right when positive, up-left when negative (IRIX flips
/// the shadow while a toggle is "in", e.g. the maximized state). The shadow
/// treats the object as opaque — only the strips falling *outside* the
/// object's footprint are drawn, so it never shows through the hollow
/// interior.
fn glyph_outline(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    r: Rectangle<i32, Logical>,
    shadow: i32,
) {
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    outline_quads(out, r, OUTLINE_COLOR);
    // Shadow = the offset copy of the footprint minus the footprint itself:
    // two strips on the offset side.
    if shadow > 0 {
        out.push((rect(x + w, y + shadow, shadow, h), OUTLINE_COLOR));
        out.push((rect(x + shadow, y + h, w - shadow, shadow), OUTLINE_COLOR));
    } else if shadow < 0 {
        let m = -shadow;
        out.push((rect(x - m, y - m, m, h), OUTLINE_COLOR));
        out.push((rect(x, y - m, w - m, m), OUTLINE_COLOR));
    }
}

/// Emit the quads for one beveled piece: `face` in the middle, light on
/// top/left and dark on bottom/right (`raised`), or swapped (`!raised`,
/// pressed/sunken). Quads do not overlap.
fn beveled_quads(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    r: Rectangle<i32, Logical>,
    face: Color32F,
    light: Color32F,
    dark: Color32F,
    raised: bool,
    bevel: i32,
) {
    let (tl, br) = if raised { (light, dark) } else { (dark, light) };
    let b = bevel.min(r.size.w / 2).min(r.size.h / 2).max(0);
    if b == 0 {
        out.push((r, face));
        return;
    }
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    // Top strip (full width), left strip (below it, stopping above the bottom
    // strip), bottom strip (full width), right strip — then the inner face.
    out.push((rect(x, y, w, b), tl));
    out.push((rect(x, y + b, b, h - 2 * b), tl));
    out.push((rect(x, y + h - b, w, b), br));
    out.push((rect(x + w - b, y + b, b, h - 2 * b), br));
    out.push((rect(x + b, y + b, w - 2 * b, h - 2 * b), face));
}

/// The beveled panel behind a menu: the palette's face color, raised.
pub fn menu_panel(
    background: Rectangle<i32, Logical>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    let mut quads = Vec::new();
    beveled_quads(
        &mut quads, background, MENU_FACE, MENU_LIGHT, MENU_DARK, true, MENU_BEVEL,
    );
    quads
        .into_iter()
        .map(|(r, c)| solid_quad(r, c, vp))
        .collect()
}

/// The highlight behind the menu item the pointer is over: the gold selection color, raised
/// like a button so the pointed-at row stands proud of the panel.
pub fn menu_item_highlight(
    item: Rectangle<i32, Logical>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    let mut quads = Vec::new();
    beveled_quads(
        &mut quads,
        item,
        MENU_HILITE,
        MENU_LIGHT,
        MENU_DARK,
        true,
        MENU_BEVEL,
    );
    quads
        .into_iter()
        .map(|(r, c)| solid_quad(r, c, vp))
        .collect()
}

/// A menu separator: an etched groove across `row`, dark line over light, Motif-style.
pub fn menu_separator(row: Rectangle<i32, Logical>, vp: Viewport) -> Vec<SolidColorRenderElement> {
    let y = row.loc.y + row.size.h / 2 - 1;
    vec![
        solid_quad(rect(row.loc.x, y, row.size.w, 1), MENU_DARK, vp),
        solid_quad(rect(row.loc.x, y + 1, row.size.w, 1), MENU_LIGHT, vp),
    ]
}

/// The area inside an icon tile where the window thumbnail is drawn.
pub fn icon_image_area(tile: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    rect(
        tile.loc.x + BEVEL,
        tile.loc.y + BEVEL,
        tile.size.w - 2 * BEVEL,
        ICON_IMAGE_H - 2 * BEVEL,
    )
}

/// The logical size of an icon thumbnail: the image area, the same for every tile. A thumbnail
/// is captured at this size (times the output scale) so it fills [`icon_image_area`] exactly.
pub fn icon_thumbnail_size() -> Size<i32, Logical> {
    Size::from((ICON_TILE_W - 2 * BEVEL, ICON_IMAGE_H - 2 * BEVEL))
}

/// The label bar rectangle of an icon tile.
pub fn icon_label_rect(tile: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    rect(
        tile.loc.x,
        tile.loc.y + ICON_IMAGE_H,
        tile.size.w,
        ICON_LABEL_H,
    )
}

/// The solid quads of one 4dwm icon tile: a beveled image frame (its face is
/// the backdrop behind the thumbnail) and a beveled label bar below. The
/// thumbnail and label text are layered on top by the renderer.
pub fn icon_tile_elements(
    tile: Rectangle<i32, Logical>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    let mut quads = Vec::new();
    beveled_quads(
        &mut quads,
        rect(tile.loc.x, tile.loc.y, tile.size.w, ICON_IMAGE_H),
        ICON_IMAGE_FACE,
        INACTIVE.light,
        INACTIVE.dark,
        true,
        BEVEL,
    );
    beveled_quads(
        &mut quads,
        icon_label_rect(tile),
        ICON_LABEL_FACE,
        INACTIVE.light,
        INACTIVE.dark,
        true,
        BEVEL,
    );
    quads
        .into_iter()
        .map(|(r, c)| solid_quad(r, c, vp))
        .collect()
}

/// Build the decoration render elements for one window, in front-to-back order.
/// `active` selects the palette; `pressed` renders that part sunken.
pub fn decoration_elements(
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
    active: bool,
    maximized: bool,
    pressed: Option<FramePart>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    let p = if active { ACTIVE } else { INACTIVE };
    let mut quads: Vec<(Rectangle<i32, Logical>, Color32F)> = Vec::new();

    let is_pressed = |part: FramePart| pressed == Some(part);
    let piece = |quads: &mut Vec<_>, r: Rectangle<i32, Logical>, sunken: bool| {
        let face = if sunken { p.press } else { p.face };
        beveled_quads(quads, r, face, p.light, p.dark, !sunken, BEVEL);
    };
    if style.titlebar {
        // Menu button: IRIX horizontal-bar glyph (absent under NO_MENU_BUTTON).
        // Measured off the original 30x30 IRIX button: bar at (3,12), 22x5.
        let menu = menu_button(client, style);
        if let Some(mb) = menu {
            glyph_outline(&mut quads, rect(mb.loc.x + 3, mb.loc.y + 12, 22, 5), 2);
        }

        let (minimize, maximize) = right_buttons(client, style);
        if let Some(r) = minimize {
            // Minimize: small IRIX box — measured at (13,12), 5x5, 1px shadow.
            glyph_outline(&mut quads, rect(r.loc.x + 13, r.loc.y + 12, 5, 5), 1);
        }
        if let Some(r) = maximize {
            // Maximize: large IRIX box — measured at (5,3), 20x22, 1px shadow.
            // While maximized the box shifts to (6,4) and the shadow flips
            // up-left ("pressed in").
            if maximized {
                glyph_outline(&mut quads, rect(r.loc.x + 6, r.loc.y + 4, 20, 22), -1);
            } else {
                glyph_outline(&mut quads, rect(r.loc.x + 5, r.loc.y + 3, 20, 22), 1);
            }
        }

        // Button + title faces (under the glyphs).
        if let Some(mb) = menu {
            piece(&mut quads, mb, is_pressed(FramePart::MenuButton));
        }
        if let Some(r) = minimize {
            piece(&mut quads, r, is_pressed(FramePart::MinimizeButton));
        }
        if let Some(r) = maximize {
            piece(&mut quads, r, is_pressed(FramePart::MaximizeButton));
        }
        let title = title_bar_piece(client, style);
        piece(&mut quads, title, is_pressed(FramePart::Titlebar));
    }

    if style.border {
        // The border never renders pressed — resizing does not sink it (unlike
        // the titlebar/buttons above).
        let frame = frame_rect(client, style);
        let face = p.face;
        let bx = frame.loc.x + OUTLINE;
        let by = frame.loc.y + OUTLINE;
        let bw = frame.size.w - 2 * OUTLINE;
        let bh = frame.size.h - 2 * OUTLINE;
        let t = BORDER - OUTLINE; // border band thickness inside the outline

        // Corner sections are single L-shaped pieces whose arms end aligned
        // with the menu/maximize button edges (horizontally) and the titlebar
        // bottom (vertically); the bottom corners mirror the same spans.
        let arm_w = (t + BUTTON_W).min(bw / 2);
        let arm_h = (t + if style.titlebar {
            TITLEBAR_HEIGHT
        } else {
            BUTTON_W
        })
        .min(bh / 2);

        for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
            // Bounding box of this corner's L.
            let ox = if left { bx } else { bx + bw - arm_w };
            let oy = if top { by } else { by + bh - arm_h };
            let h_rect = rect(ox, if top { oy } else { oy + arm_h - t }, arm_w, t);
            let v_rect = rect(if left { ox } else { ox + arm_w - t }, oy, t, arm_h);

            // Drawn as two beveled rects (H on top of V), with a face patch
            // hiding H's interior-facing bevel where the vertical arm passes
            // through — that bevel is the stray line across the corner.
            let patch_y = if top {
                h_rect.loc.y + t - BEVEL
            } else {
                h_rect.loc.y
            };
            quads.push((
                rect(v_rect.loc.x + BEVEL, patch_y, t - 2 * BEVEL, BEVEL),
                face,
            ));
            piece(&mut quads, h_rect, false);
            piece(&mut quads, v_rect, false);
        }

        // Edge middles between the corner arms (their own bevels form the
        // aligned corner seams).
        if bw > 2 * arm_w {
            piece(&mut quads, rect(bx + arm_w, by, bw - 2 * arm_w, t), false);
            piece(
                &mut quads,
                rect(bx + arm_w, by + bh - t, bw - 2 * arm_w, t),
                false,
            );
        }
        if bh > 2 * arm_h {
            piece(&mut quads, rect(bx, by + arm_h, t, bh - 2 * arm_h), false);
            piece(
                &mut quads,
                rect(bx + bw - t, by + arm_h, t, bh - 2 * arm_h),
                false,
            );
        }

        // 1px outline around everything (bottom of the stack).
        quads.push((frame, OUTLINE_COLOR));
    }

    quads
        .into_iter()
        .map(|(r, c)| solid_quad(r, c, vp))
        .collect()
}

/// The titlebar piece behind the title text (between menu and right buttons).
fn title_bar_piece(client: Rectangle<i32, Logical>, style: FrameStyle) -> Rectangle<i32, Logical> {
    let tb = titlebar_rect(client);
    let x = title_start_x(client, style);
    let (minimize, maximize) = right_buttons(client, style);
    let right_edge = minimize
        .or(maximize)
        .map(|r| r.loc.x)
        .unwrap_or(tb.loc.x + tb.size.w);
    rect(x, tb.loc.y, right_edge - x, tb.size.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> FrameStyle {
        FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            min_btn: true,
            max_btn: true,
        }
    }

    fn outline(
        client: Rectangle<i32, Logical>,
        style: Option<FrameStyle>,
    ) -> Vec<Rectangle<i32, Logical>> {
        drag_outline_quads(DragOutline { client, style })
    }

    /// The bounding box of a set of quads.
    fn bounds(quads: &[Rectangle<i32, Logical>]) -> Rectangle<i32, Logical> {
        let x0 = quads.iter().map(|r| r.loc.x).min().unwrap();
        let y0 = quads.iter().map(|r| r.loc.y).min().unwrap();
        let x1 = quads.iter().map(|r| r.loc.x + r.size.w).max().unwrap();
        let y1 = quads.iter().map(|r| r.loc.y + r.size.h).max().unwrap();
        rect(x0, y0, x1 - x0, y1 - y0)
    }

    /// The point of the wireframe: it stands exactly where the window will be. If the outline
    /// and `frame_rect` ever disagree, the window lands somewhere other than it was promised.
    #[test]
    fn the_outline_traces_the_frame_the_window_would_wear() {
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        assert_eq!(
            bounds(&outline(client, Some(style()))),
            frame_rect(client, style())
        );
    }

    #[test]
    fn a_bordered_window_gets_the_borders_two_edges() {
        // Two concentric outlines, four strokes each -- the outer edge of the frame and the
        // inner edge where the border stops, which is how IRIX drew it -- plus the rule under
        // the titlebar.
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        let quads = outline(client, Some(style()));
        assert_eq!(quads.len(), 9);

        let outer = frame_rect(client, style());
        let inner = rect(
            outer.loc.x + BORDER,
            outer.loc.y + BORDER,
            outer.size.w - 2 * BORDER,
            outer.size.h - 2 * BORDER,
        );
        let on_inner: Vec<_> = quads.iter().filter(|r| inner.contains_rect(**r)).collect();
        assert_eq!(
            on_inner.len(),
            5,
            "the inner outline's four strokes, plus the titlebar rule"
        );
    }

    /// The line the drawn frame has under its titlebar, which the wireframe needs too: without
    /// it the outline is a plain box and says nothing about which way up the window is.
    #[test]
    fn the_titlebar_is_closed_off_by_a_rule_at_its_bottom() {
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        let tb = titlebar_rect(client);
        // Flush with the top of the client area, and drawn upward into the titlebar.
        let rule = outline(client, Some(style()))
            .into_iter()
            .find(|r| {
                r.loc.x == tb.loc.x && r.size.w == tb.size.w && r.loc.y + r.size.h == client.loc.y
            })
            .expect("a full-width rule at the bottom of the titlebar");

        assert!(
            tb.contains_rect(rule),
            "{rule:?} should sit inside the titlebar {tb:?}"
        );
        // A stroke of its own, not the titlebar's top edge under another name -- those share
        // this width and this left edge, which is what makes the box read as a titlebar.
        assert!(
            rule.loc.y > tb.loc.y,
            "the rule is the titlebar's *bottom*: {rule:?} in {tb:?}"
        );
    }

    #[test]
    fn a_window_with_no_titlebar_gets_no_rule() {
        // Nothing to divide, and a line across a bare box would be a lie about the frame.
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        let bare = FrameStyle {
            titlebar: false,
            ..style()
        };
        assert_eq!(outline(client, Some(bare)).len(), 8);
    }

    #[test]
    fn an_undecorated_window_gets_one_outline_at_its_own_edge() {
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        let quads = outline(client, None);
        assert_eq!(quads.len(), 4);
        assert_eq!(bounds(&quads), client);
    }

    #[test]
    fn the_strokes_stay_inside_the_frame() {
        // Drawn inward, not straddling the edge: an outline that overhung would promise a
        // window a couple of pixels bigger than the one that arrives.
        let client = Rectangle::new(Point::new(300, 200), Size::from((400, 300)));
        let outer = frame_rect(client, style());
        for quad in outline(client, Some(style())) {
            assert!(
                outer.contains_rect(quad),
                "{quad:?} escapes the frame {outer:?}"
            );
        }
    }

    #[test]
    fn a_window_shrunk_to_nothing_does_not_become_a_red_block() {
        // A resize can clamp to a 1x1 client. The inner outline would then be inverted or
        // would meet the outer one, and the wireframe would read as a filled rectangle.
        let tiny = Rectangle::new(Point::new(300, 200), Size::from((1, 1)));
        let quads = outline(tiny, Some(style()));
        assert_eq!(
            quads.len(),
            5,
            "the inner outline should have been dropped, leaving the outer four and the rule"
        );
        for quad in &quads {
            assert!(quad.size.w > 0 && quad.size.h > 0, "{quad:?} is empty");
        }
    }
}
