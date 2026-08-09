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
//! ## The working space, and what is still deferred
//!
//! The offscreen holds **extended sRGB in BT.709 primaries, with 1.0 meaning SDR white** -- the
//! encoding the desktop already draws in, widened rather than replaced. FP16 is what makes that
//! possible: values above 1 (a highlight brighter than the desktop's white) and below 0 (a
//! BT.2020 color outside the BT.709 gamut) both have somewhere to live, and both transfer
//! functions are extended through zero by odd symmetry so they survive the trip.
//!
//! Choosing that over a linear working space is deliberate. It leaves every existing element --
//! the 4Dwm chrome, the text, the solid colors -- drawn byte for byte as it is on an SDR output,
//! so turning HDR on cannot change how the desktop looks. A client that tags its surface as PQ is
//! decoded into this space by [`Decoded`] and encoded back out at the end, and because the two
//! matrices are exact inverses its pixels pass through untouched.
//!
//! What is still deferred is *blending* in linear light. Alpha compositing here happens in the
//! sRGB-encoded space, as it always has -- which is the wrong place to do it, but is also exactly
//! what an SDR output does today, so nothing regresses. Moving to a linear working space would
//! fix it and would mean converting the palette's solid colors CPU-side, since smithay's
//! solid-color shader cannot be overridden the way its texture shader can.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            element::{Element, Id, Kind, RenderElement, texture::TextureRenderElement},
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType, element::TextureShaderElement,
            },
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    utils::{
        Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform,
        user_data::UserDataMap,
    },
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

// The sRGB EOTF (IEC 61966-2-1), extended through zero by odd symmetry -- f(-x) = -f(x), the
// scRGB convention. The extension is not decoration: the working space carries BT.2020 colours
// expressed in BT.709 primaries, and those go negative outside the smaller gamut. `pow` of a
// negative is NaN, so the sign is taken off and put back.
//
// Piecewise, and the linear toe near black matters: treating it as a pure 2.2 power crushes
// exactly the range PQ then spends most of its precision on.
vec3 srgb_to_linear(vec3 c) {
    vec3 s = sign(c);
    vec3 a = abs(c);
    vec3 low = a / 12.92;
    vec3 high = pow((a + 0.055) / 1.055, vec3(2.4));
    return s * mix(low, high, step(vec3(0.04045), a));
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
    //
    // Deliberately *not* clamped to 0..1. The working space is extended sRGB: a decoded PQ
    // surface can carry values above 1 (highlights brighter than SDR white) and below 0 (a
    // BT.2020 colour outside the BT.709 gamut), and both have to survive to be put back by the
    // matrix below. `srgb_to_linear` is odd-symmetric for exactly this reason.
    vec3 linear = srgb_to_linear(color.rgb);

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

/// PQ / BT.2020 in, working space out — the inverse of the encode above.
///
/// A client that tags its surface as ST2084 through `wp_color_management_v1` hands over buffers
/// whose code values are PQ, not sRGB. Drawn with the ordinary texture shader they would be
/// treated as sRGB and encoded a second time, which looks like a washed-out, gray video. This
/// converts them into the working space the rest of the desktop is already in, so the encode pass
/// can treat every element the same.
///
/// The two matrices here and in the encode are exact inverses, so a PQ pixel that survives
/// unclipped comes back out as the same PQ pixel: HDR content passes through this compositor
/// without being re-quantized.
const DECODE_SHADER: &str = r#"#version 100

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

// Reference white for SDR content, in cd/m^2 -- what 1.0 means in the working space.
uniform float sdr_white;

// The sRGB OETF, extended through zero by odd symmetry. Inverse of the encode pass's decode.
vec3 linear_to_srgb(vec3 c) {
    vec3 s = sign(c);
    vec3 a = abs(c);
    vec3 low = a * 12.92;
    vec3 high = 1.055 * pow(a, vec3(1.0 / 2.4)) - 0.055;
    return s * mix(low, high, step(vec3(0.0031308), a));
}

// SMPTE ST 2084 EOTF: code value -> linear light, 1.0 being 10000 cd/m^2.
vec3 pq_decode(vec3 code) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 e = pow(clamp(code, 0.0, 1.0), vec3(1.0 / m2));
    vec3 num = max(e - c1, 0.0);
    vec3 den = c2 - c3 * e;
    return pow(num / den, vec3(1.0 / m1));
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    // Un-premultiply before the transfer function, which is only defined on unassociated
    // color, and re-premultiply after. Video surfaces are opaque in practice, but a client is
    // entitled to hand over an alpha channel and this is where it would go wrong.
    vec3 code = color.a > 0.0 ? color.rgb / color.a : color.rgb;

    // PQ is absolute: 1.0 is 10000 cd/m^2 regardless of anything. Divide by the reference white
    // to land in the working space, where 1.0 is the desktop's white -- so a 203-nit highlight
    // in the video matches the desktop's white, and a 1000-nit one sits at 4.9.
    vec3 linear = pq_decode(code) * (10000.0 / sdr_white);

    // BT.2020 -> BT.709, the exact inverse of the encode pass's matrix. Wide-gamut colors land
    // outside 0..1 here; nothing clamps them, and the encode puts them back.
    const mat3 bt2020_to_bt709 = mat3(
        1.6604907, -0.1245499, -0.0181508,
        -0.5876410, 1.1328997, -0.1005788,
        -0.0728497, -0.0083498, 1.1187296
    );

    color = vec4(linear_to_srgb(bt2020_to_bt709 * linear) * color.a, color.a) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.3, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// The compiled shaders. One set per DRM device, since the programs belong to that device's
/// GL context.
pub struct Encoder {
    program: GlesTexProgram,
    decode: GlesTexProgram,
}

impl Encoder {
    /// Compile the encode and decode shaders, or `None` if this context cannot build them.
    ///
    /// A failure here is not fatal: the caller keeps the affected outputs in SDR, which is a
    /// working picture rather than a black screen. Both or neither -- an encode without a decode
    /// would show HDR clients wrongly rather than not at all, which is the worse failure.
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [UniformName::new(SDR_WHITE, UniformType::_1f)];
        let compile = |renderer: &mut GlesRenderer, source, what| {
            renderer
                .compile_custom_texture_shader(source, &uniforms)
                .inspect_err(|err| {
                    warn!(
                        ?err,
                        what, "could not compile an HDR shader; outputs stay SDR"
                    );
                })
                .ok()
        };
        Some(Self {
            program: compile(renderer, SHADER, "encode")?,
            decode: compile(renderer, DECODE_SHADER, "decode")?,
        })
    }

    /// Wrap an element so it is drawn through the PQ decode shader.
    ///
    /// For a surface a client has tagged as ST2084. Everything else is left alone -- the desktop
    /// is already in the working space, and wrapping it would cost a shader swap per element for
    /// no change.
    pub fn decoded<E>(&self, inner: E, sdr_white: f32) -> Decoded<E> {
        Decoded {
            inner,
            decode: Some((
                self.decode.clone(),
                vec![Uniform::new(SDR_WHITE, sdr_white)],
            )),
        }
    }

    /// Wrap an element that needs no conversion.
    pub fn plain<E>(&self, inner: E) -> Decoded<E> {
        Decoded {
            inner,
            decode: None,
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

/// An element drawn with a color-decode shader in front of it.
///
/// This is the seam that lets one desktop pass mix content in different color spaces. Smithay's
/// texture shader can be overridden per *frame*, not per element, and there is no hook between
/// elements inside `render_output` — so the override is set and cleared around the inner
/// element's own `draw`, which is the one place we are handed the frame.
///
/// Everything is wrapped, including the overwhelming majority that need no conversion, because a
/// uniform element type is what lets the desktop pass stay a single list. `decode: None` costs a
/// branch and nothing else.
#[derive(Debug)]
pub struct Decoded<E> {
    inner: E,
    decode: Option<(GlesTexProgram, Vec<Uniform<'static>>)>,
}

impl<E: Element> Element for Decoded<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn src(&self) -> Rectangle<f64, BufferCoord> {
        self.inner.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Decoded<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some((program, uniforms)) = self.decode.as_ref() else {
            return self
                .inner
                .draw(frame, src, dst, damage, opaque_regions, cache);
        };
        frame.override_default_tex_program(program.clone(), uniforms.clone());
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, cache);
        // Cleared unconditionally, including on the error path: the frame draws every remaining
        // element after this one, and leaving a PQ decode installed would run the rest of the
        // desktop through it.
        frame.clear_tex_program_override();
        result
    }

    fn underlying_storage(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
        // Deliberately not forwarded when a decode is in play. `underlying_storage` is what lets
        // the DRM compositor hand a client buffer straight to a plane, and a plane would scan out
        // the undecoded PQ buffer. The HDR path already passes `FrameFlags::empty()`, so this is
        // belt and braces -- but it is the kind of thing that gets switched back on later.
        if self.decode.is_some() {
            return None;
        }
        self.inner.underlying_storage(renderer)
    }
}
