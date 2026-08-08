// SPDX-License-Identifier: GPL-3.0-or-later
//! The HDR encode pass: turning the composited desktop into a PQ / BT.2020 signal.
//!
//! [`crate::hdr`] is the policy -- what the panel can show and what the kernel is told. This is
//! the GLES realization of it, deliberately kept apart so the colorimetry stays renderer-agnostic
//! if a second renderer ever appears behind the same seam.
//!
//! ## Why two passes
//!
//! An HDR output cannot simply be composited into. The scanout buffer has to hold PQ-encoded
//! BT.2020, and there is nowhere in the pipeline to put that conversion: this hardware exposes
//! only `DEGAMMA_LUT` / `CTM` / `GAMMA_LUT` on the CRTC, none of which can apply an ST2084 curve.
//! So the desktop is composited into an offscreen exactly as it is today -- same blend space, same
//! elements, no visual change -- and a single full-screen shader converts that into the output's
//! space on the way to the scanout buffer.
//!
//! The cost is real and worth stating: nothing can be promoted to a DRM plane on an HDR output,
//! so direct scanout and the hardware cursor are both lost there, and the offscreen is fully
//! redrawn every frame.
//!
//! ## What this pass is *not* doing yet
//!
//! Client content is still treated as sRGB. A client that tags its surface as PQ through
//! `wp_color_management_v1` is recorded but not yet honored -- that needs per-surface decode
//! shaders and true linear compositing, which is the next milestone. What this buys now is a
//! correct signal on the wire: the panel is genuinely in HDR mode and the desktop looks the same.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            element::{Id, Kind, texture::TextureRenderElement},
            gles::{
                GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
                element::TextureShaderElement,
            },
        },
    },
    utils::{Buffer as BufferCoord, Size, Transform},
};
use tracing::{info, warn};

/// The uniform carrying SDR reference white, in cd/m².
const SDR_WHITE: &str = "sdr_white";

/// sRGB in, PQ / BT.2020 out.
///
/// Written against smithay's own `texture.frag`, which it has to match structurally -- the
/// renderer compiles this three times (plain, `NO_ALPHA`, `EXTERNAL`), each also with
/// `DEBUG_FLAGS`, so every variant has to build. Three traps, all of which cost an hour if
/// missed:
///
/// - a custom *texture* shader carries its own `#version 100` (the doc comment on
///   `compile_custom_texture_shader` describes the *pixel* shader's rule, not this one);
/// - the marker the renderer substitutes is `//_DEFINES_`, with a trailing underscore, not
///   `//_DEFINES` as its doc comment says;
/// - `precision highp float` is not optional here. PQ spends most of its range below 10% signal,
///   and at mediump the near-black end of a gradient collapses into steps.
const SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Reference white for SDR content, in cd/m^2.
uniform float sdr_white;

// The sRGB EOTF (IEC 61966-2-1). Piecewise, and the linear toe near black matters: treating it
// as a pure 2.2 power crushes exactly the range PQ then spends most of its precision on.
vec3 srgb_to_linear(vec3 c) {
    vec3 low = c / 12.92;
    vec3 high = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(low, high, step(vec3(0.04045), c));
}

// SMPTE ST 2084 inverse EOTF. Input is linear light normalized so 1.0 is 10000 cd/m^2, which is
// what PQ is defined against; output is the code value the panel decodes.
vec3 pq_encode(vec3 linear) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 y = pow(clamp(linear, 0.0, 1.0), vec3(m1));
    return pow((c1 + c2 * y) / (1.0 + c3 * y), vec3(m2));
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    // The offscreen is cleared opaque and the desktop covers it, so alpha is 1.0 across the
    // whole surface and there is nothing to un-premultiply. Said out loud because the transfer
    // functions below are only defined on unassociated color: if this element ever stops
    // being opaque, the divide has to come back.
    vec3 linear = srgb_to_linear(clamp(color.rgb, 0.0, 1.0));

    // BT.709 -> BT.2020 in linear light (ITU-R BT.2087). Column-major, as GLSL wants it.
    const mat3 bt709_to_bt2020 = mat3(
        0.6274039, 0.0690970, 0.0163914,
        0.3292830, 0.9195404, 0.0880132,
        0.0433131, 0.0113626, 0.8955953
    );

    // SDR white lands at `sdr_white` nits; everything else scales with it.
    linear = bt709_to_bt2020 * linear * (sdr_white / 10000.0);

    color = vec4(pq_encode(linear), color.a) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.3, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// The compiled encode shader. One per DRM device, since the program belongs to that device's
/// GL context.
pub struct Encoder {
    program: GlesTexProgram,
}

impl Encoder {
    /// Compile the encode shader, or `None` if this context cannot build it.
    ///
    /// A failure here is not fatal: the caller keeps the affected outputs in SDR, which is a
    /// working picture rather than a black screen.
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        match renderer
            .compile_custom_texture_shader(SHADER, &[UniformName::new(SDR_WHITE, UniformType::_1f)])
        {
            Ok(program) => Some(Self { program }),
            Err(err) => {
                warn!(
                    ?err,
                    "could not compile the HDR encode shader; outputs stay SDR"
                );
                None
            }
        }
    }

    /// The element that draws `target` into the scanout buffer, encoded.
    ///
    /// A fresh [`Id`] every frame, deliberately: the offscreen carries no damage of its own, so
    /// a stable id would let the damage tracker conclude nothing changed and skip the frame --
    /// a frozen screen. A new id is always treated as new, which costs a full repaint per frame
    /// and is the safe direction to be wrong in.
    pub fn element(
        &self,
        renderer: &GlesRenderer,
        target: &Target,
        sdr_white: f32,
    ) -> TextureShaderElement {
        use smithay::backend::renderer::Renderer as _;

        let inner = TextureRenderElement::from_static_texture(
            Id::new(),
            renderer.context_id(),
            (0.0, 0.0),
            target.texture.clone(),
            1,
            Transform::Normal,
            None,
            None,
            // The texture was allocated at the size the output scans out, and the element is
            // placed at scale 1, so its logical extent is its pixels and this lands 1:1.
            Some((target.size.w, target.size.h).into()),
            // Fully opaque: it *is* the whole screen. Saying so lets the compositor skip
            // everything the encode covers.
            Some(vec![smithay::utils::Rectangle::from_size(target.size)]),
            Kind::Unspecified,
        );
        TextureShaderElement::new(
            inner,
            self.program.clone(),
            vec![Uniform::new(SDR_WHITE, sdr_white)],
        )
    }
}

/// The offscreen the desktop is composited into before it is encoded.
pub struct Target {
    texture: GlesTexture,
    /// The size it was allocated at. A mode change makes it stale.
    pub size: Size<i32, BufferCoord>,
}

/// Offscreen formats to try, best first.
///
/// FP16 is what this wants: re-encoding 8- or 10-bit sRGB into PQ compresses the whole SDR range
/// into the lower part of a curve built for 10000 nits, and the bits lost there show up as
/// banding in dark gradients. The fallbacks are still a working picture, just a coarser one.
const CANDIDATES: &[Fourcc] = &[Fourcc::Abgr16161616f, Fourcc::Abgr2101010, Fourcc::Abgr8888];

impl Target {
    /// Allocate an offscreen for an output of this size.
    ///
    /// Each candidate is *bound* as well as created, because creating it proves less than it
    /// looks: smithay gates `create_buffer` on `Capability::_10Bit`, which it grants to any
    /// GLES 3.0 context, while RGBA16F only becomes renderable with
    /// `GL_EXT_color_buffer_half_float`. Binding is what actually asks whether the framebuffer
    /// is complete.
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, BufferCoord>) -> Option<Self> {
        for &format in CANDIDATES {
            let Ok(mut texture) = renderer.create_buffer(format, size) else {
                continue;
            };
            if renderer.bind(&mut texture).is_err() {
                continue;
            }
            if format != CANDIDATES[0] {
                warn!(
                    ?format,
                    "HDR offscreen fell back off FP16; dark gradients may band"
                );
            } else {
                info!(?format, ?size, "HDR offscreen allocated");
            }
            return Some(Self { texture, size });
        }
        warn!(
            ?size,
            "could not allocate an HDR offscreen; this output stays SDR"
        );
        None
    }

    /// The offscreen to render into.
    pub fn texture(&mut self) -> &mut GlesTexture {
        &mut self.texture
    }
}
