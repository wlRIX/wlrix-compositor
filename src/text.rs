// SPDX-License-Identifier: GPL-3.0-or-later
//! Rasterizing window-title text for the server-side titlebars.
//!
//! The shaping is [`wlrix_ui::text`], which is the same font stack the greeter and the desktop
//! draw with -- including the `/usr/share/fonts` force-load they needed and this had no
//! equivalent of, having been written as a third independent cosmic-text wrapper.
//!
//! What is left here is the part that is the compositor's own: turning a coverage bitmap into
//! an uploadable premultiplied buffer, and caching it so a still titlebar is not rasterized
//! every frame.

use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{Color32F, element::memory::MemoryRenderBuffer},
    },
    utils::Transform,
};
use wlrix_ui::text::{Face, Fonts, Raster};

/// Title-text size in logical pixels (matched against the 30px IRIX titlebar).
pub const TITLE_PX: f32 = 15.0;

/// The face all server-side chrome is drawn in: titles, menu labels, icon captions.
const CHROME_FACE: Face = Face::Bold;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    text: String,
    px: u32,
    rgb: [u8; 3],
}

/// A rasterized line of title text: the uploadable buffer and its pixel size.
#[derive(Clone)]
pub struct Rasterized {
    pub buffer: MemoryRenderBuffer,
    pub width: i32,
    pub height: i32,
}

/// Rasterizes and caches title text. Held on [`crate::Wlrix`].
pub struct TextRenderer {
    fonts: Fonts,
    /// Colored, uploadable buffers. Keyed by color as well as text, so this is what a
    /// palette change invalidates -- see [`TextRenderer::clear`].
    cache: HashMap<Key, Rasterized>,
}

impl TextRenderer {
    /// Ready the font stack, or say why not.
    ///
    /// Fallible where the old one was not: it went through `FontSystem::new()`, which cannot
    /// fail but can silently come up with nothing. A compositor whose titlebars are all blank
    /// should say so once at startup rather than leave it to be noticed.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            fonts: Fonts::load()?,
            cache: HashMap::new(),
        })
    }

    /// How wide `text` is at `px`, without rasterizing a colored buffer for it.
    ///
    /// Menu layout wants a width and nothing else. It used to get one by rasterizing at an
    /// arbitrary color and reading the width off the result, which put a measurement-only
    /// entry in the cache under whatever color happened to be passed.
    pub fn measure(&mut self, text: &str, px: f32) -> i32 {
        self.fonts.width(CHROME_FACE, px, text)
    }

    /// Rasterize `text` at `px` pixels tall in `color`, cached. `None` for blank text.
    pub fn rasterize(&mut self, text: &str, px: f32, color: Color32F) -> Option<Rasterized> {
        if text.trim().is_empty() {
            return None;
        }
        let rgb = [
            (color.r() * 255.0).round() as u8,
            (color.g() * 255.0).round() as u8,
            (color.b() * 255.0).round() as u8,
        ];
        let key = Key {
            text: text.to_string(),
            px: px.round().max(1.0) as u32,
            rgb,
        };
        if let Some(cached) = self.cache.get(&key) {
            return Some(cached.clone());
        }

        let raster: Raster = self.fonts.rasterize(CHROME_FACE, px, text)?;
        let (width, height) = (raster.width.max(1), raster.height);

        // Premultiplied, because that is what `Abgr8888` means to everything downstream:
        // smithay blends with `ONE, ONE_MINUS_SRC_ALPHA`. Writing the color straight -- as
        // this did once -- overshoots at every partially covered pixel, which is the whole
        // outline of every glyph. On an SDR output that is a mild halo, invisible for
        // near-black text. Through the HDR path it is not mild: the linearize step divides by
        // this alpha and then applies a 2.4 power, turning a small overshoot into a bright
        // speck -- 129x too bright at 10% coverage for the inactive titlebar's gray, and that
        // is what "white dotting around text" was.
        //
        // `Raster::premultiplied` holds that invariant, and `wlrix-ui` tests it exhaustively.
        let argb = raster.premultiplied(wlrix_ui::Rgb::from_channels(rgb[0], rgb[1], rgb[2]));
        // Abgr8888 in memory order is R, G, B, A byte-wise; the raster packs ARGB.
        let mut pixels = Vec::with_capacity(argb.len() * 4);
        for pixel in argb {
            let [a, r, g, b] = pixel.to_be_bytes();
            pixels.extend_from_slice(&[r, g, b, a]);
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        let rasterized = Rasterized {
            buffer,
            width,
            height,
        };
        // A window that rewrites its title constantly (a terminal showing the running command)
        // would otherwise grow the cache without bound; drop it wholesale when it gets large
        // rather than tracking per-entry age.
        if self.cache.len() >= 512 {
            self.cache.clear();
        }
        self.cache.insert(key, rasterized.clone());
        Some(rasterized)
    }

    /// Throw away every rasterized buffer.
    ///
    /// **Mandatory after a palette change.** The key includes the color, so without this
    /// every title, menu label and icon caption already on screen keeps the color it was
    /// rasterized in: the chrome around them changes and the text does not, which looks like
    /// the switch half-worked rather than like a bug.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measuring_costs_no_cache_entry() {
        // The point of `measure`: menu layout asks for widths constantly and has no color to
        // offer, and it used to leave an entry per measured string behind.
        let mut text = TextRenderer::new().expect("system fonts");
        assert!(text.measure("Close", TITLE_PX) > 0);
        assert!(text.cache.is_empty());
    }

    #[test]
    fn a_palette_change_is_what_clear_is_for() {
        let mut text = TextRenderer::new().expect("system fonts");
        let black = Color32F::from([0.0, 0.0, 0.0, 1.0]);
        let white = Color32F::from([1.0, 1.0, 1.0, 1.0]);
        assert!(text.rasterize("Close", TITLE_PX, black).is_some());
        assert_eq!(text.cache.len(), 1);
        // The same string in another color is another entry -- which is exactly why a
        // palette change has to clear, rather than trusting the key to miss.
        assert!(text.rasterize("Close", TITLE_PX, white).is_some());
        assert_eq!(text.cache.len(), 2);
        text.clear();
        assert!(text.cache.is_empty());
    }

    #[test]
    fn blank_text_rasterizes_to_nothing() {
        let mut text = TextRenderer::new().expect("system fonts");
        let black = Color32F::from([0.0, 0.0, 0.0, 1.0]);
        assert!(text.rasterize("", TITLE_PX, black).is_none());
        assert!(text.rasterize("   ", TITLE_PX, black).is_none());
    }
}
