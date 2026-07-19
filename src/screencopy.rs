// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-screencopy-v1`: letting a client read back what an output is showing.
//!
//! This is what screenshot tools speak (`grim` and friends), and what a screen-sharing
//! portal builds on. Smithay has no implementation of it, so the server side is written
//! out here.
//!
//! The exchange is: a client asks to capture an output, we tell it what buffer to
//! allocate, it hands one back, and we fill it. Only shared-memory buffers are
//! supported -- dmabuf capture would let a client import the result directly, but shm
//! is what screenshot tools use and it needs no format negotiation.
//!
//! The copy itself cannot happen here: it needs the renderer, which lives in the
//! backend. Requests are queued and drained when the backend next draws, the same way
//! output mode changes are.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId,
            protocol::{wl_buffer::WlBuffer, wl_shm},
        },
    },
    utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform},
    wayland::shm::with_buffer_contents_mut,
};
use tracing::warn;

use crate::Wlrix;

const VERSION: u32 = 3;

use crate::render::DESKTOP_BACKGROUND as CLEAR_COLOR;

/// Which way up to render a capture before reading it back.
///
/// A capture is rendered into an offscreen texture and read back with `glReadPixels`,
/// which returns rows bottom-up while the client's shm buffer is read top-down. Working
/// that through, the flip should always be needed: the projection's `flip180` puts
/// logical row zero at the top in GL terms, and the readback starts from the bottom.
///
/// That holds under the nested backend. It is measurably wrong under udev, which reads
/// back the right way round only when rendered unflipped -- so something on that path
/// applies a second flip. It is not the capture code, which is shared and renders into
/// its own offscreen texture, and it is not the renderer type, which is a plain
/// `GlesRenderer` on both. The cause is not yet understood, so each backend states what
/// it needs and this stays a known gap rather than a guess dressed up as a rule.
///
/// `WLRIX_SCREENCOPY_FLIP=0|1` overrides it, which is how the values above were found.
fn capture_transform(state: &Wlrix) -> Transform {
    match std::env::var("WLRIX_SCREENCOPY_FLIP").as_deref() {
        Ok("0") => Transform::Normal,
        Ok("1") => Transform::Flipped180,
        _ => state.capture_transform,
    }
}

/// A capture a client has asked for and handed a buffer to, waiting to be filled.
pub struct PendingCapture {
    pub frame: ZwlrScreencopyFrameV1,
    pub output: Output,
    /// The area of the output to copy, in its own pixels.
    pub region: Rectangle<i32, Physical>,
    pub buffer: WlBuffer,
    pub overlay_cursor: bool,
    /// Whether the client asked with `copy_with_damage`, which is the only case the
    /// `damage` event may be sent in.
    pub with_damage: bool,
}

/// What a frame is capturing, recorded when it is created.
pub struct FrameData {
    /// `None` when the output went away before we could describe the capture, in which
    /// case the frame is only good for reporting failure.
    capture: Option<Capture>,
    overlay_cursor: bool,
}

struct Capture {
    output: Output,
    region: Rectangle<i32, Physical>,
}

pub struct ScreencopyState;

impl ScreencopyState {
    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ZwlrScreencopyManagerV1, _>(VERSION, ())
    }
}

/// The area of `output` in its own pixels.
fn output_size(output: &Output) -> Option<Rectangle<i32, Physical>> {
    let mode = output.current_mode()?;
    Some(Rectangle::from_size(mode.size))
}

/// Tell the client what buffer to allocate for `region`.
fn offer_buffer(frame: &ZwlrScreencopyFrameV1, region: Rectangle<i32, Physical>) {
    let stride = region.size.w as u32 * 4;
    frame.buffer(
        wl_shm::Format::Xrgb8888,
        region.size.w as u32,
        region.size.h as u32,
        stride,
    );
    if frame.version() >= 3 {
        frame.buffer_done();
    }
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // The two capture requests differ only in the area they cover.
        let (frame, output, requested, overlay_cursor) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, output, None, overlay_cursor != 0),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                output,
                Some(Rectangle::new((x, y).into(), (width, height).into())),
                overlay_cursor != 0,
            ),
            _ => return,
        };

        // An output that has gone away, or has no mode, cannot be captured -- but the
        // client is told so rather than left waiting.
        let capture = Output::from_resource(&output)
            .and_then(|output| output_size(&output).map(|full| (output, full)))
            .map(|(output, full)| {
                // A requested region is clipped to the output: asking for more than
                // exists is the client's mistake, not a reason to refuse.
                let region = requested
                    .and_then(|requested| requested.intersection(full))
                    .unwrap_or(full);
                Capture { output, region }
            });

        let region = capture.as_ref().map(|capture| capture.region);
        let frame = data_init.init(
            frame,
            FrameData {
                capture,
                overlay_cursor,
            },
        );

        match region {
            Some(region) => offer_buffer(&frame, region),
            None => frame.failed(),
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Damage between captures is not tracked; a full copy is always correct, just
        // less efficient for a client polling for changes. But the `damage` event may
        // only be sent for `copy_with_damage`, so which request arrived matters.
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };

        let Some(capture) = data.capture.as_ref() else {
            frame.failed();
            return;
        };

        // The renderer lives in the backend, so the copy happens when it next draws.
        state.pending_screencopy.push(PendingCapture {
            frame: frame.clone(),
            output: capture.output.clone(),
            region: capture.region,
            buffer,
            overlay_cursor: data.overlay_cursor,
            with_damage,
        });
        state.request_redraw();
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &FrameData,
    ) {
        // Do not try to fill a buffer for a frame the client has thrown away.
        state
            .pending_screencopy
            .retain(|pending| &pending.frame != frame);
    }
}

/// Report that a capture could not be completed.
pub fn fail(capture: &PendingCapture, reason: &str) {
    warn!(reason, "screencopy failed");
    capture.frame.failed();
}

/// Fill every queued capture, then answer the clients.
///
/// Called by the backend while it has the renderer: the output is drawn once more into
/// an offscreen buffer, read back, and copied into the client's shared memory.
pub fn take_pending(state: &mut Wlrix, renderer: &mut GlesRenderer) {
    let pending: Vec<PendingCapture> = state.pending_screencopy.drain(..).collect();
    for capture in pending {
        match copy_output(state, renderer, &capture) {
            Ok(()) => {
                // Timestamps are what a recorder uses to pace frames.
                let time = state.start_time.elapsed();
                let secs = time.as_secs();
                capture
                    .frame
                    .flags(zwlr_screencopy_frame_v1::Flags::empty());
                // Only for `copy_with_damage`, and only where the client understands
                // the event: `damage` arrived in version 2, and sending an event a
                // client's version does not have kills its connection.
                if capture.with_damage && capture.frame.version() >= 2 {
                    capture.frame.damage(
                        0,
                        0,
                        capture.region.size.w as u32,
                        capture.region.size.h as u32,
                    );
                }
                capture.frame.ready(
                    (secs >> 32) as u32,
                    (secs & 0xFFFF_FFFF) as u32,
                    time.subsec_nanos(),
                );
            }
            Err(reason) => fail(&capture, &reason),
        }
    }
}

/// Draw `capture`'s output offscreen and copy the pixels into its buffer.
fn copy_output(
    state: &mut Wlrix,
    renderer: &mut GlesRenderer,
    capture: &PendingCapture,
) -> Result<(), String> {
    let full = output_size(&capture.output).ok_or("output has no mode")?;
    let region = capture.region;

    let elements =
        crate::render::output_elements(state, renderer, &capture.output, capture.overlay_cursor);

    // Draw the whole output, then read back only the requested area. Rendering just the
    // region would put the output's top-left corner in it, since elements are drawn at
    // their own positions.
    //
    // Physical and buffer coordinates coincide here: the capture is rendered at scale 1
    // with no transform, so the sizes are relabeled rather than converted.
    let target_size: Size<i32, BufferCoord> = (full.size.w, full.size.h).into();
    let mut target: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, target_size)
        .map_err(|err| format!("could not allocate a capture buffer: {err}"))?;

    let mut damage_tracker = OutputDamageTracker::new(full.size, 1.0, capture_transform(state));
    let mut framebuffer = renderer
        .bind(&mut target)
        .map_err(|err| format!("could not draw into the capture buffer: {err}"))?;
    damage_tracker
        .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
        .map_err(|err| format!("could not draw the capture: {err}"))?;

    let read_back: Rectangle<i32, BufferCoord> = Rectangle::new(
        (region.loc.x, region.loc.y).into(),
        (region.size.w, region.size.h).into(),
    );
    let mapping = renderer
        .copy_framebuffer(&framebuffer, read_back, Fourcc::Xrgb8888)
        .map_err(|err| format!("could not read back the capture: {err}"))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| format!("could not map the capture: {err}"))?;

    with_buffer_contents_mut(&capture.buffer, |ptr, len, data| {
        let expected = (region.size.w * region.size.h * 4) as usize;
        if data.format != wl_shm::Format::Xrgb8888 && data.format != wl_shm::Format::Argb8888 {
            return Err(format!("unsupported buffer format {:?}", data.format));
        }
        if len < expected || pixels.len() < expected {
            return Err("buffer is too small for the capture".to_string());
        }
        // SAFETY: both buffers hold at least `expected` bytes, checked above, and the
        // client's shm buffer is mapped writable for us.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, expected);
        }
        Ok(())
    })
    .map_err(|err| format!("could not access the client buffer: {err}"))?
}
