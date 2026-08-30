// SPDX-License-Identifier: GPL-3.0-or-later
//! A throwaway screen locker, for exercising `ext-session-lock-v1` against the
//! compositor.
//!
//! There is no locker installed on a dev machine by default, and a screen lock is a
//! security boundary -- "it compiles" is not evidence that the desktop is actually
//! hidden and that input cannot reach it. This locks the session, paints every output a
//! flat color, holds it, and unlocks, so the compositor's side can be checked for real
//! (take a screenshot while it holds and confirm the desktop is not in it).
//!
//! Usage: `cargo run --example test_locker -- [seconds]` with `WAYLAND_DISPLAY` set to
//! the compositor under test. Not part of the compositor; a dev tool only.

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::fd::AsFd,
    time::Duration,
};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

/// Deliberately not black, so a screenshot cannot be mistaken for "drew nothing".
const LOCK_COLOR: u32 = 0xffb03030;

#[derive(Default)]
struct Locker {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    manager: Option<ExtSessionLockManagerV1>,
    outputs: Vec<WlOutput>,
    /// Set when the compositor confirms the session is locked.
    locked: bool,
    /// Set if the compositor refuses, or ends the lock itself.
    finished: bool,
    surfaces: Vec<ExtSessionLockSurfaceV1>,
}

impl Dispatch<WlRegistry, ()> for Locker {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind::<WlCompositor, _, _>(name, 4, qh, ()))
            }
            "wl_shm" => state.shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ())),
            "wl_output" => state
                .outputs
                .push(registry.bind::<WlOutput, _, _>(name, 2, qh, ())),
            "ext_session_lock_manager_v1" => {
                state.manager =
                    Some(registry.bind::<ExtSessionLockManagerV1, _, _>(name, 1, qh, ()))
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for Locker {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The compositor has the desktop off screen; only now is the session
            // genuinely locked.
            ext_session_lock_v1::Event::Locked => {
                println!("compositor confirmed: locked");
                state.locked = true;
            }
            // Refused, or the lock ended without us asking.
            ext_session_lock_v1::Event::Finished => {
                println!("compositor sent: finished (lock refused or ended)");
                state.finished = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, (WlSurface, u32)> for Locker {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        data: &(WlSurface, u32),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        else {
            return;
        };
        println!("configure: {width}x{height}");
        let (surface, index) = data;
        // A lock surface must ack before it may attach, like xdg-shell.
        state.surfaces[*index as usize].ack_configure(serial);

        let Some(buffer) = state.buffer(qh, width as i32, height as i32) else {
            return;
        };
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
    }
}

impl Locker {
    /// A single-color shm buffer.
    fn buffer(&self, qh: &QueueHandle<Self>, width: i32, height: i32) -> Option<WlBuffer> {
        let shm = self.shm.as_ref()?;
        let stride = width * 4;
        let len = (stride * height) as usize;

        // Written before the pool is created, so the compositor's mapping sees it; no
        // mmap needed on this side.
        let mut file = tempfile()?;
        let pixels: Vec<u8> = LOCK_COLOR
            .to_ne_bytes()
            .iter()
            .cycle()
            .take(len)
            .copied()
            .collect();
        file.write_all(&pixels).ok()?;
        file.flush().ok()?;
        file.seek(SeekFrom::Start(0)).ok()?;

        let pool: WlShmPool = shm.create_pool(file.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        Some(buffer)
    }
}

/// An unlinked temporary file to back the shm pool.
fn tempfile() -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-locker-{}", std::process::id()));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    // Unlinked immediately: the fd keeps it alive, and nothing is left behind.
    let _ = std::fs::remove_file(&path);
    Some(file)
}

delegate_noop!(Locker: ignore WlCompositor);
delegate_noop!(Locker: ignore WlShm);
delegate_noop!(Locker: ignore WlShmPool);
delegate_noop!(Locker: ignore WlBuffer);
delegate_noop!(Locker: ignore WlSurface);
delegate_noop!(Locker: ignore WlOutput);
delegate_noop!(Locker: ignore ExtSessionLockManagerV1);

fn main() {
    let hold = Duration::from_secs(
        std::env::args()
            .nth(1)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(5),
    );

    let conn = Connection::connect_to_env().expect("no compositor to connect to");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = Locker::default();
    // Two rounds: the first surfaces the globals, the second the outputs' details.
    queue.roundtrip(&mut state).unwrap();
    queue.roundtrip(&mut state).unwrap();

    let manager = state
        .manager
        .clone()
        .expect("compositor does not offer ext_session_lock_manager_v1");
    let compositor = state.compositor.clone().expect("no wl_compositor");
    println!("locking {} output(s)", state.outputs.len());

    let lock = manager.lock(&qh, ());
    // One surface per output, or that output stays uncovered.
    for (index, output) in state.outputs.clone().iter().enumerate() {
        let surface = compositor.create_surface(&qh, ());
        let lock_surface =
            lock.get_lock_surface(&surface, output, &qh, (surface.clone(), index as u32));
        state.surfaces.push(lock_surface);
    }
    queue.roundtrip(&mut state).unwrap();

    // Wait for the compositor to confirm. Blocking, because only a blocking dispatch
    // reads from the socket -- `dispatch_pending` alone would spin on an empty local
    // queue and never see the event at all.
    while !state.locked && !state.finished {
        queue.blocking_dispatch(&mut state).unwrap();
    }

    if state.finished {
        eprintln!("FAIL: compositor never locked the session");
        std::process::exit(1);
    }
    if !state.locked {
        eprintln!("FAIL: no `locked` event -- compositor never confirmed the lock");
        std::process::exit(1);
    }

    // Hold it so the screen can be inspected while the lock is up. Nothing more is
    // expected from the compositor here, so there is nothing to dispatch.
    std::thread::sleep(hold);

    println!("unlocking");
    lock.unlock_and_destroy();
    conn.roundtrip().unwrap();
    println!("OK: locked, held, and unlocked");
}
