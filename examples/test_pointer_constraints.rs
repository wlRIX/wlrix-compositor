// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `zwp_pointer_constraints_v1`: capturing the mouse, as a game or an emulator does.
//!
//! The compositor advertised the protocol and delivered `zwp_relative_pointer_v1` deltas long
//! before it would actually *hold* the pointer, so capturing the mouse half-worked: a client got
//! its deltas, and the cursor walked out of the window anyway. This is the client half of the
//! check, because a pointer lock cannot be tested without a hand on the mouse -- nothing can
//! synthesize the motion that proves the cursor is being held still.
//!
//! A small panel appears in the top-left corner. Move the pointer onto it and it locks: **the
//! cursor should stop dead** while the counters keep climbing. Keep moving the mouse in one
//! direction for a few seconds -- the failure this was written for is the cursor escaping across
//! the desktop -- then wait for it to let go on its own.
//!
//! What a working lock prints:
//!
//! ```text
//! locked (the cursor should now be frozen)
//! relative motion x842, latest -3.0,1.0
//! unlocked
//! done: 842 relative events, 0 absolute events while locked
//! ```
//!
//! `0 absolute events while locked` is the whole result: a locked pointer receives no
//! `wl_pointer.motion` at all, and the deltas are what the client aims with instead.
//!
//! The lock releases itself after a few seconds and the tool exits, because a locked pointer is
//! a frozen cursor and a dev tool has no business keeping one.
//!
//! Usage: `cargo run --example test_pointer_constraints [seconds]` with `WAYLAND_DISPLAY` set to
//! the compositor under test. Not part of the compositor; a dev tool only.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// An opaque red fill: this panel is a trap for the pointer, and should look like one.
const TINT: u32 = 0xffc0_3020;

/// How big the panel is. Deliberately not the whole screen -- the pointer has to be able to
/// reach it *and* the rest of the desktop has to stay usable while this runs.
const PANEL: (u32, u32) = (520, 360);

#[derive(Default)]
struct Probe {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    constraints: Option<ZwpPointerConstraintsV1>,
    relative: Option<ZwpRelativePointerManagerV1>,
    surface: Option<WlSurface>,
    pointer: Option<WlPointer>,
    lock: Option<ZwpLockedPointerV1>,
    painted: Option<(u32, u32)>,
    /// Whether the compositor has told us the lock took effect.
    locked: bool,
    /// Relative deltas received: what the client actually steers by.
    relative_events: u32,
    /// `wl_pointer.motion` received *while locked*. Must stay zero -- see the module docs.
    absolute_while_locked: u32,
    last_report: Option<Instant>,
    latest_delta: (f64, f64),
}

impl Dispatch<WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
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
            "wl_compositor" => probe.compositor = Some(registry.bind(name, 4, qh, ())),
            "wl_shm" => probe.shm = Some(registry.bind(name, 1, qh, ())),
            "zwlr_layer_shell_v1" => probe.layer_shell = Some(registry.bind(name, 1, qh, ())),
            "zwp_pointer_constraints_v1" => {
                probe.constraints = Some(registry.bind(name, 1, qh, ()))
            }
            "zwp_relative_pointer_manager_v1" => {
                probe.relative = Some(registry.bind(name, 1, qh, ()))
            }
            "wl_seat" => {
                let _seat: WlSeat = registry.bind(name, 5, qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Probe {
    fn event(
        probe: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && probe.pointer.is_none() {
            probe.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if probe.painted == Some((width, height)) {
                    return;
                }
                probe.paint(qh, width.max(1) as i32, height.max(1) as i32);
                probe.painted = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => println!("closed"),
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for Probe {
    fn event(
        probe: &mut Self,
        _pointer: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            // The pointer is over the panel, which is the moment to spring the trap. A
            // compositor is entitled to refuse a constraint for a surface that does not have
            // the pointer, so asking before this would be asking for nothing.
            wl_pointer::Event::Enter { .. } => {
                println!("pointer enter -- asking to lock it");
                probe.lock_pointer(qh);
            }
            wl_pointer::Event::Leave { .. } => println!("pointer leave"),
            // The measurement. A locked pointer is not supposed to get these at all.
            wl_pointer::Event::Motion { .. } if probe.locked => {
                probe.absolute_while_locked += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _lock: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                probe.locked = true;
                println!("locked (the cursor should now be frozen)");
            }
            zwp_locked_pointer_v1::Event::Unlocked => {
                probe.locked = false;
                println!("unlocked");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _relative: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event else {
            return;
        };
        probe.relative_events += 1;
        probe.latest_delta = (dx, dy);
        // A firehose; one line a second rather than one per event.
        let now = Instant::now();
        let due = probe
            .last_report
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
        if due {
            println!(
                "relative motion x{}, latest {dx:.1},{dy:.1}",
                probe.relative_events
            );
            probe.last_report = Some(now);
        }
    }
}

impl Probe {
    /// Ask for the pointer, once.
    fn lock_pointer(&mut self, qh: &QueueHandle<Self>) {
        if self.lock.is_some() {
            return;
        }
        let (Some(constraints), Some(surface), Some(pointer)) = (
            self.constraints.as_ref(),
            self.surface.as_ref(),
            self.pointer.as_ref(),
        ) else {
            return;
        };
        // No region: the lock applies wherever the surface does. `Oneshot`, so it goes away by
        // itself once the compositor deactivates it rather than re-arming behind our back.
        self.lock =
            Some(constraints.lock_pointer(surface, pointer, None, Lifetime::Oneshot, qh, ()));
    }

    /// Fill the surface with [`TINT`] and commit it.
    fn paint(&mut self, qh: &QueueHandle<Self>, width: i32, height: i32) {
        let (Some(shm), Some(surface)) = (self.shm.as_ref(), self.surface.as_ref()) else {
            return;
        };
        let stride = width * 4;
        let len = (stride * height) as usize;

        let Some(mut file) = tempfile() else {
            return;
        };
        let pixels: Vec<u8> = TINT
            .to_ne_bytes()
            .iter()
            .cycle()
            .take(len)
            .copied()
            .collect();
        if file.write_all(&pixels).is_err() || file.flush().is_err() {
            return;
        }
        let _ = file.seek(SeekFrom::Start(0));

        let pool: WlShmPool = shm.create_pool(file.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();

        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
    }
}

/// Whether the connection has something to read within `timeout`.
///
/// The one thing `wayland-client` does not offer: a read that gives up. `poll` on the
/// connection's own fd is what its `blocking_dispatch` does underneath, minus the waiting
/// forever.
fn readable(connection: &Connection, timeout: Duration) -> bool {
    let mut fd = libc::pollfd {
        fd: connection.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: polling one descriptor, described by one `pollfd`, which outlives the call.
    unsafe { libc::poll(&mut fd, 1, millis) > 0 }
}

/// An unlinked temporary file to back the shm pool.
fn tempfile() -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-constraints-{}", std::process::id()));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    let _ = std::fs::remove_file(&path);
    Some(file)
}

delegate_noop!(Probe: ignore WlCompositor);
delegate_noop!(Probe: ignore WlShm);
delegate_noop!(Probe: ignore WlShmPool);
delegate_noop!(Probe: ignore WlBuffer);
delegate_noop!(Probe: ignore WlSurface);
delegate_noop!(Probe: ignore ZwlrLayerShellV1);
delegate_noop!(Probe: ignore ZwpPointerConstraintsV1);
delegate_noop!(Probe: ignore ZwpRelativePointerManagerV1);

fn main() {
    // Short by default: this freezes the cursor, and a dev tool should hand it back quickly.
    let hold = Duration::from_secs(
        std::env::args()
            .nth(1)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(8),
    );

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<Probe>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut probe = Probe::default();
    queue.roundtrip(&mut probe).expect("bind globals");
    queue.roundtrip(&mut probe).expect("seat capabilities");

    let compositor = probe.compositor.clone().expect("no wl_compositor");
    let layer_shell = probe.layer_shell.clone().expect("no zwlr_layer_shell_v1");
    // Said plainly: a compositor without these cannot capture the mouse at all, and that is
    // the answer rather than a crash further down.
    let constraints = probe
        .constraints
        .clone()
        .expect("no zwp_pointer_constraints_v1: the compositor cannot lock the pointer");
    let relative = probe
        .relative
        .clone()
        .expect("no zwp_relative_pointer_manager_v1: a locked client would get nothing");
    println!("both globals present");

    // Relative motion is per-pointer, not per-surface, so it is set up once and stays.
    let pointer = probe.pointer.clone().expect("no wl_pointer");
    let _relative_pointer = relative.get_relative_pointer(&pointer, &qh, ());

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        // Above the windows, so reaching it does not mean finding bare desktop first.
        Layer::Top,
        "wlrix-test-pointer-constraints".into(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::Top | Anchor::Left);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(PANEL.0, PANEL.1);
    surface.commit();
    probe.surface = Some(surface);
    let _ = &constraints;

    println!(
        "red panel in the top-left corner. Move the pointer onto it, then keep moving \
         the mouse: the cursor should stop dead. Letting go in {hold:?}."
    );
    // Deliberately **not** `blocking_dispatch`. With the pointer locked and the mouse held
    // still, no events arrive at all -- and a blocking dispatch would sit in the middle of the
    // loop past the deadline, holding the cursor hostage until something happened to wake it.
    // A tool that freezes the pointer has to be able to let go on a clock alone.
    let deadline = Instant::now() + hold;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        queue.flush().expect("flush");
        match queue.prepare_read() {
            Some(guard) if readable(&connection, remaining) => {
                let _ = guard.read();
            }
            // Nothing to read before the deadline, or events already queued.
            Some(_) => {}
            None => {}
        }
        queue.dispatch_pending(&mut probe).expect("dispatch");
    }

    // Hand the pointer back before leaving, rather than relying on the client dying to do it.
    if let Some(lock) = probe.lock.take() {
        lock.destroy();
    }
    let _ = queue.roundtrip(&mut probe);

    println!(
        "done: {} relative events, {} absolute events while locked",
        probe.relative_events, probe.absolute_while_locked
    );
    if probe.relative_events > 0 && probe.absolute_while_locked == 0 {
        println!("PASS: the pointer was held and the client still got its deltas");
    } else if probe.relative_events == 0 {
        println!("INCONCLUSIVE: no mouse movement was seen, so nothing was tested");
    } else {
        println!("FAIL: the pointer moved while locked");
    }
}
