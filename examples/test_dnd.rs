// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `wl_data_device` drag-and-drop: starting a drag, tracking it across surfaces, and
//! delivering the payload on drop.
//!
//! wlRIX advertised `wl_data_device_manager` from the beginning, but only the clipboard half of
//! it worked. `dnd_requested` was left at smithay's default, which calls `source.cancel()`, so
//! every drag a client started was refused before a single `wl_data_device.enter` could reach a
//! target -- with no error anywhere, because nothing had failed. Clipboard and primary selection
//! kept working throughout, which is exactly what made it look implemented.
//!
//! Two panels appear along the top edge: a green **source** on the left and a blue **target** on
//! the right. Press the left mouse button on the green one, drag across to the blue one and let
//! go. The panels are deliberately not adjacent, so the trace also shows the drag crossing bare
//! desktop between them.
//!
//! What a working drag prints:
//!
//! ```text
//! button press on the source -- starting a drag
//! data_device.enter (mime text/plain offered)
//! data_device.motion x37
//! data_device.drop
//! source: send -- writing the payload
//! received: "wlrix drag payload"
//! source: dnd_drop_performed
//! source: dnd_finished
//! PASS: the drag reached a target and the payload arrived
//! ```
//!
//! The failure this was written for prints `source: canceled` with **no** `enter` line at all.
//! That is the signature: an immediate cancel and zero focus events.
//!
//! Source and target are two surfaces of one client, which is the arrangement that can be driven
//! by hand in one process. Smithay routes it through the same grab as a cross-client drag; the
//! difference is only which side reads the pipe.
//!
//! `--icon` attaches a drag icon surface to the drag. The compositor composites it under the
//! pointer, so it is also the check that the icon renders and keeps its frame callbacks -- a
//! frozen or absent square is the bug there.
//!
//! Usage: `cargo run --example test_dnd [--icon] [seconds]` with `WAYLAND_DISPLAY` set to the
//! compositor under test. Not part of the compositor; a dev tool only.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop, event_created_child,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
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

/// The one mime type this offers. Text, so any other client can be dragged onto as well.
const MIME: &str = "text/plain";
/// What the source writes when a target asks for the payload.
const PAYLOAD: &[u8] = b"wlrix drag payload";

/// Green: drag *from* here.
const SOURCE_TINT: u32 = 0xff30_9040;
/// Blue: drop *on* here.
const TARGET_TINT: u32 = 0xff30_5090;
/// Yellow, and small: the thing that should follow the pointer with `--icon`.
const ICON_TINT: u32 = 0xffd0_c020;

const PANEL: (u32, u32) = (300, 220);
const ICON: (i32, i32) = (64, 64);

/// Which panel a surface is, so the pointer handler can tell them apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Panel {
    Source,
    Target,
}

#[derive(Default)]
struct Probe {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    ddm: Option<WlDataDeviceManager>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    data_device: Option<WlDataDevice>,

    source_surface: Option<WlSurface>,
    target_surface: Option<WlSurface>,
    /// The surface the pointer is currently over.
    over: Option<Panel>,
    /// The serial of the drag's `enter`, which is what `accept` has to be answered with.
    offer_serial: u32,

    /// Whether `--icon` was passed, and the icon surface once built.
    want_icon: bool,
    icon_surface: Option<WlSurface>,

    drag: Option<WlDataSource>,
    /// The offer the compositor handed us as the drop target.
    offer: Option<WlDataOffer>,
    /// The scratch file a `receive` is writing into, until the payload lands in it.
    pending: Option<File>,
    /// Whether the source has answered the `send` and written the payload.
    payload_written: bool,

    // The trace, and the verdict.
    started: bool,
    enters: u32,
    motions: u32,
    dropped: bool,
    cancelled: bool,
    finished: bool,
    received: Option<String>,
    last_report: Option<Instant>,
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
            // Version 3 is what carries the DnD actions: without it there is no
            // `set_actions`, no `dnd_finished`, and a drop cannot be negotiated at all.
            "wl_data_device_manager" => probe.ddm = Some(registry.bind(name, 3, qh, ())),
            "wl_seat" => probe.seat = Some(registry.bind(name, 5, qh, ())),
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

impl Dispatch<ZwlrLayerSurfaceV1, Panel> for Probe {
    fn event(
        probe: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        panel: &Panel,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer.ack_configure(serial);
            let tint = match panel {
                Panel::Source => SOURCE_TINT,
                Panel::Target => TARGET_TINT,
            };
            let surface = match panel {
                Panel::Source => probe.source_surface.clone(),
                Panel::Target => probe.target_surface.clone(),
            };
            if let Some(surface) = surface {
                probe.paint(
                    qh,
                    &surface,
                    width.max(1) as i32,
                    height.max(1) as i32,
                    tint,
                );
            }
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
            wl_pointer::Event::Enter { surface, .. } => probe.over = probe.panel_of(&surface),
            wl_pointer::Event::Leave { .. } => probe.over = None,
            // A press on the green panel is the trigger. The serial handed back is the one
            // the compositor validated the implicit grab against -- `start_drag` with any
            // other serial is refused, which looks identical to the bug this tests for.
            wl_pointer::Event::Button {
                serial,
                state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } if probe.over == Some(Panel::Source) && !probe.started => {
                println!("button press on the source -- starting a drag");
                probe.start_drag(qh, serial);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, ()> for Probe {
    fn event(
        probe: &mut Self,
        _source: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // The failure signature. Before the compositor implemented `dnd_requested` this
            // arrived immediately, with `enters` still at zero.
            wl_data_source::Event::Cancelled => {
                probe.cancelled = true;
                println!("source: cancelled");
            }
            // A target asked for the payload: write it and close the fd, or the reader hangs.
            wl_data_source::Event::Send { mime_type, fd } => {
                println!("source: send -- writing the payload");
                if mime_type == MIME {
                    // `File::from` takes the descriptor, and dropping it at the end of this
                    // block closes it -- which is what tells the reader the write is finished.
                    let mut file = File::from(fd);
                    let _ = file.write_all(PAYLOAD);
                    let _ = file.flush();
                    probe.payload_written = true;
                } else {
                    drop::<OwnedFd>(fd);
                }
            }
            wl_data_source::Event::DndDropPerformed => println!("source: dnd_drop_performed"),
            wl_data_source::Event::DndFinished => {
                probe.finished = true;
                println!("source: dnd_finished");
            }
            wl_data_source::Event::Action { dnd_action } => {
                println!("source: action {dnd_action:?}");
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Probe {
    fn event(
        probe: &mut Self,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Offers arrive before the enter that uses them; hold onto the newest.
            wl_data_device::Event::DataOffer { id } => probe.offer = Some(id),
            wl_data_device::Event::Enter { serial, id, .. } => {
                probe.enters += 1;
                probe.offer_serial = serial;
                println!("data_device.enter (mime {MIME} offered)");
                // Accepting is not politeness: an offer left unaccepted is dropped with
                // `dnd_action = none`, and the source is told the drag achieved nothing.
                //
                // The serial is the one from *this* event. Answering with 0 is not "no serial
                // in particular" -- it is a serial the compositor never issued.
                if let Some(offer) = id.or_else(|| probe.offer.clone()) {
                    offer.accept(serial, Some(MIME.into()));
                    offer.set_actions(DndAction::Copy, DndAction::Copy);
                    probe.offer = Some(offer);
                }
            }
            wl_data_device::Event::Motion { .. } => {
                probe.motions += 1;
                let now = Instant::now();
                let due = probe
                    .last_report
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500));
                if due {
                    println!("data_device.motion x{}", probe.motions);
                    probe.last_report = Some(now);
                }
            }
            wl_data_device::Event::Leave => println!("data_device.leave"),
            wl_data_device::Event::Drop => {
                probe.dropped = true;
                println!("data_device.drop");
                probe.receive_payload();
            }
            _ => {}
        }
    }

    // `data_offer` is an event that *creates an object*, and wayland-client will not invent the
    // user data for one. Without this the library panics the moment the first offer arrives --
    // and because it panics inside libwayland's dispatch callback, which is `extern "C"` and
    // cannot unwind, the process aborts rather than unwinding to anything that could report it.
    //
    // Worth knowing where in the drag that lands: the compositor only sends `data_offer` once
    // the drag has entered a surface, so reaching this panic at all means the grab installed and
    // the drag was being tracked. It read as a compositor crash and was a client omission.
    event_created_child!(Probe, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for Probe {
    fn event(
        _probe: &mut Self,
        _offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            println!("  offer advertises {mime_type}");
        }
    }
}

impl Probe {
    fn panel_of(&self, surface: &WlSurface) -> Option<Panel> {
        if self.source_surface.as_ref() == Some(surface) {
            Some(Panel::Source)
        } else if self.target_surface.as_ref() == Some(surface) {
            Some(Panel::Target)
        } else {
            None
        }
    }

    /// Build a source, offer one mime type, and hand it to `start_drag`.
    fn start_drag(&mut self, qh: &QueueHandle<Self>, serial: u32) {
        // Cloned rather than borrowed: building the icon below needs `&mut self`, and these
        // are all cheap handle clones.
        let (Some(ddm), Some(device), Some(origin)) = (
            self.ddm.clone(),
            self.data_device.clone(),
            self.source_surface.clone(),
        ) else {
            return;
        };
        let source = ddm.create_data_source(qh, ());
        source.offer(MIME.into());
        source.set_actions(DndAction::Copy);

        // The icon is optional in the protocol and the usual case is `None`. When asked for,
        // it is a plain surface with a buffer and no role of its own -- `start_drag` gives it
        // the `dnd_icon` role, and the compositor draws it at the pointer from then on.
        let icon = if self.want_icon {
            self.build_icon(qh)
        } else {
            None
        };
        device.start_drag(Some(&source), &origin, icon.as_ref(), serial);
        self.icon_surface = icon;
        self.drag = Some(source);
        self.started = true;
    }

    /// A small opaque square for `--icon`.
    fn build_icon(&mut self, qh: &QueueHandle<Self>) -> Option<WlSurface> {
        let compositor = self.compositor.as_ref()?;
        let surface = compositor.create_surface(qh, ());
        self.paint(qh, &surface, ICON.0, ICON.1, ICON_TINT);
        Some(surface)
    }

    /// Asks the source for the payload. Reading it happens later, in [`Self::collect_payload`].
    ///
    /// A scratch file rather than a pipe: the offer writes into whatever descriptor it is given,
    /// and a file cannot deadlock on a source that writes more than a pipe buffer holds before
    /// anyone reads.
    ///
    /// **Requesting and reading have to be separated**, and the reason is specific to this
    /// arrangement. Source and target are the same client on the same thread, so the write that
    /// answers `receive` arrives as a `wl_data_source.send` event on *our own* queue -- it cannot
    /// happen until this handler has returned and the loop dispatches again. Reading here, or
    /// sleeping here and then reading, waits for an event that by construction cannot arrive: it
    /// reports an empty payload against a compositor that did everything right.
    fn receive_payload(&mut self) {
        let Some(offer) = self.offer.as_ref() else {
            return;
        };
        let Some(file) = tempfile("payload") else {
            return;
        };
        offer.receive(MIME.into(), file.as_fd());
        self.pending = Some(file);
    }

    /// Reads the payload once the source has written it, and closes the transfer.
    fn collect_payload(&mut self) {
        if !self.payload_written {
            return;
        }
        let Some(mut file) = self.pending.take() else {
            return;
        };

        let _ = file.seek(SeekFrom::Start(0));
        let mut text = String::new();
        if file.read_to_string(&mut text).is_ok() && !text.is_empty() {
            println!("received: {text:?}");
            self.received = Some(text);
        }

        // `finish` after the transfer, never before: it ends the drag, and ending it while the
        // source still owes us bytes is what the protocol's `invalid_finish` error is for.
        if let Some(offer) = self.offer.as_ref() {
            offer.finish();
        }
    }

    /// Fill `surface` with `tint` and commit it.
    fn paint(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &WlSurface,
        width: i32,
        height: i32,
        tint: u32,
    ) {
        let Some(shm) = self.shm.as_ref() else {
            return;
        };
        let stride = width * 4;
        let len = (stride * height) as usize;

        let Some(mut file) = tempfile("shm") else {
            return;
        };
        let pixels: Vec<u8> = tint
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

/// An unlinked temporary file, for shm pools and for catching the dropped payload.
fn tempfile(tag: &str) -> Option<File> {
    let path = std::env::temp_dir().join(format!("wlrix-test-dnd-{tag}-{}", std::process::id()));
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
delegate_noop!(Probe: ignore WlDataDeviceManager);

fn main() {
    let mut want_icon = false;
    let mut hold = Duration::from_secs(60);
    for arg in std::env::args().skip(1) {
        if arg == "--icon" {
            want_icon = true;
        } else if let Ok(secs) = arg.parse::<u64>() {
            hold = Duration::from_secs(secs);
        }
    }

    let connection = Connection::connect_to_env().expect("no Wayland display");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let display = connection.display();
    let _registry = display.get_registry(&qh, ());

    let mut probe = Probe {
        want_icon,
        ..Default::default()
    };
    queue.roundtrip(&mut probe).expect("registry roundtrip");
    // A second pass: the seat's capabilities arrive after the bind, and the pointer with them.
    queue.roundtrip(&mut probe).expect("seat roundtrip");

    let compositor = probe.compositor.clone().expect("no wl_compositor");
    let layer_shell = probe.layer_shell.clone().expect("no zwlr_layer_shell_v1");
    let ddm = probe
        .ddm
        .clone()
        .expect("no wl_data_device_manager -- drag-and-drop is not advertised at all");
    let seat = probe.seat.clone().expect("no wl_seat");
    probe.pointer.clone().expect("no wl_pointer");

    probe.data_device = Some(ddm.get_data_device(&seat, &qh, ()));

    for (panel, anchor) in [
        (Panel::Source, Anchor::Top | Anchor::Left),
        (Panel::Target, Anchor::Top | Anchor::Right),
    ] {
        let surface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Top,
            "wlrix-test-dnd".into(),
            &qh,
            panel,
        );
        layer.set_anchor(anchor);
        layer.set_exclusive_zone(0);
        // The target must take pointer input while a drag is over it, but neither panel wants
        // the keyboard -- taking it would steal focus from whatever is underneath.
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(PANEL.0, PANEL.1);
        surface.commit();
        match panel {
            Panel::Source => probe.source_surface = Some(surface),
            Panel::Target => probe.target_surface = Some(surface),
        }
    }

    println!(
        "green panel top-left, blue panel top-right. Press the left button on the green one, \
         drag to the blue one and release. Giving up in {hold:?}."
    );
    if want_icon {
        println!("--icon: a yellow square should follow the pointer for the whole drag.");
    }

    let deadline = Instant::now() + hold;
    loop {
        // Everything interesting is over once the source has been told the drop finished, or
        // once it has been refused. `dnd_finished` is the compositor's answer to our `finish`,
        // so by the time it arrives the payload has already been collected.
        if probe.finished || probe.cancelled {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        queue.flush().expect("flush");
        match queue.prepare_read() {
            Some(guard) if readable(&connection, remaining.min(Duration::from_millis(200))) => {
                let _ = guard.read();
            }
            Some(_) => {}
            None => {}
        }
        queue.dispatch_pending(&mut probe).expect("dispatch");
        // After dispatching, so the source's `send` has been handled and the payload is in the
        // file by the time this looks at it.
        probe.collect_payload();
    }
    let _ = queue.roundtrip(&mut probe);
    probe.collect_payload();

    println!(
        "\ndone: {} enter, {} motion, drop={}, cancelled={}, payload={:?}",
        probe.enters, probe.motions, probe.dropped, probe.cancelled, probe.received
    );
    if !probe.started {
        println!("INCONCLUSIVE: no drag was ever started, so nothing was tested");
    } else if probe.enters == 0 && probe.cancelled {
        println!(
            "FAIL: the drag was cancelled without reaching any target -- the compositor is \
             refusing `start_drag` (smithay's default `dnd_requested`)"
        );
    } else if probe.received.is_some() {
        println!("PASS: the drag reached a target and the payload arrived");
    } else if probe.enters > 0 {
        println!(
            "PARTIAL: the drag was tracked ({} enter events) but no payload arrived -- \
             the grab works, the transfer does not",
            probe.enters
        );
    } else {
        println!("INCONCLUSIVE: the drag started but never entered a surface");
    }
}
