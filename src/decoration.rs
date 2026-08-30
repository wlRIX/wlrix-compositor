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

use wlrix_ui::Rgb;
use wlrix_ui::bevel::{self, Run, Shade};
use wlrix_ui::canvas::Rect as UiRect;
use wlrix_ui::palette::Palette;

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
    /// The color scheme the chrome is drawn in.
    ///
    /// It rides here because a viewport is already threaded into every function that emits
    /// chrome, and it is built once per output per frame. `&'static`, so this stays `Copy`.
    pub palette: &'static Palette,
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

/// A palette color as smithay's renderer wants it.
///
/// The whole interop surface between `wlrix-ui` and smithay: `Color32F` is a `[f32; 4]`
/// newtype, so nothing more is needed and the shared crate does not have to know smithay
/// exists -- which matters, because smithay is pinned to a git rev here and a dependency on
/// it there would make every bump a two-repo lockstep.
#[inline]
fn c32(color: Rgb) -> Color32F {
    Color32F::from(color.to_f32_array())
}

/// The three tones one frame state is drawn from, plus its pressed face.
///
/// These were eight literals sampled from the reference screenshots, with `#a59f80` typed
/// here *and* in the generated palette under a different name. They are roles now, derived
/// from `wmActiveBackground` and `wmBackground` by the `frame*` factors, and they reproduce
/// every one of the eight sampled values exactly.
fn frame_palette(palette: &Palette, active: bool) -> Palette4 {
    if active {
        Palette4 {
            face: c32(palette.title_active),
            light: c32(palette.title_active_top_shadow),
            dark: c32(palette.title_active_bottom_shadow),
            press: c32(palette.title_active_armed),
        }
    } else {
        Palette4 {
            face: c32(palette.title_inactive),
            light: c32(palette.title_inactive_top_shadow),
            dark: c32(palette.title_inactive_bottom_shadow),
            press: c32(palette.title_inactive_armed),
        }
    }
}

/// The Motif four-tone palette of a frame state.
#[derive(Clone, Copy)]
struct Palette4 {
    face: Color32F,
    light: Color32F,
    dark: Color32F,
    /// Face color of a pressed (armed) piece.
    press: Color32F,
}

/// The wireframe shown while a window is moved or resized non-opaquely.
pub fn drag_outline(palette: &Palette) -> Color32F {
    c32(palette.drag_outline)
}

/// The title's own color.
///
/// Two sampled near-blacks before -- `#0a0a0a` and `#262626` -- which the palette had all
/// along as `wmActiveForeground` and `wmForeground`. Binding them means a scheme can say
/// something with them: Classic makes both black, as IRIX did, while Gotham dims the inactive
/// title to `#a1a1a1`.
pub fn title_text(palette: &Palette, active: bool) -> Color32F {
    c32(if active {
        palette.title_active_text
    } else {
        palette.title_inactive_text
    })
}

// Menus: the Motif panel face and bevel, so menus match the applications' chrome, and the
// same gold the active titlebar takes for the pointed-at row -- `wlrix-desktop`'s right-click
// menu binds that one role too, so the two menus cannot drift apart.
//
// `MENU_BG` and `MENU_TEXT` used to live here as two more sampled literals. Neither had a
// caller: the panel is drawn in the face and the labels in the foreground.

/// The menu panel's face.
pub fn menu_face(palette: &Palette) -> Color32F {
    c32(palette.face)
}

pub fn menu_light(palette: &Palette) -> Color32F {
    c32(palette.face_top_shadow)
}

pub fn menu_dark(palette: &Palette) -> Color32F {
    c32(palette.face_bottom_shadow)
}

/// A menu label, and the dimmed gray for an item that cannot be chosen -- which is how Motif
/// grays a label out.
pub fn menu_label(palette: &Palette) -> Color32F {
    c32(palette.foreground)
}

pub fn menu_label_disabled(palette: &Palette) -> Color32F {
    c32(palette.face_bottom_shadow)
}

/// Backdrop behind the window thumbnail in a minimized-window tile. Also the clear color when
/// capturing a thumbnail, so the letterboxing around an off-aspect window matches the tile.
pub fn icon_well(palette: &Palette) -> Color32F {
    c32(palette.icon_well)
}

/// Bevel thickness of the menu panel and of a highlighted row.
pub const MENU_BEVEL: i32 = 2;

// Minimized-icon tiles (matched against `reference/minimize_icons.png`). The tile is one raised
// Motif panel with the window preview inset into its face, a groove across it, and the title
// below; the measurements are the IRIX originals, in logical pixels at scale 1.0.
pub const ICON_TILE_W: i32 = 97;
pub const ICON_TILE_H: i32 = 99;
/// Where the window preview starts, in from the tile's top-left corner. The same on the right,
/// so the preview sits centered in the panel's width. The last [`BEVEL`] of it is the sunken
/// edge of the well the preview sits in -- see [`icon_preview_well`].
const ICON_PREVIEW_INSET: i32 = 6;
const ICON_PREVIEW_W: i32 = 85;
const ICON_PREVIEW_H: i32 = 67;
/// The top of the groove between the preview and the title, from the tile's top. It is below the
/// preview rather than against it -- the few pixels of face left between the two are what makes
/// the groove read as a divider rather than as the preview's own border.
const ICON_SEPARATOR_Y: i32 = ICON_PREVIEW_INSET + ICON_PREVIEW_H + 5;
/// The groove is two rows: a dark line over a light one, Motif-style.
const ICON_SEPARATOR_H: i32 = 2;
/// Tiles butt against each other, so a grid of them reads as one block of icons the way IRIX
/// lays them out. The space around an icon is [`ICON_PREVIEW_INSET`], inside the tile itself.
pub const ICON_MARGIN: i32 = 10;

/// Which decoration pieces a window gets (from per-app rules and what the window itself says
/// it can do; see [`crate::frame::capabilities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStyle {
    pub titlebar: bool,
    pub border: bool,
    pub menu_btn: bool,
    pub min_btn: bool,
    pub max_btn: bool,
    /// Which axes the window may be resized in. A frame that cannot be resized at all keeps
    /// its border -- it is still the window's edge, and still moves it on a middle-drag -- but
    /// loses the corner sections, which are the resize grips, and the resize cursors with them.
    pub resizable: Resizable,
    /// Where the title sits in the run [`title_text_area`] leaves it.
    pub title_align: TitleAlign,
}

/// Where a window's title sits along its titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleAlign {
    /// Against the left of the run, just past the menu button. Every ordinary window: the title
    /// starts where the buttons stop, so a row of them reads as one line.
    Left,
    /// Centered in the run. The toolchest, whose titlebar has no buttons for a title to line up
    /// beside -- left-aligned it would sit against an edge with nothing to relate to.
    Centered,
}

/// The axes a window may be resized in.
///
/// Per axis rather than a single flag because a window may fix one and not the other: a fixed
/// *width* is common (a settings panel, a palette), and its top and bottom edges are still
/// worth grabbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resizable {
    pub horizontal: bool,
    pub vertical: bool,
}

impl Resizable {
    pub const BOTH: Self = Self {
        horizontal: true,
        vertical: true,
    };
    pub const NONE: Self = Self {
        horizontal: false,
        vertical: false,
    };

    /// Whether the window can be resized at all.
    pub fn any(self) -> bool {
        self.horizontal || self.vertical
    }
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
    /// Border that resizes nothing, because the window has fixed that axis. Still part of the
    /// frame -- it occludes what is under it and a middle-drag still moves the window -- but a
    /// left press on it does nothing and it keeps the plain arrow.
    Border,
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

/// Where a title of `text_width` starts inside a run `width` wide beginning at `x`.
///
/// In whatever units the caller is working in: the renderer rasterizes at physical pixels and
/// passes those, so this stays unit-agnostic rather than pretending to be logical.
pub fn title_text_start(x: i32, width: i32, text_width: i32, align: TitleAlign) -> i32 {
    match align {
        TitleAlign::Left => x,
        // A title too wide to center starts at the left edge and is clipped from the right,
        // exactly as a left-aligned one is. Shifting it further left would clip the *start* of
        // the name, which is the part that says which window this is.
        TitleAlign::Centered => x + (width - text_width).max(0) / 2,
    }
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

    // Masked by what the window will actually allow: a fixed width leaves the left and right
    // edges inert, and a corner grab on a fixed-height window becomes a plain horizontal one.
    // Filtered here rather than at the call sites so the cursor, the press and the grab all
    // read the same answer.
    let edge = ResizeEdge {
        top: (on_top || ((on_left || on_right) && near_top)) && style.resizable.vertical,
        bottom: (on_bottom || ((on_left || on_right) && near_bottom)) && style.resizable.vertical,
        left: (on_left || ((on_top || on_bottom) && near_left)) && style.resizable.horizontal,
        right: (on_right || ((on_top || on_bottom) && near_right)) && style.resizable.horizontal,
    };
    if edge == ResizeEdge::default() {
        return Some(FramePart::Border);
    }
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
        .map(|r| solid_quad(r, drag_outline(vp.palette), vp))
        .collect()
}

/// The wireframe's strokes as plain rectangles. Split from the render elements so the geometry
/// -- which has to agree with where the window actually lands -- can be tested on its own.
pub fn drag_outline_quads(outline: DragOutline) -> Vec<Rectangle<i32, Logical>> {
    // The color is discarded on the way out -- this returns geometry, which is the point of
    // it being split from `drag_outline_elements`. The helpers below want one anyway.
    const GEOMETRY_ONLY: Color32F = Color32F::TRANSPARENT;
    let mut quads: Vec<(Rectangle<i32, Logical>, Color32F)> = Vec::new();
    let outer = match outline.style {
        Some(style) => frame_rect(outline.client, style),
        None => outline.client,
    };
    stroked_outline(&mut quads, outer, GEOMETRY_ONLY, DRAG_OUTLINE_STROKE);

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
            stroked_outline(&mut quads, inner, GEOMETRY_ONLY, DRAG_OUTLINE_STROKE);
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
                GEOMETRY_ONLY,
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
    outline: Color32F,
    r: Rectangle<i32, Logical>,
    shadow: i32,
) {
    let (x, y, w, h) = (r.loc.x, r.loc.y, r.size.w, r.size.h);
    outline_quads(out, r, outline);
    // Shadow = the offset copy of the footprint minus the footprint itself:
    // two strips on the offset side.
    if shadow > 0 {
        out.push((rect(x + w, y + shadow, shadow, h), outline));
        out.push((rect(x + shadow, y + h, w - shadow, shadow), outline));
    } else if shadow < 0 {
        let m = -shadow;
        out.push((rect(x - m, y - m, m, h), outline));
        out.push((rect(x, y - m, w - m, m), outline));
    }
}

/// A shared-crate rect from a logical one, and back.
///
/// `wlrix-ui` emits plain rectangles on purpose: taking smithay's `Rectangle<i32, Logical>`
/// would mean taking smithay, and the greeter and the desktop have no business linking it.
fn to_ui(r: Rectangle<i32, Logical>) -> UiRect {
    UiRect::new(r.loc.x, r.loc.y, r.size.w, r.size.h)
}

fn from_ui(r: UiRect) -> Rectangle<i32, Logical> {
    rect(r.x, r.y, r.w, r.h)
}

/// Emit a beveled *ring* of thickness `t` inside `outer`: raised on the outer edge, sunken on
/// the inner one, with the band's face between and nothing crossing the corners.
///
/// Not four [`beveled_quads`] bands. A band bevels all four of its own edges, so the top band's
/// lower shadow -- the line that reads as the seam under the titlebar -- would carry on past
/// the titlebar and across both top corners, and the bottom band would do the same. A ring has
/// only two edges to shade, so the corners stay plain face and it reads as one continuous
/// piece, which is how IRIX drew a fixed-size window's border.
///
/// The geometry is [`wlrix_ui::bevel::ring_quads`]; this only resolves the three shades and
/// reverses the run. `wlrix-ui` emits in painter's order -- later covers earlier, which is
/// what a client filling pixels wants -- and a render-element list is the other way round,
/// topmost first.
fn beveled_ring(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    outer: Rectangle<i32, Logical>,
    t: i32,
    face: Color32F,
    light: Color32F,
    dark: Color32F,
    bevel: i32,
) {
    let mut quads = Vec::new();
    bevel::ring_quads(&mut quads, to_ui(outer), t, bevel);
    out.extend(quads.into_iter().rev().map(|(r, shade)| {
        (
            from_ui(r),
            match shade {
                Shade::Face => face,
                Shade::Light => light,
                Shade::Dark => dark,
            },
        )
    }));
}

/// Emit the quads for one beveled piece: `face` in the middle, light on
/// top/left and dark on bottom/right (`raised`), or swapped (`!raised`,
/// pressed/sunken). Quads do not overlap.
///
/// Geometry from [`wlrix_ui::bevel::quads`]. No reversal here, unlike [`beveled_ring`]:
/// nothing overlaps, so the order carries no meaning.
fn beveled_quads(
    out: &mut Vec<(Rectangle<i32, Logical>, Color32F)>,
    r: Rectangle<i32, Logical>,
    shades: Shades,
    raised: bool,
    bevel: i32,
    run: Run,
) {
    let Shades { face, light, dark } = shades;
    let mut quads = Vec::new();
    bevel::quads(&mut quads, to_ui(r), bevel, raised, run);
    out.extend(quads.into_iter().map(|(r, shade)| {
        (
            from_ui(r),
            match shade {
                Shade::Face => face,
                Shade::Light => light,
                Shade::Dark => dark,
            },
        )
    }));
}

/// The three tones a beveled piece is drawn from. Always travel together, so they travel as one.
#[derive(Debug, Clone, Copy)]
struct Shades {
    face: Color32F,
    light: Color32F,
    dark: Color32F,
}

/// The beveled panel behind a menu: the palette's face color, raised.
pub fn menu_panel(
    background: Rectangle<i32, Logical>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    let mut quads = Vec::new();
    beveled_quads(
        &mut quads,
        background,
        Shades {
            face: menu_face(vp.palette),
            light: menu_light(vp.palette),
            dark: menu_dark(vp.palette),
        },
        true,
        MENU_BEVEL,
        Run::Vertical,
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
        Shades {
            face: c32(vp.palette.title_active),
            light: menu_light(vp.palette),
            dark: menu_dark(vp.palette),
        },
        true,
        MENU_BEVEL,
        Run::Vertical,
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
        solid_quad(rect(row.loc.x, y, row.size.w, 1), menu_dark(vp.palette), vp),
        solid_quad(
            rect(row.loc.x, y + 1, row.size.w, 1),
            menu_light(vp.palette),
            vp,
        ),
    ]
}

/// The area inside an icon tile where the window thumbnail is drawn.
pub fn icon_image_area(tile: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    rect(
        tile.loc.x + ICON_PREVIEW_INSET,
        tile.loc.y + ICON_PREVIEW_INSET,
        ICON_PREVIEW_W,
        ICON_PREVIEW_H,
    )
}

/// The sunken well the preview sits in: [`icon_image_area`] grown by a bevel on every side, so
/// its shadowed edge lands just outside the thumbnail without taking any room from it.
fn icon_preview_well(tile: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let image = icon_image_area(tile);
    rect(
        image.loc.x - BEVEL,
        image.loc.y - BEVEL,
        image.size.w + 2 * BEVEL,
        image.size.h + 2 * BEVEL,
    )
}

/// The logical size of an icon thumbnail: the preview area, the same for every tile. A thumbnail
/// is captured at this size (times the output scale) so it fills [`icon_image_area`] exactly.
pub fn icon_thumbnail_size() -> Size<i32, Logical> {
    Size::from((ICON_PREVIEW_W, ICON_PREVIEW_H))
}

/// Where an icon's title is drawn: the panel face below the groove, in as far as the bevel.
pub fn icon_label_rect(tile: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let top = tile.loc.y + ICON_SEPARATOR_Y + ICON_SEPARATOR_H;
    rect(
        tile.loc.x + BEVEL,
        top,
        tile.size.w - 2 * BEVEL,
        tile.loc.y + tile.size.h - BEVEL - top,
    )
}

/// The solid quads of one 4Dwm icon tile, front to back: a raised panel, the sunken well the
/// thumbnail is drawn into, and the groove dividing that from the title. The thumbnail and the
/// title text are layered on top by the renderer.
fn icon_tile_quads(
    palette: &Palette,
    tile: Rectangle<i32, Logical>,
) -> Vec<(Rectangle<i32, Logical>, Color32F)> {
    // A minimized window's tile is drawn in the *inactive* frame's tones, which is what
    // 4DWmSpec asks for: `*icon*background: WMBackground`.
    let shades = frame_palette(palette, false);
    // The well and the groove both sit over the panel's face, so they go in ahead of it. Neither
    // piece's own quads overlap each other, so their order within a piece is free.
    let mut quads = vec![
        (
            rect(
                tile.loc.x + BEVEL,
                tile.loc.y + ICON_SEPARATOR_Y,
                tile.size.w - 2 * BEVEL,
                1,
            ),
            shades.dark,
        ),
        (
            rect(
                tile.loc.x + BEVEL,
                tile.loc.y + ICON_SEPARATOR_Y + 1,
                tile.size.w - 2 * BEVEL,
                1,
            ),
            shades.light,
        ),
    ];
    // The preview's well: sunken, and its face is the backdrop the thumbnail is drawn over --
    // which is what shows through where a window has not been snapshotted yet, and what
    // letterboxes an off-aspect one.
    beveled_quads(
        &mut quads,
        icon_preview_well(tile),
        Shades {
            face: icon_well(palette),
            light: shades.light,
            dark: shades.dark,
        },
        false,
        BEVEL,
        Run::Vertical,
    );
    beveled_quads(
        &mut quads,
        tile,
        Shades {
            face: c32(palette.icon_tile_face),
            light: shades.light,
            dark: shades.dark,
        },
        true,
        BEVEL,
        Run::Vertical,
    );
    quads
}

/// The icon tile's quads as render elements, in front-to-back order.
pub fn icon_tile_elements(
    tile: Rectangle<i32, Logical>,
    vp: Viewport,
) -> Vec<SolidColorRenderElement> {
    icon_tile_quads(vp.palette, tile)
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
    decoration_quads(vp.palette, client, style, active, maximized, pressed)
        .into_iter()
        .map(|(r, c)| solid_quad(r, c, vp))
        .collect()
}

/// The decoration as plain colored rectangles, front to back.
///
/// Split from [`decoration_elements`] so the frame's geometry can be asserted on. The border's
/// corner sections in particular are eight pieces that have to meet exactly, and "looks right
/// at a glance" has already missed a two-pixel seam more than once.
pub fn decoration_quads(
    palette: &Palette,
    client: Rectangle<i32, Logical>,
    style: FrameStyle,
    active: bool,
    maximized: bool,
    pressed: Option<FramePart>,
) -> Vec<(Rectangle<i32, Logical>, Color32F)> {
    let p = frame_palette(palette, active);
    let outline = c32(palette.outer_line);
    let mut quads: Vec<(Rectangle<i32, Logical>, Color32F)> = Vec::new();

    let is_pressed = |part: FramePart| pressed == Some(part);
    // Most pieces sit in a top-to-bottom stack (the titlebar row inside the frame, menu rows,
    // icon tiles); the border's top and bottom sections are the exception and say so.
    let shades = |sunken: bool| Shades {
        face: if sunken { p.press } else { p.face },
        light: p.light,
        dark: p.dark,
    };
    let piece = |quads: &mut Vec<_>, r: Rectangle<i32, Logical>, sunken: bool| {
        beveled_quads(quads, r, shades(sunken), !sunken, BEVEL, Run::Vertical);
    };
    let piece_across = |quads: &mut Vec<_>, r: Rectangle<i32, Logical>| {
        beveled_quads(quads, r, shades(false), true, BEVEL, Run::Horizontal);
    };
    if style.titlebar {
        // Menu button: IRIX horizontal-bar glyph (absent under NO_MENU_BUTTON).
        // Measured off the original 30x30 IRIX button: bar at (3,12), 22x5.
        let menu = menu_button(client, style);
        if let Some(mb) = menu {
            glyph_outline(
                &mut quads,
                outline,
                rect(mb.loc.x + 3, mb.loc.y + 12, 22, 5),
                2,
            );
        }

        let (minimize, maximize) = right_buttons(client, style);
        if let Some(r) = minimize {
            // Minimize: small IRIX box — measured at (13,12), 5x5, 1px shadow.
            glyph_outline(
                &mut quads,
                outline,
                rect(r.loc.x + 13, r.loc.y + 12, 5, 5),
                1,
            );
        }
        if let Some(r) = maximize {
            // Maximize: large IRIX box — measured at (5,3), 20x22, 1px shadow.
            // While maximized the box shifts to (6,4) and the shadow flips
            // up-left ("pressed in").
            if maximized {
                glyph_outline(
                    &mut quads,
                    outline,
                    rect(r.loc.x + 6, r.loc.y + 4, 20, 22),
                    -1,
                );
            } else {
                glyph_outline(
                    &mut quads,
                    outline,
                    rect(r.loc.x + 5, r.loc.y + 3, 20, 22),
                    1,
                );
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

        // A window that cannot be resized gets an unbroken ring instead of the jointed border:
        // the corner sections *are* the resize grips, and drawing grips on a fixed-size window
        // invites a drag that will not happen. A ring rather than four beveled bands, because
        // bands bevel each of their own four edges -- so the seam between the top band and the
        // titlebar would run on past the titlebar and across both top corners, and the same at
        // the bottom. The ring bevels only its outer and inner edges, which is what IRIX drew.
        if !style.resizable.any() {
            beveled_ring(
                &mut quads,
                rect(bx, by, bw, bh),
                t,
                face,
                p.light,
                p.dark,
                BEVEL,
            );
            quads.push((frame, outline));
            return quads;
        }

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

            // Drawn as two beveled rects, H in front of V, with a face patch hiding the strip
            // of H's inward-facing shadow that falls where the vertical arm passes through.
            //
            // The L is one raised piece, so its inward shadow belongs on the inside of the L:
            // it starts at the L's *inner corner*, not at the outer edge. Without the patch it
            // runs on across the corner square, which reads as the seam under the titlebar
            // carrying on into the corner and stopping halfway.
            //
            // The patch reaches from the corner square's inner side out to the arm's outer
            // edge, stopping a bevel short of it: the last strip there is H's own outer
            // highlight, which is the L's edge and has to stay.
            let patch_y = if top {
                h_rect.loc.y + t - BEVEL
            } else {
                h_rect.loc.y
            };
            let patch_x = if left {
                v_rect.loc.x + BEVEL
            } else {
                v_rect.loc.x
            };
            quads.push((rect(patch_x, patch_y, t - BEVEL, BEVEL), face));
            piece_across(&mut quads, h_rect);
            piece(&mut quads, v_rect, false);
        }

        // Edge middles between the corner arms (their own bevels form the
        // aligned corner seams).
        if bw > 2 * arm_w {
            piece_across(&mut quads, rect(bx + arm_w, by, bw - 2 * arm_w, t));
            piece_across(&mut quads, rect(bx + arm_w, by + bh - t, bw - 2 * arm_w, t));
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
        quads.push((frame, outline));
    }

    quads
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
            resizable: Resizable::BOTH,
            title_align: TitleAlign::Left,
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

#[cfg(test)]
mod capability_tests {
    use super::*;

    fn client() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::new(100, 100), Size::from((400, 300)))
    }

    fn style() -> FrameStyle {
        FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            min_btn: true,
            max_btn: true,
            resizable: Resizable::BOTH,
            title_align: TitleAlign::Left,
        }
    }

    /// A point on the middle of each border, and on each corner arm.
    fn on_left(c: Rectangle<i32, Logical>, s: FrameStyle) -> Point<f64, Logical> {
        let f = frame_rect(c, s);
        Point::from((f.loc.x as f64 + 1.0, (f.loc.y + f.size.h / 2) as f64))
    }
    fn on_bottom(c: Rectangle<i32, Logical>, s: FrameStyle) -> Point<f64, Logical> {
        let f = frame_rect(c, s);
        Point::from((
            (f.loc.x + f.size.w / 2) as f64,
            (f.loc.y + f.size.h) as f64 - 1.0,
        ))
    }
    fn on_bottom_left_corner(c: Rectangle<i32, Logical>, s: FrameStyle) -> Point<f64, Logical> {
        let f = frame_rect(c, s);
        Point::from((f.loc.x as f64 + 1.0, (f.loc.y + f.size.h) as f64 - 1.0))
    }

    #[test]
    fn a_fixed_size_window_has_no_resize_handles_anywhere_on_its_border() {
        // The border is still the frame -- it occludes, and a middle-drag still moves the
        // window -- but every point on it is inert.
        let fixed = FrameStyle {
            resizable: Resizable::NONE,
            ..style()
        };
        for point in [
            on_left(client(), fixed),
            on_bottom(client(), fixed),
            on_bottom_left_corner(client(), fixed),
        ] {
            assert_eq!(
                hit_test(client(), fixed, point),
                Some(FramePart::Border),
                "{point:?} should be inert border"
            );
        }
    }

    #[test]
    fn a_window_fixed_in_one_axis_keeps_the_other_axis_handles() {
        // A fixed *width* is the common case -- a settings panel, a palette -- and its top and
        // bottom edges are still worth grabbing.
        let fixed_width = FrameStyle {
            resizable: Resizable {
                horizontal: false,
                vertical: true,
            },
            ..style()
        };
        // The left border resizes nothing now.
        assert_eq!(
            hit_test(client(), fixed_width, on_left(client(), fixed_width)),
            Some(FramePart::Border)
        );
        // The bottom edge still does, vertically only.
        assert_eq!(
            hit_test(client(), fixed_width, on_bottom(client(), fixed_width)),
            Some(FramePart::Resize(ResizeEdge {
                bottom: true,
                ..Default::default()
            }))
        );
        // And a corner degrades to the axis that is left, rather than staying diagonal.
        assert_eq!(
            hit_test(
                client(),
                fixed_width,
                on_bottom_left_corner(client(), fixed_width)
            ),
            Some(FramePart::Resize(ResizeEdge {
                bottom: true,
                ..Default::default()
            }))
        );
    }

    #[test]
    fn the_titlebar_and_its_buttons_are_unaffected_by_fixed_size() {
        // Only the border loses its handles; the titlebar still moves the window.
        let fixed = FrameStyle {
            resizable: Resizable::NONE,
            ..style()
        };
        let tb = titlebar_rect(client());
        let middle = Point::from((
            (tb.loc.x + tb.size.w / 2) as f64,
            (tb.loc.y + tb.size.h / 2) as f64,
        ));
        assert_eq!(hit_test(client(), fixed, middle), Some(FramePart::Titlebar));
    }

    #[test]
    fn dropping_the_maximize_button_slides_minimize_into_its_place() {
        // Not a gap in the titlebar: the buttons pack from the right edge inward.
        let full = style();
        let (min_full, max_full) = right_buttons(client(), full);
        let no_max = FrameStyle {
            max_btn: false,
            ..full
        };
        let (min_alone, max_alone) = right_buttons(client(), no_max);

        assert!(max_alone.is_none());
        assert_eq!(
            min_alone.unwrap(),
            max_full.unwrap(),
            "minimize should take the outermost slot"
        );
        assert_ne!(min_alone.unwrap(), min_full.unwrap());
    }

    #[test]
    fn a_window_with_neither_right_button_gives_the_title_the_whole_bar() {
        let bare = FrameStyle {
            min_btn: false,
            max_btn: false,
            ..style()
        };
        let tb = titlebar_rect(client());
        let title = title_bar_piece(client(), bare);
        assert_eq!(title.loc.x + title.size.w, tb.loc.x + tb.size.w);
    }

    #[test]
    fn hit_testing_finds_the_button_that_moved() {
        // The whole point of sliding minimize outward: clicking where maximize used to be must
        // now minimize, not fall through to the titlebar.
        let no_max = FrameStyle {
            max_btn: false,
            ..style()
        };
        let (minimize, _) = right_buttons(client(), no_max);
        let r = minimize.unwrap();
        let middle = Point::from((
            (r.loc.x + r.size.w / 2) as f64,
            (r.loc.y + r.size.h / 2) as f64,
        ));
        assert_eq!(
            hit_test(client(), no_max, middle),
            Some(FramePart::MinimizeButton)
        );
    }

    #[test]
    fn a_fixed_size_frame_still_covers_the_same_area() {
        // The border keeps its thickness -- only the corner *sections* go -- so nothing about
        // the window's placement or its occlusion changes.
        let fixed = FrameStyle {
            resizable: Resizable::NONE,
            ..style()
        };
        assert_eq!(frame_rect(client(), fixed), frame_rect(client(), style()));
        assert_eq!(insets(fixed), insets(style()));
    }
}

#[cfg(test)]
mod border_tests {
    use super::*;

    fn style() -> FrameStyle {
        FrameStyle {
            titlebar: true,
            border: true,
            menu_btn: true,
            min_btn: true,
            max_btn: true,
            resizable: Resizable::BOTH,
            title_align: TitleAlign::Left,
        }
    }

    /// A 300x160 client at (40, 60), so the frame's band runs from (33, 23) inside the outline.
    fn client() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::new(40, 60), Size::from((300, 160)))
    }

    /// The color at `point`, by painting the quads back to front the way the renderer does.
    fn color_at(quads: &[(Rectangle<i32, Logical>, Color32F)], x: i32, y: i32) -> Option<Color32F> {
        let point = Point::new(x, y);
        quads
            .iter()
            .find(|(r, _)| r.contains(point))
            .map(|(_, c)| *c)
    }

    /// The active frame's tones in the default scheme, which is what these tests draw in.
    fn active() -> Palette4 {
        frame_palette(wlrix_ui::palette::DEFAULT, true)
    }

    fn shade(quads: &[(Rectangle<i32, Logical>, Color32F)], x: i32, y: i32) -> &'static str {
        let p = active();
        match color_at(quads, x, y) {
            Some(c) if c == p.light => "light",
            Some(c) if c == p.dark => "dark",
            Some(c) if c == p.face => "face",
            Some(c) if c == c32(wlrix_ui::palette::DEFAULT.outer_line) => "outline",
            Some(_) => "other",
            None => "none",
        }
    }

    fn border() -> Vec<(Rectangle<i32, Logical>, Color32F)> {
        decoration_quads(
            wlrix_ui::palette::DEFAULT,
            client(),
            style(),
            true,
            false,
            None,
        )
    }

    /// The band's geometry, spelled out once: the outline is 1px, the band `BORDER - 1` thick,
    /// and a corner arm reaches the width of a titlebar button past it.
    const BX: i32 = 33;
    const BY: i32 = 23;
    const T: i32 = BORDER - 1;
    const ARM: i32 = T + BUTTON_W;

    /// A seam between two border sections is one piece's shadow beside the next one's
    /// highlight, and it has to cross the *whole* band. Getting this wrong left the mark
    /// between the corner arm and the top edge stopping a bevel short at each end.
    #[test]
    fn the_seam_between_top_sections_crosses_the_whole_band() {
        let quads = border();
        let seam = BX + ARM;
        for y in BY..BY + T {
            assert_eq!(shade(&quads, seam - 1, y), "dark", "shadow side at y={y}");
            assert_eq!(shade(&quads, seam, y), "light", "highlight side at y={y}");
        }
    }

    /// The same seam on a side, which was already right and must stay right.
    #[test]
    fn the_seam_between_side_sections_crosses_the_whole_band() {
        let quads = border();
        let seam = BY + ARM;
        for x in BX..BX + T {
            assert_eq!(shade(&quads, x, seam - 1), "dark", "shadow side at x={x}");
            assert_eq!(shade(&quads, x, seam), "light", "highlight side at x={x}");
        }
    }

    /// A corner is one raised L, so its inward shadow starts at the L's *inner* corner. Run it
    /// out to the frame's edge instead and it reads as the seam under the titlebar carrying on
    /// into the corner and stopping halfway -- which is what it used to do.
    #[test]
    fn nothing_crosses_the_corner_square() {
        let quads = border();
        let row = BY + T - 1; // the top band's inward shadow
        // The L's outer highlight, which does run the full height of the arm.
        assert_eq!(shade(&quads, BX, row), "light");
        // The corner square itself: plain face all the way to the inner corner.
        for x in BX + BEVEL..BX + T {
            assert_eq!(shade(&quads, x, row), "face", "x={x} should be clear");
        }
        // And the shadow picks up exactly there.
        assert_eq!(shade(&quads, BX + T, row), "dark");
    }

    #[test]
    fn every_corner_is_clear_not_just_the_first_one() {
        // The mirroring is index arithmetic and easy to get wrong on one corner alone.
        let quads = border();
        let frame = frame_rect(client(), style());
        let (right, bottom) = (
            frame.loc.x + frame.size.w - 1,
            frame.loc.y + frame.size.h - 1,
        );
        for (x, y) in [
            (BX + BEVEL + 1, BY + T - 1),
            (right - 1 - BEVEL - 1, BY + T - 1),
            (BX + BEVEL + 1, bottom - 1 - T + 1),
            (right - 1 - BEVEL - 1, bottom - 1 - T + 1),
        ] {
            assert_eq!(shade(&quads, x, y), "face", "corner at ({x}, {y})");
        }
    }

    /// A fixed-size window has no sections to seam, so nothing should read as one.
    #[test]
    fn the_fixed_size_ring_has_no_seams_at_all() {
        let fixed = FrameStyle {
            resizable: Resizable::NONE,
            ..style()
        };
        let quads = decoration_quads(
            wlrix_ui::palette::DEFAULT,
            client(),
            fixed,
            true,
            false,
            None,
        );
        // Straight down the left band, past where a corner seam would have been.
        for y in BY..BY + ARM + 20 {
            assert_eq!(shade(&quads, BX, y), "light", "outer edge broken at y={y}");
        }
    }
}

#[cfg(test)]
mod titlebar_only_tests {
    use super::*;

    /// What the toolchest gets: a titlebar and nothing else.
    fn style() -> FrameStyle {
        FrameStyle {
            titlebar: true,
            border: false,
            menu_btn: false,
            min_btn: false,
            max_btn: false,
            resizable: Resizable::NONE,
            title_align: TitleAlign::Centered,
        }
    }

    fn client() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::new(40, 60), Size::from((220, 150)))
    }

    #[test]
    fn the_frame_is_the_titlebar_and_nothing_more() {
        let frame = frame_rect(client(), style());
        // Grown at the top only: no border on any side.
        assert_eq!(insets(style()), (0, TITLEBAR_HEIGHT, 0, 0));
        assert_eq!(frame.size.w, client().size.w);
        assert_eq!(frame.loc.y, client().loc.y - TITLEBAR_HEIGHT);
    }

    #[test]
    fn the_titlebar_still_moves_the_window_and_posts_its_menu() {
        // The reason it keeps a titlebar at all: without one it could not be moved, and the
        // client would have to draw chrome of its own.
        let tb = titlebar_rect(client());
        let middle = Point::from((
            (tb.loc.x + tb.size.w / 2) as f64,
            (tb.loc.y + tb.size.h / 2) as f64,
        ));
        assert_eq!(
            hit_test(client(), style(), middle),
            Some(FramePart::Titlebar)
        );
    }

    #[test]
    fn there_are_no_buttons_to_press() {
        let (minimize, maximize) = right_buttons(client(), style());
        assert!(minimize.is_none() && maximize.is_none());
        assert!(menu_button(client(), style()).is_none());
        // And the title has the whole bar to itself.
        assert_eq!(title_bar_piece(client(), style()), titlebar_rect(client()));
    }

    #[test]
    fn there_is_no_border_to_grab() {
        // Just outside the client on each side: with no border there is no frame there at all,
        // so the press belongs to whatever is underneath.
        let c = client();
        for point in [
            Point::from((c.loc.x as f64 - 1.0, (c.loc.y + 10) as f64)),
            Point::from(((c.loc.x + c.size.w) as f64 + 1.0, (c.loc.y + 10) as f64)),
            Point::from(((c.loc.x + 10) as f64, (c.loc.y + c.size.h) as f64 + 1.0)),
        ] {
            assert_eq!(hit_test(c, style(), point), None, "{point:?}");
        }
    }

    #[test]
    fn the_border_draws_nothing() {
        // Every quad belongs to the titlebar; none of them reaches below the client's top edge
        // or outside its width.
        let quads = decoration_quads(
            wlrix_ui::palette::DEFAULT,
            client(),
            style(),
            true,
            false,
            None,
        );
        assert!(!quads.is_empty());
        let frame = frame_rect(client(), style());
        for (r, _) in &quads {
            assert!(frame.contains_rect(*r), "{r:?} escapes {frame:?}");
            assert!(
                r.loc.y + r.size.h <= client().loc.y,
                "{r:?} is below the titlebar"
            );
        }
    }

    #[test]
    fn the_title_is_centered_in_the_bar() {
        // No buttons to line up beside, so the name sits in the middle rather than against an
        // edge with nothing to relate to.
        let area = title_text_area(client(), style());
        let text = 60;
        let x = title_text_start(area.loc.x, area.size.w, text, style().title_align);
        assert_eq!(
            x - area.loc.x,
            area.loc.x + area.size.w - (x + text),
            "the gaps either side should match"
        );
    }

    /// The same thing again with a real rasterized width, so the arithmetic is exercised
    /// against text of the size the titlebar actually draws rather than a round number.
    #[test]
    fn a_real_title_lands_centered_in_the_bar() {
        let Ok(mut text) = crate::text::TextRenderer::new() else {
            return; // no fonts installed
        };
        let color = title_text(wlrix_ui::palette::DEFAULT, true);
        let Some(rasterized) = text.rasterize("Toolchest", crate::text::TITLE_PX, color) else {
            return;
        };
        let area = title_text_area(client(), style());
        assert!(
            rasterized.width < area.size.w,
            "the fixture's bar should be wide enough to center in"
        );
        let x = title_text_start(
            area.loc.x,
            area.size.w,
            rasterized.width,
            style().title_align,
        );
        let left = x - area.loc.x;
        let right = area.loc.x + area.size.w - (x + rasterized.width);
        assert!(
            (left - right).abs() <= 1,
            "gaps {left} and {right} should match within rounding"
        );
    }

    #[test]
    fn a_title_too_wide_to_center_starts_at_the_left_and_is_clipped_from_the_right() {
        // Centering an over-long title would push it left of the run and clip the beginning of
        // the name -- the part that says which window this is.
        let area = title_text_area(client(), style());
        let x = title_text_start(
            area.loc.x,
            area.size.w,
            area.size.w + 200,
            TitleAlign::Centered,
        );
        assert_eq!(x, area.loc.x);
    }

    #[test]
    fn an_ordinary_window_still_starts_its_title_at_the_left() {
        assert_eq!(title_text_start(40, 300, 60, TitleAlign::Left), 40);
        assert_eq!(title_text_start(40, 300, 600, TitleAlign::Left), 40);
    }

    #[test]
    fn the_wireframe_is_one_ring_round_the_lot() {
        // With no border there is no inner edge to trace, so the moving outline is the frame's
        // own four strokes plus the rule under the titlebar -- not the two concentric rings a
        // bordered window gets.
        let quads = drag_outline_quads(DragOutline {
            client: client(),
            style: Some(style()),
        });
        assert_eq!(quads.len(), 5);
        let frame = frame_rect(client(), style());
        let x0 = quads.iter().map(|r| r.loc.x).min().unwrap();
        let x1 = quads.iter().map(|r| r.loc.x + r.size.w).max().unwrap();
        let y0 = quads.iter().map(|r| r.loc.y).min().unwrap();
        let y1 = quads.iter().map(|r| r.loc.y + r.size.h).max().unwrap();
        assert_eq!(
            (x0, y0, x1 - x0, y1 - y0),
            (frame.loc.x, frame.loc.y, frame.size.w, frame.size.h)
        );
    }
}

/// The icon tile's measurements, which are the IRIX originals rather than anything derived, and
/// so are worth stating once where a change to them shows up as a failure.
#[cfg(test)]
mod icon_tests {
    use super::*;

    fn tile() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::new(120, 80), Size::from((ICON_TILE_W, ICON_TILE_H)))
    }

    #[test]
    fn the_tile_is_the_size_of_the_original() {
        assert_eq!((ICON_TILE_W, ICON_TILE_H), (97, 99));
    }

    #[test]
    fn the_preview_sits_six_pixels_in_from_the_top_left() {
        let t = tile();
        let preview = icon_image_area(t);
        assert_eq!(preview.loc - t.loc, Point::new(6, 6));
        assert_eq!(preview.size, Size::from((85, 67)));
        // A thumbnail is captured to fill that area exactly, so the two must not drift apart.
        assert_eq!(icon_thumbnail_size(), preview.size);
        // Inset by the same amount on the right, so the preview is centered in the tile.
        assert_eq!(t.loc.x + t.size.w - (preview.loc.x + preview.size.w), 6);
    }

    /// The well's sunken edge sits outside the preview rather than eating into it: the edge
    /// starts at (4, 4) and the thumbnail still gets its full 85x67 from (6, 6).
    #[test]
    fn the_sunken_edge_starts_two_pixels_out_from_the_preview() {
        let t = tile();
        let well = icon_preview_well(t);
        assert_eq!(well.loc - t.loc, Point::new(4, 4));
        assert_eq!(well.size, Size::from((89, 71)));
        assert!(well.contains_rect(icon_image_area(t)));
        // Still clear of the groove below it.
        assert!(well.loc.y + well.size.h <= t.loc.y + ICON_SEPARATOR_Y);
    }

    /// A sunken piece is shadowed at the top-left and lit at the bottom-right -- the opposite of
    /// the panel around it, which is what makes the preview read as set into the tile.
    #[test]
    fn the_well_is_sunken_and_the_panel_is_raised() {
        let t = tile();
        let quads = icon_tile_quads(wlrix_ui::palette::DEFAULT, t);
        let well = icon_preview_well(t);
        let corner = |r: &Rectangle<i32, Logical>, at: Point<i32, Logical>| {
            r.loc == at && (r.size.w == BEVEL || r.size.h == BEVEL)
        };
        let shade_at = |at: Point<i32, Logical>| {
            quads
                .iter()
                .find(|(r, _)| corner(r, at))
                .map(|(_, c)| *c)
                .expect("a bevel strip starts at that corner")
        };
        // A minimized tile takes the *inactive* frame's tones, per 4DWmSpec's `*icon*`.
        let tones = frame_palette(wlrix_ui::palette::DEFAULT, false);
        assert_eq!(shade_at(well.loc), tones.dark, "well's top-left");
        assert_eq!(shade_at(t.loc), tones.light, "panel's top-left");
    }

    #[test]
    fn the_groove_is_seventy_eight_pixels_down() {
        let t = tile();
        assert_eq!(ICON_SEPARATOR_Y, 78);
        // Clear of the preview, which ends at 73, and of the title, which starts below it.
        let preview = icon_image_area(t);
        assert!(preview.loc.y + preview.size.h < t.loc.y + ICON_SEPARATOR_Y);
        assert_eq!(icon_label_rect(t).loc.y, t.loc.y + ICON_SEPARATOR_Y + 2);
    }

    #[test]
    fn the_title_fills_what_is_left_below_the_groove() {
        let t = tile();
        let label = icon_label_rect(t);
        // Inside the panel's bevel on both sides and along the bottom.
        assert_eq!(label.loc.x, t.loc.x + BEVEL);
        assert_eq!(label.size.w, t.size.w - 2 * BEVEL);
        assert_eq!(label.loc.y + label.size.h, t.loc.y + t.size.h - BEVEL);
        assert!(label.size.h > 0, "no room left for the title");
    }

    /// The backdrop and the groove are drawn over the panel's face, and the quad list runs front
    /// to back -- so both have to come ahead of the face, not after it. The face covers them
    /// both, so getting this backwards leaves a bare gray tile with no preview and no groove.
    #[test]
    fn the_backdrop_and_groove_come_before_the_panel_face() {
        let t = tile();
        let quads = icon_tile_quads(wlrix_ui::palette::DEFAULT, t);
        let face = quads
            .iter()
            .position(|(r, c)| {
                *c == c32(wlrix_ui::palette::DEFAULT.icon_tile_face)
                    && r.contains_rect(icon_image_area(t))
            })
            .expect("the panel has a face quad covering the preview");
        let backdrop = quads
            .iter()
            .position(|(r, c)| {
                *r == icon_image_area(t) && *c == icon_well(wlrix_ui::palette::DEFAULT)
            })
            .expect("the well has a backdrop face");
        let groove = quads
            .iter()
            .position(|(r, _)| r.loc.y == t.loc.y + ICON_SEPARATOR_Y && r.size.h == 1)
            .expect("the tile has a groove");
        assert!(backdrop < face, "the face would hide the preview");
        assert!(groove < face, "the face would hide the groove");
    }
}
