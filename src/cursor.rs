// SPDX-License-Identifier: GPL-3.0-or-later
// Cursor theme loading follows Smithay's `anvil` example (MIT). See the NOTICE file.
//! Pointer cursor rendering.
//!
//! Two cases: a client that has called `wl_pointer.set_cursor` supplies its own surface,
//! and everywhere else (the desktop itself) we draw a themed cursor. The system XCursor
//! theme is used when one is installed; otherwise a small built-in arrow keeps the
//! pointer visible rather than invisible.

use std::{sync::Mutex, time::Duration};

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    render_elements,
    utils::{Logical, Physical, Point, Scale, Transform},
    wayland::compositor::with_states,
};
use tracing::{info, warn};
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Memory = MemoryRenderBufferRenderElement<R>,
    Surface = WaylandSurfaceRenderElement<R>,
}

/// The loaded cursor theme (or the built-in fallback).
pub struct Cursor {
    images: Vec<Image>,
    size: u32,
}

impl Cursor {
    /// Load the system cursor theme, falling back to a built-in arrow.
    pub fn load() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(24);

        let images = load_theme_images(&name).unwrap_or_else(|| {
            warn!(theme = %name, "no xcursor theme found; using built-in arrow");
            vec![fallback_arrow()]
        });

        info!(theme = %name, size, frames = images.len(), "cursor theme loaded");
        Self { images, size }
    }

    /// Pick the image for this scale, honoring animation frames.
    pub fn image(&self, scale: u32, time: Duration) -> Image {
        let target = self.size * scale;
        let nearest = self
            .images
            .iter()
            .min_by_key(|image| (target as i32 - image.size as i32).abs())
            .expect("cursor always has at least the fallback image");

        // Frames of the chosen size, cycled by their per-frame delay.
        let frames: Vec<&Image> = self
            .images
            .iter()
            .filter(|image| image.width == nearest.width && image.height == nearest.height)
            .collect();

        let total: u32 = frames.iter().map(|image| image.delay.max(1)).sum();
        if frames.len() == 1 || total == 0 {
            return frames[0].clone();
        }

        let mut position = (time.as_millis() as u32) % total;
        for frame in &frames {
            let delay = frame.delay.max(1);
            if position < delay {
                return (*frame).clone();
            }
            position -= delay;
        }
        frames[0].clone()
    }
}

fn load_theme_images(name: &str) -> Option<Vec<Image>> {
    let theme = CursorTheme::load(name);
    // "left_ptr" is the conventional default arrow; "default" is the modern alias.
    let path = theme
        .load_icon("default")
        .or_else(|| theme.load_icon("left_ptr"))?;
    let bytes = std::fs::read(path).ok()?;
    let images = parse_xcursor(&bytes)?;
    (!images.is_empty()).then_some(images)
}

/// A minimal arrow, drawn so the pointer is never invisible when no theme exists.
/// `#` is the outline, `o` the fill, space is transparent.
fn fallback_arrow() -> Image {
    const ART: &[&str] = &[
        "#           ",
        "##          ",
        "#o#         ",
        "#oo#        ",
        "#ooo#       ",
        "#oooo#      ",
        "#ooooo#     ",
        "#oooooo#    ",
        "#ooooooo#   ",
        "#oooooooo#  ",
        "#ooooo####  ",
        "#oo#oo#     ",
        "#o# #oo#    ",
        "##   #oo#   ",
        "#    #oo#   ",
        "      #oo#  ",
        "      #oo#  ",
        "       ##   ",
    ];
    let width = ART[0].len() as i32;
    let height = ART.len() as i32;

    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for row in ART {
        let mut chars = row.chars();
        for _ in 0..width {
            // RGBA, matching xcursor's `pixels_rgba`.
            match chars.next() {
                Some('#') => pixels.extend_from_slice(&[0, 0, 0, 255]),
                Some('o') => pixels.extend_from_slice(&[255, 255, 255, 255]),
                _ => pixels.extend_from_slice(&[0, 0, 0, 0]),
            }
        }
    }

    Image {
        size: height as u32,
        width: width as u32,
        height: height as u32,
        xhot: 0,
        yhot: 0,
        delay: 0,
        pixels_rgba: pixels,
        pixels_argb: Vec::new(),
    }
}

/// Turns the current [`CursorImageStatus`] into render elements at the pointer position.
pub struct PointerRenderer {
    cursor: Cursor,
    /// Cached upload of the themed image, keyed by the image it was built from.
    cached: Option<(Image, MemoryRenderBuffer)>,
}

impl PointerRenderer {
    pub fn new() -> Self {
        Self {
            cursor: Cursor::load(),
            cached: None,
        }
    }

    /// Build the cursor elements. `location` is the pointer position in physical coords.
    pub fn render<R>(
        &mut self,
        renderer: &mut R,
        status: &CursorImageStatus,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        time: Duration,
    ) -> Vec<PointerRenderElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        // `MemoryRenderBufferRenderElement::from_buffer` requires a Send texture.
        R::TextureId: Send + Clone + 'static,
    {
        match status {
            CursorImageStatus::Hidden => Vec::new(),

            // The client drew its own cursor.
            CursorImageStatus::Surface(surface) => render_elements_from_surface_tree(
                renderer,
                surface,
                location,
                scale,
                1.0,
                Kind::Cursor,
            ),

            // Everywhere else: the themed cursor.
            CursorImageStatus::Named(_) => {
                let image = self.cursor.image(scale.x.max(1.0) as u32, time);
                let buffer = self.buffer_for(image);
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    &buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                )
                .map(|element| vec![PointerRenderElement::Memory(element)])
                .unwrap_or_default()
            }
        }
    }

    /// Offset, in physical pixels, from the pointer position to the cursor image's
    /// top-left corner.
    ///
    /// A client-supplied cursor carries its own hotspot; a themed one uses the hotspot
    /// baked into the image. Using the theme's for both misplaces client cursors.
    pub fn hotspot(
        &mut self,
        status: &CursorImageStatus,
        scale: Scale<f64>,
        time: Duration,
    ) -> Point<i32, Physical> {
        match status {
            CursorImageStatus::Surface(surface) => {
                let logical: Point<i32, Logical> = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<Mutex<CursorImageAttributes>>()
                        .map(|attributes| attributes.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                logical.to_physical_precise_round(scale)
            }
            _ => {
                let image = self.cursor.image(scale.x.max(1.0) as u32, time);
                (image.xhot as i32, image.yhot as i32).into()
            }
        }
    }

    fn buffer_for(&mut self, image: Image) -> MemoryRenderBuffer {
        if let Some((cached_image, buffer)) = self.cached.as_ref()
            && cached_image == &image
        {
            return buffer.clone();
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            smithay::backend::allocator::Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        self.cached = Some((image, buffer.clone()));
        buffer
    }
}
