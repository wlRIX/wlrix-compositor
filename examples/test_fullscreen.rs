// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `xdg_toplevel.set_fullscreen`: what a browser does on F11, and what it hears back.
//!
//! Entering fullscreen is a conversation, not a request: the client asks, and then waits to be
//! told it happened. A compositor that changes what is on screen without ever configuring the
//! toplevel `Fullscreen` -- or that sends a later configure with the state missing -- leaves the
//! client believing it is not fullscreen, and a browser backs out of its own fullscreen mode a
//! second or two later. From the outside that looks like the compositor spontaneously dropping
//! fullscreen; if the cause is a configure it is visible here, because every one is printed.
//!
//! What a working exchange prints:
//!
//! ```text
//! [ 0.02s] mapped 800x600
//! [ 2.02s] -> set_fullscreen(None)
//! [ 2.05s] configure 2560x1440 states=["Activated", "Fullscreen"]
//!            ^ fullscreen confirmed
//! ---
//! fullscreen confirmed after 2 configure(s)
//! fullscreen never taken away
//! still fullscreen at exit: yes
//! ```
//!
//! **This tool passing does not mean fullscreen works.** It only proves the xdg-shell half. A
//! real browser also wants the frame it drew to be reported as *presented*, and gives up on
//! fullscreen when it never is -- which is a `wp_presentation` bug that looks exactly like a
//! fullscreen bug and which this probe, asking for no feedback, cannot see. Reproduce that one
//! with an actual browser (`microsoft-edge-stable --ozone-platform=wayland --start-fullscreen`)
//! under `WAYLAND_DEBUG=1`, and watch for `unset_fullscreen` coming back from the client.
//!
//! Usage: `cargo run --example test_fullscreen [--early] [seconds]` with `WAYLAND_DISPLAY` set to
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
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

/// How long to sit mapped before asking for fullscreen, so the window is settled first --
/// exactly as a browser is when someone presses F11.
const BEFORE_REQUEST: Duration = Duration::from_secs(2);
/// How long to watch afterwards. The reported failure takes "a few seconds" to show up, so this
/// has to outlast it by enough to be believed.
const DEFAULT_WATCH: Duration = Duration::from_secs(15);

/// The window before fullscreen. Small, because what matters is the size it is configured *to*.
const WINDOW: (i32, i32) = (800, 600);
const TINT: u32 = 0xff20_60c0;

struct Probe {
    start: Instant,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    surface: Option<WlSurface>,
    toplevel: Option<XdgToplevel>,
    /// The size the last configure asked for, which is what gets painted.
    size: (i32, i32),
    painted: bool,
    /// Whether the *latest* configure carried `Fullscreen`.
    fullscreen: bool,
    /// Configures seen since the request went out.
    configures_after_request: u32,
    /// How many configures it took to be told fullscreen, if it ever arrived.
    confirmed_after: Option<u32>,
    /// The configure that took the state away again, if one did.
    lost_at: Option<(f32, String)>,
    requested: bool,
}

impl Probe {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            compositor: None,
            shm: None,
            wm_base: None,
            surface: None,
            toplevel: None,
            size: WINDOW,
            painted: false,
            fullscreen: false,
            configures_after_request: 0,
            confirmed_after: None,
            lost_at: None,
            requested: false,
        }
    }

    fn elapsed(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    fn paint(&mut self, qh: &QueueHandle<Self>) {
        let (Some(shm), Some(surface)) = (self.shm.as_ref(), self.surface.as_ref()) else {
            return;
        };
        let (width, height) = self.size;
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
        self.painted = true;
    }
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
            "xdg_wm_base" => probe.wm_base = Some(registry.bind(name, 1, qh, ())),
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Probe {
    fn event(
        _probe: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Answering the ping matters here: an unresponsive client is a state a compositor is
        // entitled to act on, and this tool must not be mistaken for one.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Probe {
    fn event(
        probe: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg_surface.ack_configure(serial);
        probe.paint(qh);
    }
}

impl Dispatch<XdgToplevel, ()> for Probe {
    fn event(
        probe: &mut Self,
        _toplevel: &XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
        else {
            if matches!(event, xdg_toplevel::Event::Close) {
                println!("[{:5.2}s] close requested", probe.elapsed());
            }
            return;
        };

        let states = decode_states(&states);
        let was_fullscreen = probe.fullscreen;
        probe.fullscreen = states.iter().any(|state| state == "Fullscreen");

        // A zero size means "pick your own", which is not a real answer to a fullscreen
        // request -- so it is reported rather than silently normalized.
        if width > 0 && height > 0 {
            probe.size = (width, height);
        }
        println!(
            "[{:5.2}s] configure {width}x{height} states={states:?}",
            probe.elapsed()
        );

        if probe.requested {
            probe.configures_after_request += 1;
            if probe.fullscreen && probe.confirmed_after.is_none() {
                probe.confirmed_after = Some(probe.configures_after_request);
                println!("           ^ fullscreen confirmed");
            }
            if was_fullscreen && !probe.fullscreen && probe.lost_at.is_none() {
                probe.lost_at = Some((probe.elapsed(), format!("{width}x{height} {states:?}")));
                println!("           ^ FULLSCREEN TAKEN AWAY by this configure");
            }
        }
    }
}

/// `xdg_toplevel.configure`'s states array: little-endian `u32`s, one per state.
fn decode_states(raw: &[u8]) -> Vec<String> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_ne_bytes)
        .map(|value| match value {
            1 => "Maximized".to_string(),
            2 => "Fullscreen".to_string(),
            3 => "Resizing".to_string(),
            4 => "Activated".to_string(),
            5 => "TiledLeft".to_string(),
            6 => "TiledRight".to_string(),
            7 => "TiledTop".to_string(),
            8 => "TiledBottom".to_string(),
            9 => "Suspended".to_string(),
            other => format!("Unknown({other})"),
        })
        .collect()
}

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

fn tempfile() -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-fullscreen-{}", std::process::id()));
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--early` asks before the window is mapped, which is what a browser started with
    // `--start-fullscreen` does. Without it the request goes out from a settled window, which
    // is the F11 case.
    let early = args.iter().any(|arg| arg == "--early");
    let watch = args
        .iter()
        .find_map(|arg| arg.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WATCH);

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<Probe>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut probe = Probe::new();
    queue.roundtrip(&mut probe).expect("bind globals");

    let compositor = probe.compositor.clone().expect("wl_compositor");
    let wm_base = probe.wm_base.clone().expect("xdg_wm_base");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("com.wlrix.test-fullscreen".into());
    toplevel.set_title("fullscreen probe".into());
    surface.commit();
    probe.surface = Some(surface);
    probe.toplevel = Some(toplevel.clone());

    if early {
        // What `--start-fullscreen` looks like on the wire, and what a game that wants the
        // screen from the first frame does: the toplevel exists and has had its role commit,
        // but no buffer has been attached, so it is not mapped and not in the compositor's
        // window list yet.
        println!(
            "[{:5.2}s] -> set_fullscreen(None) before the first configure",
            probe.elapsed()
        );
        toplevel.set_fullscreen(None);
        probe.requested = true;
        let _ = connection.flush();
    }

    queue.roundtrip(&mut probe).expect("initial configure");
    println!(
        "[{:5.2}s] mapped {}x{}",
        probe.elapsed(),
        probe.size.0,
        probe.size.1
    );

    let request_at = Instant::now() + BEFORE_REQUEST;
    let stop_at = request_at + watch;
    loop {
        let now = Instant::now();
        if now >= stop_at {
            break;
        }
        if !probe.requested && now >= request_at {
            probe.requested = true;
            println!("[{:5.2}s] -> set_fullscreen(None)", probe.elapsed());
            toplevel.set_fullscreen(None);
            let _ = connection.flush();
        }
        let next = if probe.requested { stop_at } else { request_at };
        if readable(&connection, next.saturating_duration_since(now)) {
            queue.blocking_dispatch(&mut probe).expect("dispatch");
        } else {
            queue
                .dispatch_pending(&mut probe)
                .expect("dispatch pending");
        }
        let _ = connection.flush();
    }

    println!("---");
    match probe.confirmed_after {
        Some(n) => println!("fullscreen confirmed after {n} configure(s)"),
        None => println!("fullscreen NEVER confirmed -- no configure carried the state"),
    }
    match &probe.lost_at {
        Some((at, what)) => println!("fullscreen taken away at {at:5.2}s by configure {what}"),
        None => println!("fullscreen never taken away"),
    }
    println!(
        "still fullscreen at exit: {}",
        if probe.fullscreen { "yes" } else { "NO" }
    );
}

delegate_noop!(Probe: ignore WlCompositor);
delegate_noop!(Probe: ignore WlShm);
delegate_noop!(Probe: ignore WlShmPool);
delegate_noop!(Probe: ignore WlBuffer);
delegate_noop!(Probe: ignore WlSurface);
