// SPDX-License-Identifier: GPL-3.0-or-later
//! A GL context that reports losing the GPU instead of ending the process.
//!
//! Mesa's amdgpu winsys reacts to a lost context by calling `abort()` -- deliberately, and
//! only when the context is *not* robust. Its reasoning is that a client which cannot be
//! told the context died would otherwise sit there drawing nothing, which looks like a
//! freeze with no explanation; better to die loudly. For a compositor that trade is exactly
//! backwards. One rejected command submission takes down the session and every client in
//! it, and the user is dropped back to the TTY mid-work:
//!
//! ```text
//! kernel: amdgpu 0000:c3:00.0: [drm] *ERROR* Not enough memory for command submission!
//! amdgpu: The CS has been rejected, see dmesg for more information (-12).
//! wlrix-session: compositor exited (signal: 6 (SIGABRT) (core dumped))
//! ```
//!
//! Asking for `EGL_LOSE_CONTEXT_ON_RESET` opts out of that bargain: Mesa reports the loss
//! through `glGetGraphicsResetStatus` -- see [`gpu_reset`] -- and leaves the process alone.
//! The outputs on that GPU still stop drawing, because every GL object on the context is
//! gone, but the session, its clients and the VT switch all survive, and the real error
//! reaches the log instead of having to be reconstructed from a core dump.
//!
//! Smithay has no robustness knob: none of `EGLContext`'s six constructors set the
//! attribute, so the context is built by hand here and adopted through
//! `EGLContext::from_raw`. Delete this in favor of an upstream constructor if one lands.
//!
//! # Keeping the `EGLDisplay` alive
//!
//! `EGLContext::from_raw` wraps the raw display handle in a *fresh* [`EGLDisplay`] that
//! shares the handle but not the refcount, so unlike `EGLContext::new` it does not keep the
//! original alive. Let the original drop and `eglTerminate` runs while the renderer is still
//! using it -- after which every `eglMakeCurrent` fails with `EGL_NOT_INITIALIZED` and no
//! client dmabuf will import, which presents as a compositor that starts, logs nothing
//! alarming, and shows a black screen.
//!
//! [`create_context`] therefore parks a clone of the display in the context's own user data,
//! restoring the lifetime `EGLContext::new` would have given it. Callers need do nothing.

use smithay::backend::{
    egl::{
        EGLContext, EGLDisplay, Error as EGLError,
        context::{GlAttributes, PixelFormatRequirements},
        ffi as egl_ffi,
    },
    renderer::gles::{GlesRenderer, ffi as gles_ffi},
};
use tracing::{info, warn};

/// `EGL_CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY`, as spelled by EGL 1.5 core and by
/// `EGL_KHR_create_context`. `EGL_EXT_create_context_robustness` numbers the same attribute
/// differently, hence the pair.
const RESET_NOTIFICATION_STRATEGY: i32 = 0x31BD;
const RESET_NOTIFICATION_STRATEGY_EXT: i32 = 0x3138;
/// `EGL_LOSE_CONTEXT_ON_RESET`. The core, KHR and EXT spellings all agree on this value.
const LOSE_CONTEXT_ON_RESET: i32 = 0x31BF;

/// Smithay's generated bindings stop at the EGL/GLES core, so the reset statuses are
/// spelled out. Core GLES 3.2 and the KHR/EXT robustness extensions all agree on these.
const GUILTY_CONTEXT_RESET: u32 = 0x8253;
const INNOCENT_CONTEXT_RESET: u32 = 0x8254;
const UNKNOWN_CONTEXT_RESET: u32 = 0x8255;

/// Create the rendering context for a GPU, robust if the driver allows it.
///
/// Falls back to Smithay's plain context -- with a warning, because that is the
/// configuration where a GPU fault is fatal to the whole session -- if robustness is
/// unavailable for any reason.
pub fn create_context(display: &EGLDisplay) -> Result<EGLContext, EGLError> {
    match try_robust(display) {
        Ok(context) => Ok(context),
        Err(reason) => {
            warn!(
                reason,
                "no robust GL context: a GPU fault will now abort the compositor and end the session"
            );
            EGLContext::new(display)
        }
    }
}

/// Build the context by hand with the reset-notification attribute set.
///
/// `Err` carries a human-readable reason rather than an [`EGLError`] because every failure
/// here is recoverable -- the caller just falls back -- and the reason is only ever logged.
fn try_robust(display: &EGLDisplay) -> Result<EGLContext, &'static str> {
    let strategy = reset_notification_attr(display).ok_or("driver has no context robustness")?;

    // The context is never made current against a surface -- the renderer draws into
    // offscreen targets and dmabufs -- so it must be usable surfaceless. Smithay's own
    // constructor accepts `EGL_KHR_no_config_context` in place of this, but that path is
    // closed to us: `from_raw` rejects the null config that a configless context carries.
    if !display
        .extensions()
        .iter()
        .any(|ext| ext == "EGL_KHR_surfaceless_context")
    {
        return Err("driver has no EGL_KHR_surfaceless_context");
    }

    // GLES 3.0, matching what Smithay asks for. No depth or stencil: the renderer allocates
    // whatever attachments it needs per target, and demanding them here only narrows the
    // set of configs the driver will offer.
    let attributes = GlAttributes {
        version: (3, 0),
        profile: None,
        debug: cfg!(debug_assertions),
        vsync: false,
    };
    let (_, config_id) = display
        .choose_config(
            attributes,
            PixelFormatRequirements {
                hardware_accelerated: Some(true),
                color_bits: Some(24),
                float_color_buffer: false,
                alpha_bits: Some(8),
                depth_bits: None,
                stencil_bits: None,
                multisampling: None,
            },
        )
        .map_err(|_| "no EGL config for a robust context")?;

    let attribs = [
        egl_ffi::egl::CONTEXT_MAJOR_VERSION as i32,
        attributes.version.0 as i32,
        egl_ffi::egl::CONTEXT_MINOR_VERSION as i32,
        attributes.version.1 as i32,
        strategy,
        LOSE_CONTEXT_ON_RESET,
        egl_ffi::egl::NONE as i32,
    ];

    let raw_display = **display.get_display_handle();
    let context = unsafe {
        egl_ffi::egl::CreateContext(
            raw_display,
            config_id,
            egl_ffi::egl::NO_CONTEXT,
            attribs.as_ptr(),
        )
    };
    if context.is_null() {
        return Err("eglCreateContext rejected the robustness attribute");
    }

    // Adopting the context makes it "externally managed": Smithay will not destroy it on
    // drop. That is fine -- it lives as long as the compositor does -- and the only other
    // effect, `is_shared()` reading true, costs a `glFinish` on drivers without fencing.
    let context = unsafe { EGLContext::from_raw(raw_display, config_id, context) }
        .map_err(|_| "could not adopt the robust context")?;

    // `from_raw` wraps the raw handle in a *fresh* `EGLDisplay` that shares the handle but
    // not the refcount, so on its own it would let the real display drop -- and
    // `eglTerminate` run -- while the renderer is still using it. Parking a clone in the
    // context's user data gives it exactly the lifetime `EGLContext::new` would have: the
    // display now outlives the context, and `user_data` is declared after `display` in
    // `EGLContext`, so it is released last of all.
    //
    // Keeping it here rather than in the caller matters, because the renderer is refcounted
    // and shared: anything holding an `Rc` clone of it outlives the backend's device entry,
    // and a display parked out there would terminate first.
    context.user_data().insert_if_missing(|| display.clone());

    info!("robust GL context created: a GPU reset will be reported, not fatal");
    Ok(context)
}

/// Which attribute names the reset-notification strategy on this display, if any.
fn reset_notification_attr(display: &EGLDisplay) -> Option<i32> {
    let extensions = display.extensions();
    if display.get_egl_version() >= (1, 5)
        || extensions.iter().any(|e| e == "EGL_KHR_create_context")
    {
        Some(RESET_NOTIFICATION_STRATEGY)
    } else if extensions
        .iter()
        .any(|e| e == "EGL_EXT_create_context_robustness")
    {
        Some(RESET_NOTIFICATION_STRATEGY_EXT)
    } else {
        None
    }
}

/// Whether the GPU behind `renderer` has been reset, and who the driver blames.
///
/// `None` means the context is still good, so an ordinary render failure (a full plane, a
/// buffer that would not import) is not mistaken for a dead GPU. Only an explicit reset
/// status counts: failing to even bind the context is left alone, because a VT switch
/// racing with a frame produces exactly that and recovers on its own.
pub fn gpu_reset(renderer: &mut GlesRenderer) -> Option<&'static str> {
    let status = renderer
        .with_context(|gl| unsafe { gl.GetGraphicsResetStatus() })
        .ok()?;

    match status {
        gles_ffi::NO_ERROR => None,
        GUILTY_CONTEXT_RESET => Some("this compositor's own commands caused the reset"),
        INNOCENT_CONTEXT_RESET => Some("another process on this GPU caused the reset"),
        UNKNOWN_CONTEXT_RESET => Some("cause unknown"),
        _ => Some("the driver reported an unrecognized reset status"),
    }
}
