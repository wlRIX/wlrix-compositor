// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `ext-idle-notify-v1` and `zwp_idle_inhibit_manager_v1`.
//!
//! No idle daemon is installed on a dev machine, and the interesting part of the idle
//! code is timing, which a compile cannot check. This asks for a short notification and
//! waits to be told it went idle, then repeats the whole thing behind an idle inhibitor,
//! where it must *not* be told.
//!
//! Usage: `cargo run --example test_idle` with `WAYLAND_DISPLAY` set to the compositor
//! under test. Exits non-zero on failure. Not part of the compositor; a dev tool only.
//!
//! Note that `resumed` is not covered: it needs real input, which cannot be injected
//! from here.

use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    ext::idle_notify::v1::client::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::ExtIdleNotifierV1,
    },
    wp::idle_inhibit::zv1::client::{
        zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
        zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
    },
};

/// Short enough to keep the test quick, long enough not to race the compositor's own
/// startup work.
const TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Default)]
struct Idle {
    compositor: Option<WlCompositor>,
    notifier: Option<ExtIdleNotifierV1>,
    inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    seat: Option<WlSeat>,
    idled: bool,
    resumed: bool,
}

impl Dispatch<WlRegistry, ()> for Idle {
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
            "wl_seat" => state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ())),
            "ext_idle_notifier_v1" => {
                state.notifier = Some(registry.bind::<ExtIdleNotifierV1, _, _>(name, 1, qh, ()))
            }
            "zwp_idle_inhibit_manager_v1" => {
                state.inhibit_manager =
                    Some(registry.bind::<ZwpIdleInhibitManagerV1, _, _>(name, 1, qh, ()))
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for Idle {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                println!("  idled");
                state.idled = true;
            }
            ext_idle_notification_v1::Event::Resumed => {
                println!("  resumed");
                state.resumed = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(Idle: ignore WlCompositor);
delegate_noop!(Idle: ignore WlSurface);
delegate_noop!(Idle: ignore WlSeat);
delegate_noop!(Idle: ignore ExtIdleNotifierV1);
delegate_noop!(Idle: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(Idle: ignore ZwpIdleInhibitorV1);

/// Pump the queue until `done` or `limit` passes.
fn pump(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<Idle>,
    state: &mut Idle,
    limit: Duration,
    done: fn(&Idle) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < limit && !done(state) {
        // Bounded rather than blocking: the interesting outcome in the inhibited case
        // is that *nothing* arrives, and a blocking dispatch would simply hang.
        conn.flush().unwrap();
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read_without_dispatch();
        }
        queue.dispatch_pending(state).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("no compositor to connect to");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = Idle::default();
    queue.roundtrip(&mut state).unwrap();
    queue.roundtrip(&mut state).unwrap();

    let notifier = state
        .notifier
        .clone()
        .expect("compositor does not offer ext_idle_notifier_v1");
    let seat = state.seat.clone().expect("no wl_seat");
    let mut failures = 0;

    // 1. Plain notification: must go idle on its own.
    println!("waiting to go idle (timeout {TIMEOUT:?})");
    let notification = notifier.get_idle_notification(TIMEOUT.as_millis() as u32, &seat, &qh, ());
    queue.roundtrip(&mut state).unwrap();
    pump(&conn, &mut queue, &mut state, TIMEOUT * 4, |state| {
        state.idled
    });
    if state.idled {
        println!("PASS: idled");
    } else {
        eprintln!("FAIL: never went idle");
        failures += 1;
    }
    notification.destroy();
    queue.roundtrip(&mut state).unwrap();

    // 2. Same again, but inhibited: must *not* go idle.
    let (Some(inhibit_manager), Some(compositor)) =
        (state.inhibit_manager.clone(), state.compositor.clone())
    else {
        eprintln!("FAIL: compositor does not offer zwp_idle_inhibit_manager_v1");
        std::process::exit(1);
    };

    println!("waiting again, with an inhibitor held");
    let surface = compositor.create_surface(&qh, ());
    let inhibitor = inhibit_manager.create_inhibitor(&surface, &qh, ());
    queue.roundtrip(&mut state).unwrap();

    state.idled = false;
    let notification = notifier.get_idle_notification(TIMEOUT.as_millis() as u32, &seat, &qh, ());
    queue.roundtrip(&mut state).unwrap();
    pump(&conn, &mut queue, &mut state, TIMEOUT * 3, |state| {
        state.idled
    });
    if state.idled {
        eprintln!("FAIL: went idle while an inhibitor was held");
        failures += 1;
    } else {
        println!("PASS: stayed awake while inhibited");
    }

    // 3. Drop the inhibitor: it must be able to go idle again.
    println!("releasing the inhibitor");
    inhibitor.destroy();
    queue.roundtrip(&mut state).unwrap();
    pump(&conn, &mut queue, &mut state, TIMEOUT * 4, |state| {
        state.idled
    });
    if state.idled {
        println!("PASS: idled after the inhibitor was released");
    } else {
        eprintln!("FAIL: never went idle after the inhibitor was released");
        failures += 1;
    }
    notification.destroy();
    queue.roundtrip(&mut state).unwrap();

    if failures > 0 {
        eprintln!("{failures} failure(s)");
        std::process::exit(1);
    }
    println!("OK");
}
