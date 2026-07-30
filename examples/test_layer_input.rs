// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises input routing to `zwlr_layer_shell_v1` surfaces.
//!
//! The compositor composited layer surfaces long before it would talk to them: `surface_under`
//! consulted only the window space, so a layer client got no pointer events and could never
//! hold keyboard focus. That is fine for an inert backdrop -- which is all `wlrix-greeter` puts
//! on a layer -- but not for the desktop icons, where the interactive part *is* the background.
//!
//! This is a **background**-layer surface asking for `on-demand` keyboard interactivity, i.e.
//! exactly what `wlrix-desktop` will be. It prints every pointer and keyboard event it gets, so
//! "does a background layer receive input, and does clicking it take focus?" has an answer that
//! does not depend on the desktop app existing yet.
//!
//! What should happen: moving over the desktop prints `motion`, clicking prints `button` **and**
//! `keyboard enter`, and clicking a window on top of it prints `keyboard leave` and no further
//! motion until the pointer is back over bare desktop.
//!
//! Usage: `cargo run --example test_layer_input [seconds]` with `WAYLAND_DISPLAY` set to the
//! compositor under test. Not part of the compositor; a dev tool only.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// A translucent tint, so windows on top stay readable while the probe runs.
const TINT: u32 = 0x4000_80ff;

#[derive(Default)]
struct Probe {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    surface: Option<WlSurface>,
    /// Set once the surface has been given a size and painted, so `configure` only draws
    /// again when the size actually changes.
    painted: Option<(u32, u32)>,
    /// Motion is a firehose; count it and print one line per second instead.
    motions: u32,
    last_report: Option<Instant>,
    /// Whether the compositor has given this surface keyboard focus.
    focused: bool,
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
            "wl_seat" => {
                let _seat: WlSeat = registry.bind(name, 5, qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Probe {
    fn event(
        _probe: &mut Self,
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
        if caps.contains(wl_seat::Capability::Pointer) {
            seat.get_pointer(qh, ());
        }
        if caps.contains(wl_seat::Capability::Keyboard) {
            seat.get_keyboard(qh, ());
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
                println!("configure: {width}x{height}");
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
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => println!("pointer enter at {surface_x:.0},{surface_y:.0}"),
            wl_pointer::Event::Leave { .. } => println!("pointer leave"),
            // One line a second, not one per motion event.
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                probe.motions += 1;
                let now = Instant::now();
                let due = probe
                    .last_report
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
                if due {
                    println!(
                        "motion x{}, latest {surface_x:.0},{surface_y:.0}",
                        probe.motions
                    );
                    probe.motions = 0;
                    probe.last_report = Some(now);
                }
            }
            wl_pointer::Event::Button { button, state, .. } => {
                println!("button {button} {state:?}")
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for Probe {
    fn event(
        probe: &mut Self,
        _keyboard: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // This is the on-demand focus arriving: the compositor decided a click on the
            // background layer should give it the keyboard.
            wl_keyboard::Event::Enter { .. } => {
                probe.focused = true;
                println!("keyboard enter (this surface now has focus)");
            }
            wl_keyboard::Event::Leave { .. } => {
                probe.focused = false;
                println!("keyboard leave");
            }
            wl_keyboard::Event::Key { key, state, .. } => println!("key {key} {state:?}"),
            _ => {}
        }
    }
}

impl Probe {
    /// Fill the surface with [`TINT`] and commit it.
    fn paint(&mut self, qh: &QueueHandle<Self>, width: i32, height: i32) {
        let (Some(shm), Some(surface)) = (self.shm.as_ref(), self.surface.as_ref()) else {
            return;
        };
        let stride = width * 4;
        let len = (stride * height) as usize;

        // Written before the pool is created, so the compositor's mapping sees it.
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

/// An unlinked temporary file to back the shm pool.
fn tempfile() -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-layer-input-{}", std::process::id()));
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

delegate_noop!(Probe: ignore WlCompositor);
delegate_noop!(Probe: ignore WlShm);
delegate_noop!(Probe: ignore WlShmPool);
delegate_noop!(Probe: ignore WlBuffer);
delegate_noop!(Probe: ignore WlSurface);
delegate_noop!(Probe: ignore ZwlrLayerShellV1);

fn main() {
    let hold = Duration::from_secs(
        std::env::args()
            .nth(1)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(30),
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

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Background,
        "wlrix-test-layer-input".into(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::all());
    // 0, not -1: shrink to avoid other clients' exclusive zones, as a desktop should.
    layer.set_exclusive_zone(0);
    // The whole point of the probe -- without this the compositor should never focus it.
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.set_size(0, 0);
    surface.commit();
    probe.surface = Some(surface);

    println!("background layer up; move the pointer over bare desktop and click. {hold:?}");
    let deadline = Instant::now() + hold;
    while Instant::now() < deadline {
        queue.blocking_dispatch(&mut probe).expect("dispatch");
    }
    println!("done (focused={})", probe.focused);
}
