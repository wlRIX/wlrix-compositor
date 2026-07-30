// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-image-capture-source-v1` + `ext-image-copy-capture-v1`: the standard way to read back
//! what the compositor is showing.
//!
//! This is the successor to `wlr-screencopy-v1` (see [`crate::screencopy`], still advertised
//! because that is what `grim` and today's portals speak). The split is the point of the new
//! design: a *source* object names what to capture -- an output, or a window from the
//! [`ext-foreign-toplevel-list`][crate::foreign_toplevel] -- and a separate *copy* protocol
//! negotiates buffers and drives frames against it. Per-window capture is what wlr-screencopy
//! never had, and it is what a screen-sharing portal needs to offer "share a single window".
//!
//! Smithay owns the protocol objects here (unlike wlr-screencopy, which it has no
//! implementation of), so this module is the compositor half: say how big a buffer a source
//! needs, keep those constraints current as windows resize and outputs change mode, and fill
//! the buffers.
//!
//! Filling them cannot happen here -- it needs the renderer, which lives in the backend -- so a
//! requested frame is queued and drained when the backend next draws, exactly as
//! [`crate::screencopy`] and [`crate::thumbnail`] do.
//!
//! Only shared-memory buffers are offered. A dmabuf path would let a client import the result
//! without a copy through the CPU, which is what a serious screen recorder wants, but it needs
//! format/modifier negotiation against the render node and is a separate piece of work.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::AsRenderElements,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    output::{Output, WeakOutput},
    reexports::wayland_server::{DisplayHandle, protocol::wl_shm},
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::{
        foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelWeakHandle},
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, FrameRef, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::with_buffer_contents_mut,
    },
};
use tracing::warn;

use crate::{Wlrix, render::DESKTOP_BACKGROUND as CLEAR_COLOR, render::OutputElem};

/// Everything the two protocols need, kept together because they are only useful in a pair.
pub struct ImageCaptureState {
    output_source: OutputCaptureSourceState,
    toplevel_source: ToplevelCaptureSourceState,
    copy: ImageCopyCaptureState,
    /// Live sessions. Owned: dropping a [`Session`] is what sends `stopped`, so a session whose
    /// source has gone away is retired simply by dropping it out of this list.
    sessions: Vec<Session>,
    /// Frames a client has handed a buffer for, waiting on the backend's renderer.
    pending: Vec<Pending>,
}

/// A frame waiting to be filled, with what it is capturing resolved at request time.
struct Pending {
    frame: Frame,
    target: Target,
    draw_cursor: bool,
}

/// What a session is capturing. Resolved from the source's user data, which is where the
/// `*_source_created` handlers stash it.
#[derive(Clone)]
enum Target {
    Output(Output),
    Window(Window),
}

impl ImageCaptureState {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            output_source: OutputCaptureSourceState::new::<Wlrix>(display),
            toplevel_source: ToplevelCaptureSourceState::new::<Wlrix>(display),
            // Reading the screen is privileged: a sandboxed client must not be able to watch
            // every other application. Same filter the rest of the privileged protocols use.
            copy: ImageCopyCaptureState::new_with_filter::<Wlrix, _>(display, |client| {
                !Wlrix::client_is_sandboxed(client)
            }),
            sessions: Vec::new(),
            pending: Vec::new(),
        }
    }
}

impl Wlrix {
    /// Resolve a source object back to the thing it names.
    ///
    /// Both handles are weak, so a source outliving its output or window resolves to `None` and
    /// the session is stopped rather than left capturing nothing.
    fn capture_target(&self, source: &ImageCaptureSource) -> Option<Target> {
        if let Some(output) = source.user_data().get::<WeakOutput>() {
            return output.upgrade().map(Target::Output);
        }
        let handle = source
            .user_data()
            .get::<ForeignToplevelWeakHandle>()?
            .upgrade()?;
        self.window_for_foreign_toplevel(&handle)
            .map(Target::Window)
    }

    /// The window a foreign-toplevel handle refers to.
    ///
    /// [`crate::foreign_toplevel`] stores the handle in the window's user data when it
    /// announces it, so this is that mapping read backwards.
    fn window_for_foreign_toplevel(&self, handle: &ForeignToplevelHandle) -> Option<Window> {
        self.space
            .elements()
            .chain(self.desks.hidden().iter())
            .find(|window| {
                window
                    .user_data()
                    .get::<ForeignToplevelHandle>()
                    .is_some_and(|other| other.matches(handle))
            })
            .cloned()
    }

    /// The scale a window is captured at: that of the output it is on, so a capture on a HiDPI
    /// screen keeps the detail the client actually rendered.
    fn capture_scale(&self, window: &Window) -> f64 {
        self.space
            .outputs_for_element(window)
            .first()
            .or_else(|| self.space.outputs().next())
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    /// The buffer size a target needs, in pixels.
    fn capture_size(&self, target: &Target) -> Option<Size<i32, BufferCoord>> {
        let size = match target {
            Target::Output(output) => {
                let mode = output.current_mode()?;
                (mode.size.w, mode.size.h)
            }
            Target::Window(window) => {
                let scale = self.capture_scale(window);
                let size = window
                    .geometry()
                    .size
                    .to_physical_precise_round::<_, i32>(scale);
                (size.w, size.h)
            }
        };
        (size.0 > 0 && size.1 > 0).then(|| size.into())
    }

    fn constraints_for(&self, target: &Target) -> Option<BufferConstraints> {
        Some(BufferConstraints {
            size: self.capture_size(target)?,
            // The two formats `with_buffer_contents_mut` below is willing to write into. Both
            // are 4 bytes per pixel in the same order; the capture is opaque either way.
            shm: vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888],
            dma: None,
        })
    }

    /// Keep every session's advertised buffer size in step with its source.
    ///
    /// Called once per event-loop dispatch, the same reconcile-by-walking approach
    /// [`crate::foreign_toplevel`] uses: a window resize, a move to a differently-scaled
    /// output and an output mode change all show up as the computed size changing, so one
    /// check covers all three rather than hooking each.
    pub fn refresh_image_capture(&mut self) {
        // Collected first: `constraints_for` borrows `self`, which cannot happen while the
        // session list is borrowed out of it.
        let sessions: Vec<SessionRef> = self
            .image_capture
            .sessions
            .iter()
            .map(|session| (**session).clone())
            .collect();

        let mut dead = Vec::new();
        for session in sessions {
            let constraints = self
                .capture_target(&session.source())
                .and_then(|target| self.constraints_for(&target));
            match constraints {
                Some(constraints) => {
                    // `update_constraints` re-sends the whole set, so only do it on a change:
                    // a client is entitled to re-allocate its buffers when it hears one.
                    let current = session.current_constraints();
                    if current.is_none_or(|current| current.size != constraints.size) {
                        session.update_constraints(constraints);
                    }
                }
                // The output unplugged or the window closed. Nothing left to capture.
                None => dead.push(session),
            }
        }

        if !dead.is_empty() {
            self.image_capture
                .sessions
                .retain(|session| !dead.iter().any(|gone| session == gone));
        }
        self.image_capture.copy.cleanup();
    }
}

impl ImageCaptureSourceHandler for Wlrix {}

impl OutputCaptureSourceHandler for Wlrix {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.image_capture.output_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ToplevelCaptureSourceHandler for Wlrix {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.image_capture.toplevel_source
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        source
            .user_data()
            .insert_if_missing(|| toplevel.downgrade());
    }
}

impl ImageCopyCaptureHandler for Wlrix {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_capture.copy
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let target = self.capture_target(source)?;
        self.constraints_for(&target)
    }

    fn new_session(&mut self, session: Session) {
        self.image_capture.sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(target) = self.capture_target(&session.source()) else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        // The renderer lives in the backend, so the copy happens when it next draws.
        self.image_capture.pending.push(Pending {
            frame,
            target,
            draw_cursor: session.draw_cursor(),
        });
        self.request_redraw();
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        // Do not try to fill a buffer for a frame the client has thrown away.
        self.image_capture
            .pending
            .retain(|pending| pending.frame != frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.image_capture.sessions.retain(|held| **held != session);
    }
}

/// Fill every queued frame, then answer the clients.
///
/// Called by the backend while it has the renderer, right beside [`crate::screencopy`]'s own
/// drain.
pub fn take_pending(state: &mut Wlrix, renderer: &mut GlesRenderer) {
    let pending: Vec<Pending> = state.image_capture.pending.drain(..).collect();
    for job in pending {
        match copy(state, renderer, &job) {
            Ok(size) => {
                // Damage is not tracked between frames, so the whole buffer is reported
                // changed: always correct, just more work for a client polling for changes.
                let damage = vec![Rectangle::from_size(size)];
                // The capture is rendered upright, so the client needs no correction.
                job.frame
                    .success(Transform::Normal, damage, state.start_time.elapsed());
            }
            Err(reason) => {
                warn!(reason, "image-copy-capture frame failed");
                job.frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
}

/// Draw a job's target offscreen and copy the pixels into the client's buffer.
///
/// Returns the size captured, for the damage report.
fn copy(
    state: &mut Wlrix,
    renderer: &mut GlesRenderer,
    job: &Pending,
) -> Result<Size<i32, BufferCoord>, String> {
    let size = state
        .capture_size(&job.target)
        .ok_or("nothing to capture: the target has no size")?;

    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .map_err(|err| format!("could not allocate a capture buffer: {err}"))?;
    let mut framebuffer = renderer
        .bind(&mut texture)
        .map_err(|err| format!("could not draw into the capture buffer: {err}"))?;

    let physical: Size<i32, Physical> = (size.w, size.h).into();
    match &job.target {
        Target::Output(output) => {
            let elements = crate::render::output_elements(state, renderer, output, job.draw_cursor);
            // Upright, not the output's own transform: see `screencopy::capture_transform`
            // for why an offscreen capture never wants the display surface's flip.
            let mut damage = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
            damage
                .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
                .map_err(|err| format!("could not draw the capture: {err}"))?;
        }
        Target::Window(window) => {
            // `draw_cursor` is ignored here: the pointer is drawn over the desktop, not into a
            // window's own surface tree, so there is nothing to overlay onto a window capture.
            let scale = state.capture_scale(window);
            // `render_elements` places the surface origin; shift so the window's geometry
            // origin (not the surface's, which differs for a client keeping a CSD margin)
            // lands at the buffer's top-left.
            let origin = Point::<i32, Physical>::from((0, 0))
                - window
                    .geometry()
                    .loc
                    .to_physical_precise_round(Scale::from(scale));
            let elements: Vec<OutputElem<GlesRenderer>> =
                window.render_elements(renderer, origin, Scale::from(scale), 1.0);
            // Upright regardless of backend: a plain offscreen texture, not the display
            // surface, so the nested output's `Flipped180` does not apply -- the same
            // reasoning as `thumbnail::snapshot`.
            let mut damage = OutputDamageTracker::new(physical, 1.0, Transform::Normal);
            damage
                .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
                .map_err(|err| format!("could not draw the capture: {err}"))?;
        }
    }

    let mapping = renderer
        .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Xrgb8888)
        .map_err(|err| format!("could not read back the capture: {err}"))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| format!("could not map the capture: {err}"))?;

    let buffer = job.frame.buffer();
    with_buffer_contents_mut(&buffer, |ptr, len, data| {
        let expected = (size.w * size.h * 4) as usize;
        if data.format != wl_shm::Format::Xrgb8888 && data.format != wl_shm::Format::Argb8888 {
            return Err(format!("unsupported buffer format {:?}", data.format));
        }
        if len < expected || pixels.len() < expected {
            return Err("buffer is too small for the capture".to_string());
        }
        // SAFETY: both buffers hold at least `expected` bytes, checked above, and the client's
        // shm buffer is mapped writable for us.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, expected);
        }
        Ok(())
    })
    .map_err(|err| format!("could not access the client buffer: {err}"))??;

    Ok(size)
}
