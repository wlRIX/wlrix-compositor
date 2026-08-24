// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `ext-image-capture-source-v1` + `ext-image-copy-capture-v1`: the standard
//! screen-capture pair that supersedes `wlr-screencopy`.
//!
//! Advertising the globals is not the same as being able to capture, so this drives a whole
//! exchange: make a source, open a session, wait for the buffer constraints, allocate a
//! matching shm buffer, request a frame, and write what comes back to a PNM file. It does the
//! same for a window from `ext-foreign-toplevel-list-v1`, which is the capability
//! `wlr-screencopy` never had.
//!
//! Usage: `cargo run --example test_image_capture [prefix]` with `WAYLAND_DISPLAY` set to the
//! compositor under test. Pass `--watch` instead to hold a session open on the first window and
//! report every constraint change and the `stopped` that must arrive when it closes. Not part
//! of the compositor; a dev tool only.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::WlOutput,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
        ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

/// What a session told us to allocate, filled in from its events before `done`.
#[derive(Default, Clone, Copy)]
struct Constraints {
    width: u32,
    height: u32,
    format: Option<wl_shm::Format>,
}

/// How the current capture ended, so `main` can stop dispatching. `None` means still waiting.
enum Outcome {
    Ready,
    Failed(String),
}

#[derive(Default)]
struct App {
    shm: Option<WlShm>,
    output: Option<WlOutput>,
    output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    toplevel_sources: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    copy: Option<ExtImageCopyCaptureManagerV1>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, String)>,

    /// Constraints being accumulated, then the set the last `done` published.
    incoming: Constraints,
    settled: Option<Constraints>,
    stopped: bool,
    outcome: Option<Outcome>,
    transform: Option<u32>,
    /// The render node the compositor says dmabufs must come from, as a `dev_t`.
    dmabuf_device: Option<u64>,
    /// Offered dmabuf formats, as (fourcc, modifier count).
    dmabuf_formats: Vec<(u32, usize)>,
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
            // The first output is the one captured; a second would just be noise here.
            "wl_output" if app.output.is_none() => {
                app.output = Some(registry.bind(name, 1, qh, ()))
            }
            "ext_output_image_capture_source_manager_v1" => {
                app.output_sources = Some(registry.bind(name, 1, qh, ()))
            }
            "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                app.toplevel_sources = Some(registry.bind(name, 1, qh, ()))
            }
            "ext_image_copy_capture_manager_v1" => app.copy = Some(registry.bind(name, 1, qh, ())),
            "ext_foreign_toplevel_list_v1" => {
                let _list: ExtForeignToplevelListV1 = registry.bind(name, 1, qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(
        app: &mut Self,
        _session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                app.incoming.width = width;
                app.incoming.height = height;
            }
            // Reported, not used: this probe captures into shared memory. Printing them is
            // how "does the compositor offer dmabuf at all" gets answered without a client
            // that can allocate on the render node.
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                let id = u64::from_ne_bytes(device.try_into().unwrap_or([0; 8]));
                app.dmabuf_device = Some(id);
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                app.dmabuf_formats.push((format, modifiers.len() / 8));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                // Take the first offered format; the compositor lists them in preference order.
                if app.incoming.format.is_none()
                    && let Ok(format) = format.into_result()
                {
                    app.incoming.format = Some(format);
                }
            }
            // Everything since the last `done` is one consistent set.
            ext_image_copy_capture_session_v1::Event::Done => {
                app.settled = Some(app.incoming);
                app.incoming.format = None;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => app.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        _frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                app.transform = Some(transform.into());
            }
            ext_image_copy_capture_frame_v1::Event::Ready => app.outcome = Some(Outcome::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                app.outcome = Some(Outcome::Failed(format!("{reason:?}")));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for App {
    fn event(
        app: &mut Self,
        _list: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            app.toplevels.push((toplevel, String::new()));
        }
    }

    wayland_client::event_created_child!(App, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Event::Title { title } = event
            && let Some((_, stored)) = app.toplevels.iter_mut().find(|(res, _)| res == handle)
        {
            *stored = title;
        }
    }
}

// Objects the test drives but never reads events from.
macro_rules! ignore_events {
    ($($ty:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for App {
            fn event(
                _app: &mut Self,
                _obj: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
ignore_events!(
    WlShm,
    WlShmPool,
    WlBuffer,
    WlOutput,
    ExtImageCaptureSourceV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtImageCopyCaptureManagerV1,
);

/// An anonymous, resizable file to back the shm pool.
fn memfd(size: usize) -> OwnedFd {
    // SAFETY: a valid NUL-terminated name and a flag constant; the fd is checked below.
    let raw = unsafe { libc::memfd_create(c"wlrix-capture".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0, "memfd_create failed");
    // SAFETY: `raw` is a fresh fd this process owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    assert_eq!(
        // SAFETY: `fd` is a valid memfd, which supports ftruncate.
        unsafe { libc::ftruncate(raw, size as libc::off_t) },
        0,
        "ftruncate failed"
    );
    fd
}

/// Run one capture against `source` and write the result to `path`.
fn capture(
    app: &mut App,
    queue: &mut wayland_client::EventQueue<App>,
    qh: &QueueHandle<App>,
    label: &str,
    source: &ExtImageCaptureSourceV1,
    path: &str,
) {
    let copy = app
        .copy
        .clone()
        .expect("no ext_image_copy_capture_manager_v1");
    let session = copy.create_session(
        source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        qh,
        (),
    );

    app.settled = None;
    app.dmabuf_device = None;
    // Cleared per capture, or the second session reports the first session's formats as well
    // and the count silently doubles.
    app.dmabuf_formats.clear();
    app.stopped = false;
    app.outcome = None;
    app.transform = None;

    // The constraints arrive unprompted, before the first frame can be asked for.
    for _ in 0..10 {
        queue.roundtrip(app).expect("roundtrip for constraints");
        if app.settled.is_some() || app.stopped {
            break;
        }
    }
    let Some(constraints) = app.settled else {
        println!("{label}: no buffer constraints (stopped={})", app.stopped);
        session.destroy();
        return;
    };
    let Some(format) = constraints.format else {
        println!("{label}: no shm format offered");
        session.destroy();
        return;
    };
    println!(
        "{label}: constraints {}x{} {format:?}",
        constraints.width, constraints.height
    );
    match app.dmabuf_device {
        Some(device) => println!(
            "{label}: dmabuf offered on dev_t {device} — {} formats, e.g. {:?}",
            app.dmabuf_formats.len(),
            app.dmabuf_formats
                .iter()
                .take(3)
                .map(|(code, modifiers)| format!(
                    "{}({} mods)",
                    String::from_utf8_lossy(&code.to_le_bytes()),
                    modifiers
                ))
                .collect::<Vec<_>>(),
        ),
        None => println!("{label}: no dmabuf offered (shm only)"),
    }

    let stride = constraints.width as usize * 4;
    let len = stride * constraints.height as usize;
    let fd = memfd(len);
    let shm = app.shm.clone().expect("no wl_shm");
    let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        constraints.width as i32,
        constraints.height as i32,
        stride as i32,
        format,
        qh,
        (),
    );

    let frame = session.create_frame(qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, constraints.width as i32, constraints.height as i32);
    frame.capture();

    // The compositor fills the buffer on its next draw, so give it several dispatches.
    for _ in 0..40 {
        queue.blocking_dispatch(app).expect("dispatch");
        if app.outcome.is_some() {
            break;
        }
    }

    match app.outcome.take() {
        Some(Outcome::Ready) => {
            // SAFETY: `len` bytes were allocated above and the compositor has finished writing.
            let map = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    0,
                )
            };
            assert_ne!(map, libc::MAP_FAILED, "mmap failed");
            // SAFETY: `map` covers `len` readable bytes.
            let pixels = unsafe { std::slice::from_raw_parts(map as *const u8, len) };
            write_pnm(path, constraints.width, constraints.height, pixels);
            // Non-black pixels are the proof the capture is real rather than a cleared buffer.
            let lit = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|px| px[..3] != [0, 0, 0])
                .count();
            println!(
                "{label}: ready, transform={:?}, {lit} non-black pixels -> {path}",
                app.transform
            );
            // SAFETY: unmapping the region mapped just above.
            unsafe { libc::munmap(map, len) };
        }
        Some(Outcome::Failed(reason)) => println!("{label}: failed ({reason})"),
        _ => println!("{label}: timed out waiting for the frame"),
    }

    frame.destroy();
    buffer.destroy();
    pool.destroy();
    session.destroy();
}

/// Dump the capture as binary PPM, which any image viewer opens.
fn write_pnm(path: &str, width: u32, height: u32, pixels: &[u8]) {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    // The buffer is xrgb8888 little-endian: B, G, R, X per pixel.
    for px in pixels.as_chunks::<4>().0.iter() {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::write(path, out).expect("write the capture");
}

/// Hold a session open on `source` and report what the compositor tells it over time.
///
/// The compositor is meant to re-announce constraints when what it is capturing resizes, and to
/// stop the session outright when that thing goes away -- a session left alive over a closed
/// window would capture nothing forever.
fn watch(
    app: &mut App,
    queue: &mut wayland_client::EventQueue<App>,
    qh: &QueueHandle<App>,
    source: &ExtImageCaptureSourceV1,
    seconds: u64,
) {
    let copy = app
        .copy
        .clone()
        .expect("no ext_image_copy_capture_manager_v1");
    let session = copy.create_session(
        source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        qh,
        (),
    );

    app.settled = None;
    app.dmabuf_device = None;
    // Cleared per capture, or the second session reports the first session's formats as well
    // and the count silently doubles.
    app.dmabuf_formats.clear();
    app.stopped = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut last: Option<(u32, u32)> = None;
    while std::time::Instant::now() < deadline {
        queue.roundtrip(app).expect("roundtrip");
        if let Some(constraints) = app.settled
            && last != Some((constraints.width, constraints.height))
        {
            last = Some((constraints.width, constraints.height));
            println!(
                "watch: constraints {}x{}",
                constraints.width, constraints.height
            );
        }
        if app.stopped {
            println!("watch: session stopped");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !app.stopped {
        println!("watch: still running (never stopped)");
    }
    session.destroy();
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "capture".into());
    let watching = arg == "--watch";
    let prefix = if watching { "capture".to_string() } else { arg };

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<App>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("bind globals");
    // A second pass: the toplevel list is only bound during the first, so its window
    // announcements arrive after it.
    queue.roundtrip(&mut app).expect("toplevel list");
    queue.roundtrip(&mut app).expect("toplevel titles");

    if watching {
        let (handle, title) = app
            .toplevels
            .first()
            .cloned()
            .expect("--watch needs a window open");
        let toplevel_sources = app
            .toplevel_sources
            .clone()
            .expect("no ext_foreign_toplevel_image_capture_source_manager_v1");
        let source = toplevel_sources.create_source(&handle, &qh, ());
        println!("watch: holding a session on {title:?}");
        watch(&mut app, &mut queue, &qh, &source, 30);
        return;
    }

    let output = app.output.clone().expect("no wl_output");
    let output_sources = app
        .output_sources
        .clone()
        .expect("no ext_output_image_capture_source_manager_v1");
    let source = output_sources.create_source(&output, &qh, ());
    capture(
        &mut app,
        &mut queue,
        &qh,
        "output",
        &source,
        &format!("{prefix}-output.pnm"),
    );
    source.destroy();

    let Some((handle, title)) = app.toplevels.first().cloned() else {
        println!("toplevel: no windows open, skipping the per-window capture");
        return;
    };
    let toplevel_sources = app
        .toplevel_sources
        .clone()
        .expect("no ext_foreign_toplevel_image_capture_source_manager_v1");
    let source = toplevel_sources.create_source(&handle, &qh, ());
    println!("toplevel: capturing {title:?}");
    capture(
        &mut app,
        &mut queue,
        &qh,
        "toplevel",
        &source,
        &format!("{prefix}-toplevel.pnm"),
    );
    source.destroy();
}
