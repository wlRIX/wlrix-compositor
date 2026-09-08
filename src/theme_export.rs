// SPDX-License-Identifier: GPL-3.0-or-later
//! The window frame, exported as GTK theme assets.
//!
//! A GTK client never negotiates `xdg-decoration`, so [`crate::frame`] gives it the border and
//! no titlebar and the client draws its own headerbar inside our frame. That headerbar is the
//! seam: correct in structure, Adwaita in appearance. This module produces the assets that make
//! it look like the titlebar row the compositor would have drawn instead.
//!
//! ## Why this is not a screenshot
//!
//! [`decoration::decoration_quads`] returns the whole frame as axis-aligned colored rectangles
//! and nothing else -- no gradients, no curves, no antialiasing. So the frame is not pixels to
//! be captured off a screen; it is a rectangle list to be replayed. One `<rect>` per quad is a
//! *lossless* export that scales to any HiDPI factor, which no capture of a 1x screen can be.
//!
//! KDE does the same thing for Breeze -- the reference assets in `_docs/kde-config-ref` are
//! SVGs its decoration painted through `QSvgGenerator`.
//!
//! ## The two things that will silently produce a flat slab
//!
//! - **`decoration_quads` is topmost-first.** `wlrix-ui` emits in painter's order and a
//!   render-element list is the reverse, which [`wlrix_ui::bevel`]'s module docs state and
//!   [`decoration::beveled_ring`] repeats. SVG is painter's order, so the run is reversed here
//!   -- otherwise a button's face covers its own glyph.
//! - **A Motif bevel is not a CSS border.** Titlebar pieces use [`Run::Vertical`]: the top and
//!   bottom shadows run the *full width* and the left/right pair is inset between them, while a
//!   CSS border mitres its corners at 45 degrees. `border-style: outset` cannot draw one.
//!
//! ## Why the buttons are images and the bar is not
//!
//! A button is 30x30 with a glyph at whole-pixel offsets, so it is exported. The *bar* behind it
//! is a bevel with nothing in it, and four inset `box-shadow`s reproduce `Run::Vertical` exactly
//! -- listing the left and right pair first so the full-width top and bottom paint over them.
//! That is worth doing rather than slicing an image: the bar then takes its colors from
//! `@define-color` and needs no asset at all.
//!
//! It is also the only thing that works. GTK 3 *parses* `border-image-slice: 2 fill` and then
//! ignores the `fill`, so the middle of the source is never drawn and the bar comes out
//! transparent -- which under a compositor means black. Found by rendering it, not by reading it.
//!
//! [`Run::Vertical`]: wlrix_ui::bevel::Run::Vertical

use std::fmt::Write as _;
use std::path::Path;

use smithay::backend::renderer::Color32F;
use smithay::utils::{Logical, Point, Rectangle, Size};
use wlrix_ui::palette::Palette;

use crate::decoration::{self, FramePart, FrameStyle, Resizable, TitleAlign};

/// A quad as the frame emits it.
type Quad = (Rectangle<i32, Logical>, Color32F);

/// Which GTK generation a stylesheet is for.
///
/// They need genuinely different files: GTK 3 apps read Adwaita's `@theme_bg_color` family and
/// GTK 4 apps read libadwaita's `@window_bg_color` family, and neither answers to the other's
/// names. The *assets* are shared -- an SVG does not care who loads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gtk {
    V3,
    V4,
}

impl Gtk {
    fn dir(self) -> &'static str {
        match self {
            Self::V3 => "gtk-3.0",
            Self::V4 => "gtk-4.0",
        }
    }

    /// The selector for one titlebar button, in this generation's node names.
    ///
    /// They are not the same and the difference is not cosmetic: **GTK 4 has no `titlebutton`
    /// class at all**. Its window controls are `windowcontrols > button.minimize` and friends,
    /// with `image-button` alongside, while GTK 3 spells the same button
    /// `headerbar button.titlebutton.minimize`. A sheet written for one matches nothing in the
    /// other -- silently, since a CSS selector that matches nothing is not an error. Read off a
    /// live widget tree rather than from documentation, after exactly that happened.
    fn button(self, class: &str, state: &str, prefix: &str) -> String {
        match self {
            Self::V3 => format!(
                "{prefix}headerbar button.titlebutton.{class}{state},\n\
                 {prefix}.titlebar button.titlebutton.{class}{state}"
            ),
            Self::V4 => format!("{prefix}windowcontrols > button.{class}{state}"),
        }
    }
}

/// Write the whole theme's generated half under `dir`.
///
/// The hand-written half -- the structural CSS and `index.theme` -- lives in `wlrix-assets` and
/// is not touched. This only writes what is derived from the frame and the palette, so a run is
/// safe to repeat and its output is safe to diff.
pub fn dump(dir: &Path) -> std::io::Result<()> {
    for palette in wlrix_ui::palette::ALL {
        // Emptied rather than written over: an asset that stops being generated would otherwise
        // sit in the tree forever, and `check-gtk-theme` -- which only looks for a *diff* -- would
        // go on passing with a dead file installed beside the live ones.
        let assets = dir.join("assets").join(palette.id);
        if assets.is_dir() {
            std::fs::remove_dir_all(&assets)?;
        }
        std::fs::create_dir_all(&assets)?;
        let written = assets_for(palette);
        for (name, svg) in &written {
            std::fs::write(assets.join(format!("{name}.svg")), svg)?;
        }

        for gtk in [Gtk::V3, Gtk::V4] {
            let schemes = dir.join(gtk.dir()).join("schemes");
            std::fs::create_dir_all(&schemes)?;
            std::fs::write(
                schemes.join(format!("{}.css", palette.id)),
                scheme_css(palette, gtk),
            )?;
        }

        // One line per scheme rather than one per file: eighty paths scrolling past says
        // nothing a count does not, and this is run from a `just` recipe beside `palette`.
        println!(
            "  {} ({} assets, gtk-3.0 + gtk-4.0)",
            palette.id,
            written.len()
        );
    }
    Ok(())
}

/// Every asset one scheme needs, as `(file stem, svg)`.
///
/// Five buttons by four states: twenty files. There is no hover asset because Motif has no hover
/// state -- a titlebar button reacts to being pressed and to nothing else -- and no asset for the
/// bar itself, which is four inset box-shadows in the stylesheet.
fn assets_for(palette: &Palette) -> Vec<(String, String)> {
    let mut out = Vec::new();

    for (button, part, maximized, glyph) in [
        ("menu", FramePart::MenuButton, false, true),
        ("minimize", FramePart::MinimizeButton, false, true),
        ("maximize", FramePart::MaximizeButton, false, true),
        // The maximize button while the window *is* maximized: IRIX flips the box's shadow to
        // up-left so the glyph reads as pushed in.
        ("maximized", FramePart::MaximizeButton, true, true),
        // Close, which IRIX had no button for -- it lives in the window menu, and the
        // compositor's own frame has no such button either. GTK draws one anyway whenever the
        // decoration layout asks for it, and that layout is not always ours to set: a settings
        // *portal* overrides `settings.ini` outright, so a session with one keeps GTK's default
        // `menu:close` until `org.freedesktop.impl.portal.Settings` is implemented.
        //
        // So it gets the bare button face and keeps GTK's own symbol on top. Hiding it instead
        // would leave a window that cannot be closed from its titlebar, which is a worse answer
        // to a fidelity question than an extra button.
        ("close", FramePart::MinimizeButton, false, false),
    ] {
        for (state, active) in [("active", true), ("backdrop", false)] {
            for (suffix, pressed) in [("", false), ("-pressed", true)] {
                out.push((
                    format!("{button}-{state}{suffix}"),
                    button_svg(palette, part, active, maximized, pressed, glyph),
                ));
            }
        }
    }

    out
}

/// The full frame style, so every button exists to be cropped out of it.
///
/// `border: false` deliberately: the border's quads sit outside the titlebar row and would only
/// be cropped away again, and leaving it off keeps the sample geometry to the one row this
/// module is about.
fn titlebar_style() -> FrameStyle {
    FrameStyle {
        titlebar: true,
        border: false,
        menu_btn: true,
        min_btn: true,
        max_btn: true,
        resizable: Resizable::BOTH,
        title_align: TitleAlign::Left,
    }
}

/// A client rectangle whose titlebar row starts at the origin and is `width` across.
fn sample_client(width: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::new(0, decoration::TITLEBAR_HEIGHT),
        Size::from((width, 100)),
    )
}

/// One 30x30 titlebar button, cropped out of a real frame.
///
/// Cropped rather than drawn: asking the frame for the whole titlebar and taking one button out
/// of it means the button's bevel, its glyph and the glyph's offset shadow are whatever
/// [`decoration::decoration_quads`] says they are, with nothing restated here to drift.
///
/// `glyph: false` gives the bare face, which is what a button wlRIX has no symbol for gets --
/// see the `close` entry in [`assets_for`].
fn button_svg(
    palette: &Palette,
    part: FramePart,
    active: bool,
    maximized: bool,
    pressed: bool,
    glyph: bool,
) -> String {
    let style = titlebar_style();
    // Wide enough that the four pieces do not collide: three buttons plus a title run.
    let client = sample_client(decoration::BUTTON_W * 8);
    let (minimize, maximize) = decoration::right_buttons(client, style);
    let area = match part {
        FramePart::MenuButton => decoration::menu_button(client, style),
        FramePart::MinimizeButton => minimize,
        FramePart::MaximizeButton => maximize,
        _ => None,
    }
    .expect("the sample style enables every button");

    let quads = decoration::decoration_quads(
        palette,
        client,
        style,
        active,
        maximized,
        pressed.then_some(part),
    );
    let mut quads = crop(&quads, area);
    if !glyph {
        // The glyph is exactly the quads drawn in `outer_line` -- `glyph_outline` uses that one
        // color for the outline and its drop shadow, and nothing else in a button does. So
        // "the face without its glyph" is a filter rather than a second way of drawing a bevel.
        let outline = Color32F::from(palette.outer_line.to_f32_array());
        quads.retain(|(_, color)| *color != outline);
    }
    svg(area.size, &quads)
}

/// Clip `quads` to `area` and move them into its own coordinate space.
fn crop(quads: &[Quad], area: Rectangle<i32, Logical>) -> Vec<Quad> {
    quads
        .iter()
        .filter_map(|(r, c)| {
            r.intersection(area).map(|mut r| {
                r.loc -= area.loc;
                (r, *c)
            })
        })
        .filter(|(r, _)| !r.is_empty())
        .collect()
}

/// The quads as an SVG of that size.
///
/// **Reversed**, because `decoration_quads` answers topmost-first and SVG paints in document
/// order. `shape-rendering="crispEdges"` because every edge here is a whole-pixel boundary and
/// antialiasing one would turn a two-pixel bevel into a smear.
fn svg(size: Size<i32, Logical>, quads: &[Quad]) -> String {
    let (w, h) = (size.w, size.h);
    let mut out = String::new();
    // No `--` anywhere in this comment. A double hyphen is illegal *inside* an XML comment, and
    // an SVG loader is a strict XML parser: the flag that produces these files cannot be named
    // here, or every asset fails to load with a parse error rather than looking wrong. wlRIX has
    // paid for this once already, in the settings daemon's introspection XML.
    out.push_str(&format!(
        "<!-- GENERATED by the wlRIX compositor's GTK theme export. Do not edit: this is the\n     \
         window frame's own geometry, from src/decoration.rs. -->\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" \
         viewBox=\"0 0 {w} {h}\" shape-rendering=\"crispEdges\">\n"
    ));
    for (r, color) in quads.iter().rev() {
        let _ = writeln!(
            out,
            "  <rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
            r.loc.x,
            r.loc.y,
            r.size.w,
            r.size.h,
            hex(*color)
        );
    }
    out.push_str("</svg>\n");
    out
}

/// A quad's color as `#rrggbb`.
///
/// Exact, not approximate: [`wlrix_ui::color::Rgb::to_f32_array`] is a byte divided by 255, so
/// multiplying back and rounding recovers every one of the 256 values it could have been. Alpha
/// is dropped because the frame has none -- every quad in it is opaque.
fn hex(color: Color32F) -> String {
    let [r, g, b, _] = color.components();
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// One scheme's stylesheet: its colors, and which assets go on which button.
///
/// The structural rules -- sizes, padding, what is switched off -- are hand-written in
/// `wlrix-assets` and shared by every scheme. Only what actually varies with the palette is
/// generated, which is what keeps a scheme file short enough to read.
fn scheme_css(palette: &Palette, gtk: Gtk) -> String {
    let mut css = String::new();
    let c = |color: wlrix_ui::Rgb| hex(Color32F::from(color.to_f32_array()));

    let _ = write!(
        css,
        "/* GENERATED by `wlrix-compositor --dump-gtk-theme`. Do not edit; edit the palette\n\
         \x20  JSON in wlrix-assets/palette/ and regenerate.\n\
         \x20\n\
         \x20  {} ({}), for {}. Pairs with wlrix.css, which carries the structure.\n\
         \x20*/\n\n",
        palette.name,
        if palette.dark { "dark" } else { "light" },
        gtk.dir(),
    );

    // The frame's own colors, under names the structural sheet can reach.
    let _ = write!(
        css,
        "@define-color wlrix_title_active {};\n\
         @define-color wlrix_title_active_text {};\n\
         @define-color wlrix_title_inactive {};\n\
         @define-color wlrix_title_inactive_text {};\n\n",
        c(palette.title_active),
        c(palette.title_active_text),
        c(palette.title_inactive),
        c(palette.title_inactive_text),
    );

    css.push_str(&body_colors(palette, gtk));
    css.push_str(&asset_rules(palette, gtk));
    css
}

/// The window body's colors, in whichever family this GTK generation reads.
///
/// Not a full widget theme -- the widgets keep Adwaita's shape and only their palette moves.
/// That is deliberate: an IRIX GTK widget theme is the size of `Wlrix.Avalonia` and belongs in
/// its own milestone, while a window that is merely *the right gray* under a gold titlebar
/// already stops the headerbar from looking like a mistake.
///
/// # What is deliberately left alone
///
/// GTK's `warning` and `success` colors are **not** bound. The nearest wlRIX roles are IRIX's
/// `warningColor` and `informationColor`, which are pure blue and pure green -- they are the
/// colors of the icons in a Motif message dialog, not semantic accents. Binding them would give
/// a GTK application blue warning banners and pure-green success ones, which reads as *wrong
/// information* rather than as a different theme. `error` is bound, because red means the same
/// thing in both.
fn body_colors(palette: &Palette, gtk: Gtk) -> String {
    let c = |color: wlrix_ui::Rgb| hex(Color32F::from(color.to_f32_array()));
    let mut css = String::new();

    let pairs: Vec<(&str, String)> = match gtk {
        Gtk::V3 => vec![
            ("theme_bg_color", c(palette.panel)),
            ("theme_fg_color", c(palette.foreground)),
            ("theme_base_color", c(palette.text_background)),
            ("theme_text_color", c(palette.foreground)),
            ("theme_selected_bg_color", c(palette.selection_background)),
            ("theme_selected_fg_color", c(palette.selection_foreground)),
            ("insensitive_bg_color", c(palette.panel)),
            ("insensitive_fg_color", c(palette.disabled_foreground)),
            ("insensitive_base_color", c(palette.read_only)),
            ("borders", c(palette.outer_line)),
            ("unfocused_borders", c(palette.outer_line)),
            // IRIX does not dim a window's contents when it loses the keyboard -- only its
            // titlebar changes. So every unfocused color is its focused one.
            ("theme_unfocused_bg_color", c(palette.panel)),
            ("theme_unfocused_fg_color", c(palette.foreground)),
            ("theme_unfocused_base_color", c(palette.text_background)),
            ("theme_unfocused_text_color", c(palette.foreground)),
            (
                "theme_unfocused_selected_bg_color",
                c(palette.selection_background),
            ),
            (
                "theme_unfocused_selected_fg_color",
                c(palette.selection_foreground),
            ),
            ("tooltip_background", c(palette.tooltip_background)),
            ("tooltip_text", c(palette.tooltip_foreground)),
            ("error_color", c(palette.error)),
            ("link_color", c(palette.link)),
            ("link_visited_color", c(palette.link_visited)),
        ],
        Gtk::V4 => vec![
            ("window_bg_color", c(palette.panel)),
            ("window_fg_color", c(palette.foreground)),
            ("view_bg_color", c(palette.text_background)),
            ("view_fg_color", c(palette.foreground)),
            ("headerbar_bg_color", c(palette.title_active)),
            ("headerbar_fg_color", c(palette.title_active_text)),
            ("headerbar_border_color", c(palette.outer_line)),
            ("headerbar_backdrop_color", c(palette.title_inactive)),
            ("headerbar_shade_color", c(palette.outer_line)),
            ("sidebar_bg_color", c(palette.panel)),
            ("sidebar_fg_color", c(palette.foreground)),
            ("secondary_sidebar_bg_color", c(palette.panel)),
            ("card_bg_color", c(palette.face)),
            ("card_fg_color", c(palette.foreground)),
            ("dialog_bg_color", c(palette.panel)),
            ("dialog_fg_color", c(palette.foreground)),
            ("popover_bg_color", c(palette.panel)),
            ("popover_fg_color", c(palette.foreground)),
            ("accent_bg_color", c(palette.selection_background)),
            ("accent_fg_color", c(palette.selection_foreground)),
            ("accent_color", c(palette.accent)),
            ("destructive_bg_color", c(palette.error)),
            ("destructive_color", c(palette.error)),
            ("borders", c(palette.outer_line)),
        ],
    };

    for (name, value) in pairs {
        let _ = writeln!(css, "@define-color {name} {value};");
    }
    css.push('\n');
    css
}

/// Which asset each titlebar piece takes, for one scheme.
///
/// The paths are relative to this file, which is where GTK resolves a `url()` inside an
/// imported sheet from: `gtk-N.0/schemes/<id>.css` up two to the theme root, then into the
/// shared asset directory.
fn asset_rules(palette: &Palette, gtk: Gtk) -> String {
    let scheme = palette.id;
    let mut css = String::new();
    let c = |color: wlrix_ui::Rgb| hex(Color32F::from(color.to_f32_array()));
    let url = |name: &str| format!("url(\"../../assets/{scheme}/{name}.svg\")");

    // The bar's own bevel, as four inset shadows rather than an image. The left and right pair
    // is listed *first* so the full-width top and bottom paint over it, which is what
    // Run::Vertical means: the two that butt against the next piece run the whole span.
    let bevel = |light: wlrix_ui::Rgb, dark: wlrix_ui::Rgb| {
        format!(
            "inset 2px 0 0 0 {}, inset -2px 0 0 0 {},\n\x20             inset 0 2px 0 0 {}, inset 0 -2px 0 0 {}",
            c(light),
            c(dark),
            c(light),
            c(dark)
        )
    };

    let _ = write!(
        css,
        "/* The titlebar row. Not a border: a CSS border mitres its corners at 45 degrees and a\n\
         \x20  Motif bevel does not. Not a border-image either -- GTK 3 parses\n\
         \x20  `border-image-slice: 2 fill` and ignores the `fill`, leaving the middle unpainted,\n\
         \x20  which under a compositor is black. Four inset shadows draw it exactly and take\n\
         \x20  their colors from the scheme with no asset at all. */\n\
         headerbar, .titlebar {{\n\
         \x20 background-color: @wlrix_title_active;\n\
         \x20 color: @wlrix_title_active_text;\n\
         \x20 box-shadow: {};\n\
         }}\n\n\
         headerbar:backdrop, .titlebar:backdrop {{\n\
         \x20 background-color: @wlrix_title_inactive;\n\
         \x20 color: @wlrix_title_inactive_text;\n\
         \x20 box-shadow: {};\n\
         }}\n\n",
        bevel(
            palette.title_active_top_shadow,
            palette.title_active_bottom_shadow
        ),
        bevel(
            palette.title_inactive_top_shadow,
            palette.title_inactive_bottom_shadow
        ),
    );

    // `menu` is the window-menu button, which is where IRIX kept Close -- there is no close
    // button on the bar itself, and `gtk-decoration-layout` asks for the same three the frame
    // has. GTK only materializes it for an application that has an app menu, and GTK 4 not at
    // all, so it is styled in case it appears rather than in expectation of it.
    for (css_class, asset) in [
        ("menu", "menu"),
        ("minimize", "minimize"),
        ("maximize", "maximize"),
        ("close", "close"),
    ] {
        for (state, asset_state) in [
            ("", "active"),
            (":active", "active-pressed"),
            (":backdrop", "backdrop"),
            (":backdrop:active", "backdrop-pressed"),
        ] {
            let _ = write!(
                css,
                "{} {{\n\x20 background-image: {};\n}}\n",
                gtk.button(css_class, state, ""),
                url(&format!("{asset}-{asset_state}")),
            );
        }
        css.push('\n');
    }

    // Maximized: the box's shadow flips to up-left, which is IRIX's way of drawing a toggle
    // that is in. The `.maximized` class is on the window, so this has to outrank the rules
    // above by specificity rather than by order.
    css.push_str("/* While the window is maximized. */\n");
    for (state, asset_state) in [
        ("", "active"),
        (":active", "active-pressed"),
        (":backdrop", "backdrop"),
        (":backdrop:active", "backdrop-pressed"),
    ] {
        let _ = write!(
            css,
            "{} {{\n\x20 background-image: {};\n}}\n",
            gtk.button("maximize", state, ".maximized "),
            url(&format!("maximized-{asset_state}")),
        );
    }

    css
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classic() -> &'static Palette {
        wlrix_ui::palette::by_id("classic").expect("classic ships")
    }

    #[test]
    fn no_asset_can_break_its_own_xml() {
        // A double hyphen inside an XML comment is a hard parse error, and an SVG loader is a
        // strict XML parser -- so an asset with one does not render wrong, it does not render at
        // all. Caught the first time by rendering the output rather than reading it, which is
        // the only way this shows up.
        for palette in wlrix_ui::palette::ALL {
            for (name, svg) in assets_for(palette) {
                let mut rest = svg.as_str();
                while let Some(open) = rest.find("<!--") {
                    let body = &rest[open + 4..];
                    let close = body.find("-->").expect("an unterminated comment");
                    assert!(
                        !body[..close].contains("--"),
                        "{}/{name}: `--` inside an XML comment",
                        palette.id
                    );
                    rest = &body[close + 3..];
                }
            }
        }
    }

    #[test]
    fn a_byte_survives_the_round_trip_through_f32() {
        // The whole export rests on this: the frame holds colors as f32 and CSS wants bytes.
        // If the conversion were lossy the assets would be a shade off the compositor's own
        // chrome, which is exactly the drift nobody would notice until the two sat side by side.
        for v in 0u8..=255 {
            let rgb = wlrix_ui::Rgb::from_channels(v, v, v);
            let back = hex(Color32F::from(rgb.to_f32_array()));
            assert_eq!(back, format!("#{v:02x}{v:02x}{v:02x}"), "{v}");
        }
    }

    #[test]
    fn a_button_asset_is_the_frames_own_quads_reversed() {
        // The reason `svg` takes a quad list rather than doing its own drawing: this can assert
        // the asset *is* the frame, rather than looking like it.
        let style = titlebar_style();
        let client = sample_client(decoration::BUTTON_W * 8);
        let (minimize, _) = decoration::right_buttons(client, style);
        let area = minimize.expect("minimize is enabled");

        let quads = decoration::decoration_quads(classic(), client, style, true, false, None);
        let cropped = crop(&quads, area);
        let svg = svg(area.size, &cropped);

        // One <rect> per quad, and the first one drawn is the last one the frame listed.
        assert_eq!(svg.matches("<rect").count(), cropped.len());
        let first = cropped.last().expect("the button has quads");
        assert!(
            svg.contains(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
                first.0.loc.x,
                first.0.loc.y,
                first.0.size.w,
                first.0.size.h,
                hex(first.1)
            )),
            "{svg}"
        );
    }

    #[test]
    fn a_button_asset_is_exactly_the_button() {
        // A crop that let a neighbouring piece in would put a sliver of the title run down the
        // side of every button, which reads as a seam rather than as a mistake.
        for (part, maximized) in [
            (FramePart::MenuButton, false),
            (FramePart::MinimizeButton, false),
            (FramePart::MaximizeButton, false),
            (FramePart::MaximizeButton, true),
        ] {
            let svg = button_svg(classic(), part, true, maximized, false, true);
            assert!(
                svg.contains(&format!(
                    "width=\"{}\" height=\"{}\"",
                    decoration::BUTTON_W,
                    decoration::TITLEBAR_HEIGHT
                )),
                "{part:?}: {svg}"
            );
            assert!(svg.contains("<rect"), "{part:?} drew nothing");
        }
    }

    #[test]
    fn the_close_asset_is_a_button_with_no_glyph() {
        // Close is the one button wlRIX has no symbol for. Its asset has to be the face alone,
        // because GTK draws its own symbol on top -- and the face has to be a real button's, or
        // it would not sit flush beside the ones that do have glyphs.
        let outline = hex(Color32F::from(classic().outer_line.to_f32_array()));
        let face = button_svg(
            classic(),
            FramePart::MinimizeButton,
            true,
            false,
            false,
            false,
        );
        let glyphed = button_svg(
            classic(),
            FramePart::MinimizeButton,
            true,
            false,
            false,
            true,
        );

        assert!(
            !face.contains(&outline),
            "the face still has a glyph: {face}"
        );
        assert!(glyphed.contains(&outline), "the glyphed one lost its glyph");
        // Same bevel, so the two sit flush: every rect of the face is in the glyphed one too.
        for rect in face.lines().filter(|l| l.trim_start().starts_with("<rect")) {
            assert!(
                glyphed.contains(rect.trim()),
                "{rect} is not the button's own"
            );
        }
    }

    #[test]
    fn a_pressed_button_is_not_the_same_as_a_released_one() {
        // The pressed face is a different role (`*_armed`) and the bevel inverts. If `pressed`
        // were not being threaded through, both assets would come out identical and the theme
        // would simply never respond to a click.
        let released = button_svg(
            classic(),
            FramePart::MinimizeButton,
            true,
            false,
            false,
            true,
        );
        let pressed = button_svg(
            classic(),
            FramePart::MinimizeButton,
            true,
            false,
            true,
            true,
        );
        assert_ne!(released, pressed);
    }

    #[test]
    fn an_inactive_button_is_not_the_same_as_an_active_one() {
        let active = button_svg(classic(), FramePart::MenuButton, true, false, false, true);
        let backdrop = button_svg(classic(), FramePart::MenuButton, false, false, false, true);
        assert_ne!(active, backdrop);
    }

    #[test]
    fn the_maximized_glyph_differs_from_the_maximize_one() {
        // IRIX shifts the box a pixel and flips its shadow up-left. Two assets that came out
        // the same would mean the maximized state was being asked for and ignored.
        let normal = button_svg(
            classic(),
            FramePart::MaximizeButton,
            true,
            false,
            false,
            true,
        );
        let maximized = button_svg(
            classic(),
            FramePart::MaximizeButton,
            true,
            true,
            false,
            true,
        );
        assert_ne!(normal, maximized);
    }

    #[test]
    fn the_bar_draws_its_bevel_the_way_the_frame_does() {
        // Run::Vertical: the top and bottom shadows run the full width and the left and right
        // pair is inset between them. In CSS that is the *order* of the inset shadows -- a later
        // one paints over an earlier one -- so getting it backwards would put the light tone in
        // the bottom-left corner, which is what a sunken bevel looks like.
        let css = scheme_css(classic(), Gtk::V3);
        // The declaration wraps across two lines, so the whole sheet is the haystack.
        let shadow = &css[css.find("box-shadow:").expect("the bar has a bevel")..];
        let shadow = &shadow[..shadow.find(';').expect("a declaration ends")];
        let light = hex(Color32F::from(
            classic().title_active_top_shadow.to_f32_array(),
        ));
        let dark = hex(Color32F::from(
            classic().title_active_bottom_shadow.to_f32_array(),
        ));

        // The horizontal pair first, so the vertical pair covers it at the corners.
        assert!(
            shadow.contains(&format!("inset 2px 0 0 0 {light}")),
            "{shadow}"
        );
        assert!(
            shadow.find("inset 2px 0").unwrap() < shadow.find("inset 0 2px").unwrap(),
            "the full-width pair has to come last: {shadow}"
        );
        assert!(css.contains(&format!("inset 0 -2px 0 0 {dark}")), "{css}");
    }

    #[test]
    fn every_scheme_gets_the_full_set_of_assets() {
        for palette in wlrix_ui::palette::ALL {
            let assets = assets_for(palette);
            assert_eq!(assets.len(), 20, "{}", palette.id);
            let names: std::collections::BTreeSet<_> =
                assets.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(
                names.len(),
                assets.len(),
                "{}: a name is used twice",
                palette.id
            );
            for wanted in [
                "menu-active",
                "minimize-backdrop-pressed",
                "maximized-active",
            ] {
                assert!(names.contains(wanted), "{}: no {wanted}", palette.id);
            }
        }
    }

    #[test]
    fn each_generation_spells_a_button_the_way_it_names_it() {
        // GTK 4 has no `titlebutton` class -- its window controls are `windowcontrols >
        // button.minimize`. A sheet written with GTK 3's names matches nothing there, and a CSS
        // selector that matches nothing is not an error, so the buttons simply stayed Adwaita's
        // while everything around them was right. Read off a live widget tree; asserted here so
        // it cannot quietly go back.
        let v3 = scheme_css(classic(), Gtk::V3);
        assert!(v3.contains("headerbar button.titlebutton.minimize"), "{v3}");
        assert!(!v3.contains("windowcontrols"), "{v3}");

        let v4 = scheme_css(classic(), Gtk::V4);
        assert!(v4.contains("windowcontrols > button.minimize"), "{v4}");
        assert!(!v4.contains("titlebutton"), "{v4}");

        // Both still reach every button and every state.
        for css in [&v3, &v4] {
            for class in ["menu", "minimize", "maximize", "close"] {
                assert!(css.contains(class), "no rules for {class}");
            }
            for state in [":active", ":backdrop", ":backdrop:active"] {
                assert!(css.contains(state), "no {state} rules");
            }
            assert!(css.contains(".maximized "), "no maximized override");
        }
    }

    #[test]
    fn a_scheme_sheet_names_only_its_own_assets() {
        // Every url() has the scheme's own directory in it. A copied path would give one scheme
        // another's buttons, which looks like a palette bug rather than a typo.
        for palette in wlrix_ui::palette::ALL {
            for gtk in [Gtk::V3, Gtk::V4] {
                let css = scheme_css(palette, gtk);
                for other in wlrix_ui::palette::ALL {
                    let looked_for = format!("assets/{}/", other.id);
                    assert_eq!(
                        css.contains(&looked_for),
                        other.id == palette.id,
                        "{} ({}) refers to {}",
                        palette.id,
                        gtk.dir(),
                        other.id
                    );
                }
            }
        }
    }

    #[test]
    fn each_gtk_generation_gets_the_names_it_reads() {
        // GTK 3 apps read Adwaita's family and GTK 4 apps read libadwaita's. Emitting one set to
        // both would leave half the desktop unstyled, and neither GTK warns about a color
        // nobody defined.
        let v3 = scheme_css(classic(), Gtk::V3);
        assert!(v3.contains("@define-color theme_bg_color"));
        assert!(!v3.contains("@define-color window_bg_color"));

        let v4 = scheme_css(classic(), Gtk::V4);
        assert!(v4.contains("@define-color window_bg_color"));
        assert!(!v4.contains("@define-color theme_bg_color"));

        // The frame's own names are in both: the structural sheet reads them either way.
        for css in [&v3, &v4] {
            assert!(css.contains("@define-color wlrix_title_active "));
            assert!(css.contains("@define-color wlrix_title_inactive_text "));
        }
    }
}
