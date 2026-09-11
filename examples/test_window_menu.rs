// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `xdg_toplevel.show_window_menu`: what a right-click on a GTK headerbar sends.
//!
//! A window this compositor decorates reaches its window menu through the frame -- the menu
//! button, or a right-click on the border. A client that draws its own titlebar has neither, so
//! the toolkit asks the compositor for the menu with this request. GTK 3 and GTK 4 both send it.
//! A compositor that does not implement it drops the request, and the right-click does nothing
//! at all -- which is what wlRIX did until `XdgShellHandler::show_window_menu` was written.
//!
//! Nothing comes back: the request has no reply and no protocol error. **So this probe cannot
//! tell you it worked** -- it tells you the request went out, and the menu either appears on
//! screen or does not. Run it nested and look, or screenshot with `grim`.
//!
//! ## The part worth testing, which is the coordinates
//!
//! The position is "relative to the local surface coordinates of the parent surface" -- the
//! *surface*, not the window. A client that draws a shadow around itself declares a smaller
//! window geometry inside a larger surface, and a compositor that confuses the two posts the
//! menu out in the shadow, diagonally off the corner of the window.
//!
//! `--margin N` is what makes that visible: it declares a window geometry inset `N` pixels on
//! every side, the way GTK does for its shadow, and paints the margin in a second color so the
//! two origins are distinguishable on screen. The menu should appear at the point marked by the
//! crosshair, which is drawn at the requested position, and never at a diagonal offset from it.
//!
//! Usage: `cargo run --example test_window_menu [--margin N] [x y] [--seconds N]` with
//! `WAYLAND_DISPLAY` set to the compositor under test. Not part of the compositor; a dev tool.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
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

/// How long to stay up after asking, so the menu can be looked at or screenshotted.
const DEFAULT_WATCH: Duration = Duration::from_secs(20);
/// Long enough to be mapped and settled first: the menu is posted relative to where the window
/// ended up, so asking before it has been placed would measure nothing.
const ASK_AFTER: Duration = Duration::from_millis(800);

const WINDOW: (i32, i32) = (600, 400);
/// The window itself, and the margin outside its declared geometry.
const TINT: u32 = 0xff20_60c0;
const MARGIN_TINT: u32 = 0x8020_2020;
/// The crosshair drawn at the requested point, so the menu's corner can be compared to it.
const MARK: u32 = 0xffff_e000;

struct Probe {
    start: Instant,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    seat: Option<WlSeat>,
    surface: Option<WlSurface>,
    size: (i32, i32),
    /// The inset this client claims for its shadow, on every side.
    margin: i32,
    /// Where the menu is asked for, in surface-local coordinates.
    ask_at: (i32, i32),
    painted: bool,
    asked: bool,
}

impl Probe {
    fn new(margin: i32, ask_at: (i32, i32)) -> Self {
        Self {
            start: Instant::now(),
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            surface: None,
            size: WINDOW,
            margin,
            ask_at,
            painted: false,
            asked: false,
        }
    }

    fn elapsed(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// Paint the surface: the margin in one color, the window in another, and a crosshair where
    /// the menu is about to be asked for.
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

        let mut pixels: Vec<u8> = Vec::with_capacity(len);
        for y in 0..height {
            for x in 0..width {
                let inside = x >= self.margin
                    && y >= self.margin
                    && x < width - self.margin
                    && y < height - self.margin;
                let (mx, my) = self.ask_at;
                let on_mark = (x - mx).abs() <= 6 && (y - my).abs() <= 1
                    || (x - mx).abs() <= 1 && (y - my).abs() <= 6;
                let color = if on_mark {
                    MARK
                } else if inside {
                    TINT
                } else {
                    MARGIN_TINT
                };
                pixels.extend_from_slice(&color.to_ne_bytes());
            }
        }
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
            // The request names the seat the user acted on. Bound for that and nothing else --
            // this probe reads no input.
            "wl_seat" => probe.seat = Some(registry.bind(name, 1, qh, ())),
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
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    probe.size = (width, height);
                }
            }
            xdg_toplevel::Event::Close => {
                println!(
                    "[{:5.2}s] the compositor asked this window to close",
                    probe.elapsed()
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

delegate_noop!(Probe: ignore WlCompositor);
delegate_noop!(Probe: ignore WlShm);
delegate_noop!(Probe: ignore WlShmPool);
delegate_noop!(Probe: ignore WlBuffer);
delegate_noop!(Probe: ignore WlSurface);
delegate_noop!(Probe: ignore WlSeat);

fn tempfile() -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-window-menu-{}", std::process::id()));
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
    let number_after = |flag: &str| -> Option<i32> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|value| value.parse().ok())
    };
    let margin = number_after("--margin").unwrap_or(0);
    let watch = number_after("--seconds")
        .map(|s| Duration::from_secs(s.max(1) as u64))
        .unwrap_or(DEFAULT_WATCH);
    // Bare numbers are the point to ask at, which defaults to somewhere along the titlebar a
    // client would draw for itself. A flag's own value is not one of them -- reading every
    // number in the command line made `--margin 26` mean "ask at 26" as well, which is a
    // confusing way to be given the wrong answer.
    let mut positional: Vec<i32> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg.starts_with("--") {
            rest.next();
        } else if let Ok(value) = arg.parse() {
            positional.push(value);
        }
    }
    let ask_at = match positional.as_slice() {
        [x, y, ..] => (*x, *y),
        _ => (margin + 60, margin + 15),
    };

    let connection = Connection::connect_to_env().expect("connect to the compositor");
    let display = connection.display();
    let mut queue = connection.new_event_queue::<Probe>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut probe = Probe::new(margin, ask_at);
    queue.roundtrip(&mut probe).expect("bind globals");

    let compositor = probe.compositor.clone().expect("wl_compositor");
    let wm_base = probe.wm_base.clone().expect("xdg_wm_base");
    let seat = probe.seat.clone().expect("wl_seat");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("com.wlrix.test-window-menu".into());
    toplevel.set_title("window menu probe".into());
    if margin > 0 {
        // What a client with a drop shadow declares: the window is this rectangle, and
        // everything outside it in the surface is decoration the compositor should ignore.
        let (width, height) = WINDOW;
        xdg_surface.set_window_geometry(margin, margin, width - 2 * margin, height - 2 * margin);
    }
    surface.commit();
    probe.surface = Some(surface);

    println!(
        "surface {}x{}, window geometry inset {margin}px, asking at surface-local {:?}",
        WINDOW.0, WINDOW.1, ask_at
    );

    // Round trips on a timer rather than `blocking_dispatch`, and that is not a style choice: a
    // mapped window that nothing is happening to sends no events at all, so a loop that waits
    // for one never wakes up to ask, and the probe sits there having done nothing. It looks
    // exactly like a compositor ignoring the request.
    let deadline = Instant::now() + ASK_AFTER + watch;
    while Instant::now() < deadline {
        queue.roundtrip(&mut probe).expect("dispatch");
        std::thread::sleep(Duration::from_millis(50));
        if !probe.asked && probe.painted && probe.start.elapsed() >= ASK_AFTER {
            // Serial 0: this compositor does not require the request to carry the serial of a
            // real button press, deliberately -- the protocol allows a key press or a touch as
            // the user action, and neither leaves a pointer grab to check against. A compositor
            // that does check will ignore this, which is its right and worth knowing.
            toplevel.show_window_menu(&seat, 0, ask_at.0, ask_at.1);
            let _ = connection.flush();
            probe.asked = true;
            println!(
                "[{:5.2}s] -> show_window_menu(seat, serial=0, {}, {})",
                probe.elapsed(),
                ask_at.0,
                ask_at.1
            );
            println!("           the menu is the compositor's to draw; look at the screen");
        }
    }
    println!("---");
    println!(
        "request sent: {}",
        if probe.asked {
            "yes"
        } else {
            "no -- never mapped"
        }
    );
}
