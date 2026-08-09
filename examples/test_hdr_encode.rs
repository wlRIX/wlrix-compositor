// SPDX-License-Identifier: GPL-3.0-or-later
//! Checks the HDR encode pass on real hardware, without needing a TTY.
//!
//!     cargo run --example test_hdr_encode -- /dev/dri/renderD128
//!
//! The encode is the half of HDR that cannot be verified by reading the DRM properties: the
//! panel can be in PQ mode and the picture still be wrong. This runs the real shader on the real
//! GPU and checks the numbers.
//!
//! For the given render node it:
//!
//! 1. compiles the encode shader -- all three variants the renderer builds, which is where a
//!    GLSL ES 1.00 mistake shows up;
//! 2. allocates the offscreen and reports which format survived, so an FP16 fallback is visible
//!    here rather than as banding on a monitor;
//! 3. fills that offscreen with known sRGB values, runs them through the encode, reads the
//!    result back, and compares against ST 2084 computed on the CPU.
//!
//! Step 3 is the point. sRGB white at 203 cd/m² reference has to come out at ~58% -- the
//! BT.2408 reference-white figure. If the transfer function is wrong, or the matrix is
//! transposed, or precision collapsed to mediump, the numbers move and this says so.
//!
//! Not part of the compositor; a dev tool only.

use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::{
    allocator::Fourcc,
    egl::EGLDisplay,
    renderer::{
        Bind, Color32F, ExportMem, Frame, ImportMem, Offscreen, Renderer,
        damage::OutputDamageTracker,
        element::{Id, Kind, texture::TextureRenderElement},
        gles::GlesRenderer,
    },
};
use smithay::utils::{DeviceFd, Rectangle, Size, Transform};
use std::fs::OpenOptions;

// Included rather than linked -- this is a binary crate, so the module is compiled a second
// time here. The example exercises the encode path, not the wrapper types, hence the allow.
#[path = "../src/hdr_render.rs"]
#[allow(dead_code)]
mod hdr_render;

/// Reference white the encode is asked for, in cd/m². Matches the compositor's default.
const SDR_WHITE: f32 = 203.0;

/// ST 2084 inverse EOTF on the CPU, to compare the GPU against.
///
/// `f64` deliberately: this is the reference the shader is judged against, so it should not
/// share the shader's precision. The constants are the spec's, written out in full rather than
/// rounded, so they can be checked against ST 2084 by eye.
fn pq_encode(linear: f64) -> f64 {
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;
    let y = linear.clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// The sRGB EOTF, piecewise as the shader has it.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    let node = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: test_hdr_encode /dev/dri/renderD128");
        std::process::exit(2);
    });

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .unwrap_or_else(|err| panic!("could not open {node}: {err}"));
    let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(
        std::os::fd::OwnedFd::from(file),
    )))
    .expect("gbm device");
    let display = unsafe { EGLDisplay::new(gbm) }.expect("egl display");
    let context = smithay::backend::egl::EGLContext::new(&display).expect("egl context");
    let mut renderer = unsafe { GlesRenderer::new(context) }.expect("gles renderer");

    // 1. The shader. All three variants (plain, NO_ALPHA, EXTERNAL) plus their debug builds are
    //    compiled inside this call, so a failure here is a real GLSL problem, not a config one.
    let encoder =
        hdr_render::ColorPipeline::new(&mut renderer).expect("the colour shaders must compile");
    println!("shader:    compiled");

    // 2. The offscreen. The format it settles on is what the compositor would use.
    let size: Size<i32, smithay::utils::Buffer> = (64, 64).into();
    let mut target = hdr_render::Target::new(&mut renderer, size).expect("offscreen must allocate");
    println!(
        "offscreen: allocated at {}x{}",
        target.size.w, target.size.h
    );

    // 3. Encode known values and read them back. Each patch is a flat sRGB gray filled into the
    //    offscreen, encoded, and compared with the CPU's answer for the same input.
    let mut failures = 0;
    for srgb in [0.0f64, 0.18, 0.5, 1.0] {
        let measured = f64::from(encode_one(
            &mut renderer,
            &encoder,
            &mut target,
            size,
            srgb as f32,
        ));
        let expected = pq_encode(srgb_to_linear(srgb) * f64::from(SDR_WHITE) / 10_000.0);

        // The readback is 8-bit, so a step is 1/255 -- allow a couple, plus room for the
        // BT.709 -> BT.2020 matrix, which is not exactly identity even on a neutral.
        let tolerance = 3.0 / 255.0;
        let ok = (measured - expected).abs() <= tolerance;
        println!(
            "sRGB {srgb:>4} -> {:>7.2} cd/m^2: PQ measured {measured:.4}, expected {expected:.4}  {}",
            srgb_to_linear(srgb) * f64::from(SDR_WHITE),
            if ok { "ok" } else { "MISMATCH" }
        );
        if !ok {
            failures += 1;
        }
    }

    // 4. The round trip. A client that tags its surface as PQ has its content decoded into the
    //    working space and then encoded again on the way out. Those two conversions are supposed
    //    to be exact inverses, so a PQ code value must come back as itself -- otherwise HDR
    //    content is being quietly re-graded on its way through the compositor.
    println!();
    for pq in [0.0f32, 0.25, 0.5081, 0.75, 1.0] {
        let measured = round_trip(&mut renderer, &encoder, &mut target, size, pq);
        // Two 8-bit quantisations (the fill and the read-back) plus the matrix pair.
        let tolerance = 4.0 / 255.0;
        let ok = (measured - pq).abs() <= tolerance;
        println!(
            "PQ {pq:>6.4} -> decode -> encode -> {measured:.4}  {}",
            if ok { "ok" } else { "MISMATCH" }
        );
        if !ok {
            failures += 1;
        }
    }

    // 5. Tone mapping, the path a PQ surface takes when it is on an SDR output. Below the knee
    //    the curve must be the identity -- ordinary content is not allowed to be dimmed just
    //    because the format it arrived in can express highlights.
    println!();
    for nits in [20.0f64, 100.0, 203.0, 1000.0] {
        let measured = tonemapped(&mut renderer, &encoder, &mut target, size, nits);
        let expected = tonemap_reference(nits);
        // 8-bit in and out, plus the BT.2020 -> BT.709 round trip on a neutral.
        let ok = (measured - expected).abs() <= 4.0 / 255.0;
        println!(
            "{nits:>7.0} cd/m^2 -> tone map -> sRGB {measured:.4}, expected {expected:.4}  {}",
            if ok { "ok" } else { "MISMATCH" }
        );
        if !ok {
            failures += 1;
        }
    }

    // 6. Blending. Half-transparent white over black is the cleanest way to tell the two
    //    working spaces apart: composited in linear light it lands at half the *light*, and in
    //    the encoded space at half the *code value*, which are nowhere near each other. Both are
    //    checked, so this pins down which space is actually in use rather than just that some
    //    blending happened.
    println!();
    for space in [
        hdr_render::WorkingSpace::Linear,
        hdr_render::WorkingSpace::Encoded,
    ] {
        let measured = blend_half_white(&mut renderer, &encoder, &mut target, size, space);
        // In linear light the result is 0.5 of SDR white. In the encoded space the blend
        // happens on sRGB code values, so 0.5 code decodes to 0.214 of SDR white.
        let expected_linear = 0.5;
        let expected_encoded = srgb_to_linear(0.5);
        let expected = match space {
            hdr_render::WorkingSpace::Linear => expected_linear,
            hdr_render::WorkingSpace::Encoded => expected_encoded,
        };
        let measured_fraction = pq_decode(measured) * 10_000.0 / f64::from(SDR_WHITE);
        let ok = (measured_fraction - expected).abs() <= 0.03;
        println!(
            "{space:?}: 50% white over black -> {measured_fraction:.4} of SDR white, expected {expected:.4}  {}",
            if ok { "ok" } else { "MISMATCH" }
        );
        if !ok {
            failures += 1;
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} check(s) did not match: the colour pipeline is wrong");
        std::process::exit(1);
    }
    println!("\nencode, PQ round trip, tone map and both blend spaces check out on this GPU");
}

/// Fill the offscreen with a flat sRGB gray, encode it, and read back the top-left pixel.
fn encode_one(
    renderer: &mut GlesRenderer,
    encoder: &hdr_render::ColorPipeline,
    target: &mut hdr_render::Target,
    size: Size<i32, smithay::utils::Buffer>,
    srgb: f32,
) -> f32 {
    let physical: Size<i32, smithay::utils::Physical> = (size.w, size.h).into();

    // The desktop pass, reduced to a flat fill: this is what `output_elements` would have left
    // in the offscreen, in the same (sRGB-encoded) space.
    {
        let mut framebuffer = renderer.bind(target.texture()).expect("bind offscreen");
        let mut frame = renderer
            .render(&mut framebuffer, physical, Transform::Normal)
            .expect("render into offscreen");
        frame
            .clear(
                Color32F::new(srgb, srgb, srgb, 1.0),
                &[Rectangle::from_size(physical)],
            )
            .expect("clear");
        let _ = frame.finish().expect("finish offscreen");
    }

    // The encode pass, into an 8-bit buffer so it can be read back directly. The compositor
    // scans out 10-bit; 8 is plenty to tell a correct curve from a wrong one.
    let mut scanout: smithay::backend::renderer::gles::GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("scanout buffer");
    let element = encoder.element(
        renderer,
        target,
        SDR_WHITE,
        hdr_render::WorkingSpace::Encoded,
    );
    let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
    let mut framebuffer = renderer.bind(&mut scanout).expect("bind scanout");
    tracker
        .render_output(renderer, &mut framebuffer, 0, &[element], Color32F::BLACK)
        .expect("encode pass");

    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((1, 1).into()),
            Fourcc::Abgr8888,
        )
        .expect("read back");
    let pixels = renderer.map_texture(&mapping).expect("map read-back");
    // Red channel; the input is neutral, so all three agree to within the matrix.
    f32::from(pixels[0]) / 255.0
}

/// Fill the offscreen with a flat PQ code value *through the decode shader*, then run the encode
/// pass over it, and read back what came out.
///
/// This is the path a PQ-tagged client surface takes. The fill has to go through the decode
/// shader rather than being written directly, because that is what the compositor does: the
/// offscreen holds working-space values, not PQ ones.
fn round_trip(
    renderer: &mut GlesRenderer,
    encoder: &hdr_render::ColorPipeline,
    target: &mut hdr_render::Target,
    size: Size<i32, smithay::utils::Buffer>,
    pq: f32,
) -> f32 {
    let physical: Size<i32, smithay::utils::Physical> = (size.w, size.h).into();

    // A 1x1 source holding the PQ code value, standing in for the client's buffer.
    let source_pixels = [
        (pq * 255.0).round() as u8,
        (pq * 255.0).round() as u8,
        (pq * 255.0).round() as u8,
        255u8,
    ];
    let source = renderer
        .import_memory(&source_pixels, Fourcc::Abgr8888, (1, 1).into(), false)
        .expect("import the PQ source");

    // Decode it into the offscreen, exactly as a tagged surface would be drawn.
    {
        let element = TextureRenderElement::from_static_texture(
            Id::new(),
            renderer.context_id(),
            (0.0, 0.0),
            source,
            1,
            Transform::Normal,
            None,
            None,
            Some((size.w, size.h).into()),
            None,
            Kind::Unspecified,
        );
        let decoded = encoder.decoded(element, SDR_WHITE, hdr_render::WorkingSpace::Encoded);
        let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
        let mut framebuffer = renderer.bind(target.texture()).expect("bind offscreen");
        tracker
            .render_output(renderer, &mut framebuffer, 0, &[decoded], Color32F::BLACK)
            .expect("decode pass");
    }

    // And encode it back out.
    let mut scanout: smithay::backend::renderer::gles::GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("scanout buffer");
    let element = encoder.element(
        renderer,
        target,
        SDR_WHITE,
        hdr_render::WorkingSpace::Encoded,
    );
    let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
    let mut framebuffer = renderer.bind(&mut scanout).expect("bind scanout");
    tracker
        .render_output(renderer, &mut framebuffer, 0, &[element], Color32F::BLACK)
        .expect("encode pass");

    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((1, 1).into()),
            Fourcc::Abgr8888,
        )
        .expect("read back");
    let pixels = renderer.map_texture(&mapping).expect("map read-back");
    f32::from(pixels[0]) / 255.0
}

/// The tone map computed on the CPU: the same knee, applied to luminance, then sRGB.
fn tonemap_reference(nits: f64) -> f64 {
    let k = f64::from(hdr_render::TONEMAP_KNEE);
    let x = nits / f64::from(SDR_WHITE);
    // A neutral gray has luminance equal to its channel value whatever the primaries, so the
    // luminance-scaling step is the identity here and the knee applies directly.
    let y = if x <= k {
        x
    } else {
        k + (x - k) / (1.0 + (x - k) / (1.0 - k))
    };
    let y = y.clamp(0.0, 1.0);
    if y <= 0.0031308 {
        12.92 * y
    } else {
        1.055 * y.powf(1.0 / 2.4) - 0.055
    }
}

/// Draw a flat PQ patch of a given absolute luminance through the tone map, and read it back.
fn tonemapped(
    renderer: &mut GlesRenderer,
    encoder: &hdr_render::ColorPipeline,
    target: &mut hdr_render::Target,
    size: Size<i32, smithay::utils::Buffer>,
    nits: f64,
) -> f64 {
    let physical: Size<i32, smithay::utils::Physical> = (size.w, size.h).into();

    // The PQ code value for this luminance, as a client's buffer would carry it.
    let code = pq_encode(nits / 10_000.0);
    let byte = (code * 255.0).round() as u8;
    let source = renderer
        .import_memory(
            &[byte, byte, byte, 255],
            Fourcc::Abgr8888,
            (1, 1).into(),
            false,
        )
        .expect("import the PQ source");

    let element = TextureRenderElement::from_static_texture(
        Id::new(),
        renderer.context_id(),
        (0.0, 0.0),
        source,
        1,
        Transform::Normal,
        None,
        None,
        Some((size.w, size.h).into()),
        None,
        Kind::Unspecified,
    );
    // Straight into an 8-bit buffer: on an SDR output there is no offscreen, the tone map runs
    // as the surface is drawn.
    let mut scanout: smithay::backend::renderer::gles::GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("scanout buffer");
    let mapped = encoder.tonemapped(element, SDR_WHITE);
    let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
    let mut framebuffer = renderer.bind(&mut scanout).expect("bind scanout");
    tracker
        .render_output(renderer, &mut framebuffer, 0, &[mapped], Color32F::BLACK)
        .expect("tone map pass");
    let _ = target;

    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((1, 1).into()),
            Fourcc::Abgr8888,
        )
        .expect("read back");
    let pixels = renderer.map_texture(&mapping).expect("map read-back");
    f64::from(pixels[0]) / 255.0
}

/// ST 2084 EOTF on the CPU, to turn a measured code value back into light.
fn pq_decode(code: f64) -> f64 {
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;
    let e = code.clamp(0.0, 1.0).powf(1.0 / M2);
    ((e - C1).max(0.0) / (C2 - C3 * e)).powf(1.0 / M1)
}

/// Composite a half-transparent white solid over an opaque black one, encode, and read back.
///
/// This is the desktop pass in miniature: two elements, alpha between them, in whichever working
/// space is being tested.
fn blend_half_white(
    renderer: &mut GlesRenderer,
    encoder: &hdr_render::ColorPipeline,
    target: &mut hdr_render::Target,
    size: Size<i32, smithay::utils::Buffer>,
    space: hdr_render::WorkingSpace,
) -> f64 {
    use smithay::backend::renderer::element::solid::SolidColorRenderElement;
    let physical: Size<i32, smithay::utils::Physical> = (size.w, size.h).into();
    let full = Rectangle::from_size(physical);

    // Front to back, as the compositor orders them: the translucent white sits over the black.
    let white = SolidColorRenderElement::new(
        Id::new(),
        full,
        smithay::backend::renderer::utils::CommitCounter::default(),
        hdr_render::ColorPipeline::to_working(Color32F::new(0.5, 0.5, 0.5, 0.5), space),
        Kind::Unspecified,
    );
    let black = SolidColorRenderElement::new(
        Id::new(),
        full,
        smithay::backend::renderer::utils::CommitCounter::default(),
        Color32F::new(0.0, 0.0, 0.0, 1.0),
        Kind::Unspecified,
    );
    let elements = [encoder.plain(white, space), encoder.plain(black, space)];

    {
        let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
        let mut framebuffer = renderer.bind(target.texture()).expect("bind offscreen");
        tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                hdr_render::ColorPipeline::to_working(Color32F::BLACK, space),
            )
            .expect("blend pass");
    }

    let mut scanout: smithay::backend::renderer::gles::GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .expect("scanout buffer");
    let element = encoder.element(renderer, target, SDR_WHITE, space);
    let mut tracker = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
    let mut framebuffer = renderer.bind(&mut scanout).expect("bind scanout");
    tracker
        .render_output(renderer, &mut framebuffer, 0, &[element], Color32F::BLACK)
        .expect("encode pass");

    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((1, 1).into()),
            Fourcc::Abgr8888,
        )
        .expect("read back");
    let pixels = renderer.map_texture(&mapping).expect("map read-back");
    f64::from(pixels[0]) / 255.0
}
