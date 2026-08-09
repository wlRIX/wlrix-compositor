// SPDX-License-Identifier: GPL-3.0-or-later
//! Rasterizing window-title text for the server-side titlebars.
//!
//! cosmic-text gives shaping and system-font fallback, so CJK and RTL titles render (unlike a
//! single bundled Latin font). One line is drawn into an RGBA buffer, cached per
//! (title, size, color) so a still titlebar is not re-rasterized every frame.

use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{Color32F, element::memory::MemoryRenderBuffer},
    },
    utils::Transform,
};

/// Scale one color channel by coverage, for a pre-multiplied buffer.
///
/// The invariant that matters is `premultiply(c, a) <= a`: a channel may never exceed its own
/// alpha. Everything downstream divides by alpha somewhere -- the HDR linearise pass most
/// sharply, since it follows the divide with a 2.4 power -- and a channel above its alpha comes
/// back as a value above 1, which shows up as a bright speck on the outline of every glyph.
fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u16::from(channel) * u16::from(alpha)) / 255) as u8
}

/// Title-text size in logical pixels (matched against the 30px IRIX titlebar).
pub const TITLE_PX: f32 = 15.0;

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
    fonts: FontSystem,
    swash: SwashCache,
    cache: HashMap<Key, Rasterized>,
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            cache: HashMap::new(),
        }
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

        let metrics = Metrics::new(px, px * 1.3);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = Attrs::new().weight(Weight::BOLD).family(Family::SansSerif);
        buffer.set_text(&mut self.fonts, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.fonts, false);

        // The first (and only) shaped line's advance width, and the line box height.
        let width = buffer
            .layout_runs()
            .next()
            .map(|run| run.line_w.ceil() as i32)
            .unwrap_or(0)
            .max(1);
        let height = metrics.line_height.ceil() as i32;

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let text_color = Color::rgba(rgb[0], rgb[1], rgb[2], 255);
        buffer.draw(
            &mut self.fonts,
            &mut self.swash,
            text_color,
            |x, y, w, h, color| {
                let alpha = color.a();
                if alpha == 0 {
                    return;
                }
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        let (px, py) = (x + dx, y + dy);
                        if px < 0 || py < 0 || px >= width || py >= height {
                            continue;
                        }
                        let idx = ((py * width + px) * 4) as usize;
                        // Pre-multiplied, because that is what `Abgr8888` means to everything
                        // downstream: smithay blends with `ONE, ONE_MINUS_SRC_ALPHA`. Writing
                        // the color straight -- as this did -- overshoots at every partially
                        // covered pixel, which is the whole outline of every glyph. On an SDR
                        // output that is a mild halo, invisible for near-black text. Through the
                        // HDR path it is not mild: the linearize step divides by this alpha and
                        // then applies a 2.4 power, turning a small overshoot into a bright
                        // speck -- 129x too bright at 10% coverage for the inactive titlebar's
                        // gray, and that is what "white dotting around text" was.
                        pixels[idx] = premultiply(color.r(), alpha);
                        pixels[idx + 1] = premultiply(color.g(), alpha);
                        pixels[idx + 2] = premultiply(color.b(), alpha);
                        pixels[idx + 3] = alpha;
                    }
                }
            },
        );

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
}

#[cfg(test)]
mod tests {
    use super::premultiply;

    /// The invariant a straight-alpha buffer violated: no channel may exceed its own alpha.
    ///
    /// Checked exhaustively because it is cheap and because the failure it guards against was
    /// invisible on an SDR output and glaring through the HDR path.
    #[test]
    fn a_channel_never_exceeds_its_alpha() {
        for channel in 0..=255u8 {
            for alpha in 0..=255u8 {
                let premultiplied = premultiply(channel, alpha);
                assert!(
                    premultiplied <= alpha,
                    "{channel} at alpha {alpha} gave {premultiplied}"
                );
            }
        }
    }

    #[test]
    fn the_ends_of_the_range_are_exact() {
        // Fully covered keeps the color, uncovered keeps nothing, and opaque black stays black.
        assert_eq!(premultiply(200, 255), 200);
        assert_eq!(premultiply(200, 0), 0);
        assert_eq!(premultiply(0, 255), 0);
        // Half coverage halves the channel, which is what the blend then adds to the background.
        assert_eq!(premultiply(200, 128), 100);
    }
}
