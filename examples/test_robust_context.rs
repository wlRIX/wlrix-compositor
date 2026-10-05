// SPDX-License-Identifier: GPL-3.0-or-later
//! Throwaway check that `backend::robust_context` gets a robust context on real hardware.
//!
//!     cargo run --release --example test_robust_context -- /dev/dri/renderD128 ...
//!
//! For each node: builds the context the compositor would, asks EGL what reset-notification
//! strategy it actually ended up with, drives a `GlesRenderer` far enough to prove the
//! adopted context is usable, and imports a real dmabuf through it -- the operation clients
//! depend on, and the one that broke when the original `EGLDisplay` was allowed to drop.
//!
//! The display is dropped early on purpose, to prove the adopted context keeps it alive; if it
//! did not, `eglTerminate` would run and poison the renderer. The robust pass then rebuilds the
//! renderer the way recovery from a GPU reset does, and imports through the new context.

use smithay::backend::{
    allocator::{
        Allocator, Fourcc, Modifier,
        dmabuf::AsDmabuf,
        gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
    },
    drm::DrmDeviceFd,
    egl::{EGLDisplay, ffi as egl_ffi},
    renderer::{
        ImportDma,
        gles::{GlesRenderer, ffi as gles_ffi},
    },
};
use smithay::utils::DeviceFd;
use std::fs::OpenOptions;

#[path = "../src/backend/robust_context.rs"]
mod robust_context;

const RESET_NOTIFICATION_STRATEGY: i32 = 0x31BD;
const LOSE_CONTEXT_ON_RESET: i32 = 0x31BF;
const NO_RESET_NOTIFICATION: i32 = 0x31BE;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    let nodes: Vec<String> = std::env::args().skip(1).collect();
    if nodes.is_empty() {
        eprintln!("usage: test_robust_context /dev/dri/renderD128 [...]");
        std::process::exit(2);
    }

    let mut regressions = 0;
    for path in nodes {
        // Both context kinds: the question is never "does this driver work" but "does the
        // robust context work as well as the plain one it replaces". A driver that cannot
        // import a linear dmabuf either way is not this change's doing.
        let mut results = Vec::new();
        for robust in [false, true] {
            let kind = if robust { "robust" } else { "plain (baseline)" };
            println!("\n=== {path} -- {kind} ===");
            let result = check(&path, robust);
            if let Err(err) = &result {
                println!("  FAIL: {err}");
            }
            results.push(result);
        }

        match (&results[0], &results[1]) {
            (_, Ok(())) => println!("\n  {path}: robust context ok"),
            (Err(_), Err(_)) => {
                println!("\n  {path}: fails both ways -- driver behavior, not the robust context")
            }
            (Ok(()), Err(err)) => {
                println!("\n  {path}: REGRESSION -- plain works, robust does not: {err}");
                regressions += 1;
            }
        }
    }
    std::process::exit(if regressions == 0 { 0 } else { 1 });
}

fn check(path: &str, robust: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)));
    let gbm = GbmDevice::new(fd)?;
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    println!("  EGL version: {:?}", display.get_egl_version());

    let context = if robust {
        robust_context::create_context(&display)?
    } else {
        smithay::backend::egl::EGLContext::new(&display)?
    };

    // What did we actually get? This is the whole point: a driver may accept the attribute
    // and quietly hand back a non-robust context, which would abort the session just the same.
    //
    // Only readable where `EGL_EXT_query_reset_notification_strategy` says so -- Mesa has it,
    // the NVIDIA driver does not -- so its absence is inconclusive, not a failure.
    let mut strategy: i32 = 0;
    let queryable = display
        .extensions()
        .iter()
        .any(|e| e == "EGL_EXT_query_reset_notification_strategy");
    if queryable {
        let raw_display = **display.get_display_handle();
        let queried = unsafe {
            egl_ffi::egl::QueryContext(
                raw_display,
                context.get_context_handle(),
                RESET_NOTIFICATION_STRATEGY,
                &mut strategy,
            )
        };
        let described = match strategy {
            LOSE_CONTEXT_ON_RESET => "LOSE_CONTEXT_ON_RESET (robust)",
            NO_RESET_NOTIFICATION => "NO_RESET_NOTIFICATION (NOT robust -- Mesa will abort)",
            _ => "unknown",
        };
        println!("  eglQueryContext ok={queried} strategy=0x{strategy:x} {described}");
    } else {
        println!("  strategy not queryable here (no EGL_EXT_query_reset_notification_strategy)");
    }

    // Let go of the display here, exactly as `device_added` does when its block closes. The
    // context is expected to be holding its own reference: if it is not, `eglTerminate` runs
    // now and everything below fails -- which is the black screen this example exists to
    // catch, and which nothing above would have noticed.
    drop(display);

    // The adopted context has to survive being driven by the renderer, not just exist.
    let mut renderer = unsafe { GlesRenderer::new(context)? };
    let gl_renderer = renderer.with_context(|gl| unsafe {
        std::ffi::CStr::from_ptr(gl.GetString(gles_ffi::RENDERER) as *const _)
            .to_string_lossy()
            .into_owned()
    })?;
    println!("  GL renderer: {gl_renderer}");
    println!(
        "  gpu_reset(): {:?} (None means healthy)",
        robust_context::gpu_reset(&mut renderer)
    );

    // A client handing over a dmabuf is what actually exercises the EGL display, and what
    // silently stopped working when the display was terminated early.
    let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let buffer = allocator.create_buffer(256, 256, Fourcc::Argb8888, &[Modifier::Linear])?;
    let dmabuf = buffer.export()?;

    // `import_dmabuf` returns `Ok` even when the underlying `EGLImage` was rejected -- which
    // is how an early `eglTerminate` stayed invisible until clients started dying with
    // "create_immed failed and produced an invalid wl_buffer". Ask GL directly instead.
    renderer.with_context(|gl| unsafe { while gl.GetError() != gles_ffi::NO_ERROR {} })?;
    renderer
        .import_dmabuf(&dmabuf, None)
        .map_err(|err| format!("dmabuf import failed: {err}"))?;
    let gl_error = renderer.with_context(|gl| unsafe { gl.GetError() })?;
    if gl_error != gles_ffi::NO_ERROR {
        return Err(format!("dmabuf import raised GL error 0x{gl_error:x}").into());
    }
    println!("  dmabuf import: ok, no GL error (with the local display already dropped)");

    // What a GPU reset leads to: the renderer swapped onto a fresh context in place. There is
    // no forcing a real reset from here without risking the running session, so this proves
    // the swap itself -- the old context destroyed, the display still initialized, and the
    // new one able to take a client buffer.
    if robust {
        robust_context::rebuild(&mut renderer)?;
        println!(
            "  rebuild: ok, gpu_reset(): {:?}",
            robust_context::gpu_reset(&mut renderer)
        );
        renderer.with_context(|gl| unsafe { while gl.GetError() != gles_ffi::NO_ERROR {} })?;
        renderer
            .import_dmabuf(&dmabuf, None)
            .map_err(|err| format!("dmabuf import after rebuild failed: {err}"))?;
        let gl_error = renderer.with_context(|gl| unsafe { gl.GetError() })?;
        if gl_error != gles_ffi::NO_ERROR {
            return Err(
                format!("dmabuf import after rebuild raised GL error 0x{gl_error:x}").into(),
            );
        }
        println!("  dmabuf import after rebuild: ok, no GL error");
    }

    if robust && queryable && strategy != LOSE_CONTEXT_ON_RESET {
        return Err("context is not robust".into());
    }
    Ok(())
}
