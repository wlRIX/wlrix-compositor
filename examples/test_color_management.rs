// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises `wp_color_manager_v1`: what the compositor says each output is.
//!
//! Binds the manager, records the feature set it advertises, then fetches and prints every
//! output's image description. Asserts rather than just demonstrates -- the perceptual intent
//! and the sRGB primaries are mandatory, and an output that claims PQ has to come with a
//! luminance range, or a client has nothing to encode against.
//!
//! With `hdr = true` set on an output this is how to check, without trusting the monitor's OSD,
//! that the compositor and the panel agree about what is being sent.
//!
//! With `tag` it also drives the *write* half: builds a PQ / BT.2020 image description through
//! the parametric creator and sets it on a surface, which is what an HDR video player does. That
//! path has no visible effect on an SDR output, so the check is that it is accepted without a
//! protocol error and the description goes ready.
//!
//! With `show [seconds]` the tagged surface becomes a visible 256x256 window, painted with the PQ
//! code value of exactly its own 203-nit reference white (148/255). That gives a capture a known
//! right answer: a screenshot is an SDR image, the compositor should tone-map this surface into
//! it, and reference white has to come out **white** (255). Pixels copied raw come out 148 -- the
//! grey tint Chromium windows had in screenshots of an HDR output, since Chromium renders and
//! tags PQ there. Leave it up and point `grim` (or `wlrix-screenshot`) at it.
//!
//! Usage, with `WAYLAND_DISPLAY` pointing at the compositor under test:
//!   cargo run --example test_color_management
//!   cargo run --example test_color_management -- tag
//!   cargo run --example test_color_management -- show 20
//!
//! Not part of the compositor; a dev tool only.

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
    },
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{
        self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
    },
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::XdgToplevel,
    xdg_wm_base::{self, XdgWmBase},
};

/// One output's description, filled in as the info events arrive.
#[derive(Default)]
struct Described {
    name: String,
    primaries: Option<Primaries>,
    tf: Option<TransferFunction>,
    /// min (cd/m² * 10000), max (cd/m²), reference white (cd/m²).
    luminances: Option<(u32, u32, u32)>,
    target_luminance: Option<(u32, u32)>,
    max_cll: Option<u32>,
    identity: Option<u32>,
    complete: bool,
}

#[derive(Default)]
struct App {
    compositor: Option<wayland_client::protocol::wl_compositor::WlCompositor>,
    manager: Option<WpColorManagerV1>,
    intents: Vec<RenderIntent>,
    features: Vec<Feature>,
    transfer_functions: Vec<TransferFunction>,
    primaries: Vec<Primaries>,
    /// Set once the manager's `done` has arrived and the advertised set is complete.
    manager_done: bool,
    outputs: Vec<(WlOutput, usize)>,
    described: Vec<Described>,
    /// Only for `show`: what a visible window needs on top of a tagged surface.
    shm: Option<wayland_client::protocol::wl_shm::WlShm>,
    wm_base: Option<XdgWmBase>,
    /// The serial of the last `xdg_surface.configure`, until it is acknowledged.
    configure: Option<u32>,
}

impl App {
    fn entry(&mut self, index: usize) -> &mut Described {
        &mut self.described[index]
    }

    fn report(&self) {
        println!("--- wp_color_manager_v1 ---");
        println!("  intents:   {:?}", self.intents);
        println!("  features:  {:?}", self.features);
        println!("  transfer:  {:?}", self.transfer_functions);
        println!("  primaries: {:?}", self.primaries);
        for output in &self.described {
            println!("--- output {} ---", output.name);
            println!("  identity:  {:?}", output.identity);
            println!("  primaries: {:?}", output.primaries);
            println!("  transfer:  {:?}", output.tf);
            match output.luminances {
                Some((min, max, reference)) => println!(
                    "  luminance: {:.4}..{} cd/m^2, reference white {} cd/m^2",
                    min as f64 / 10_000.0,
                    max,
                    reference
                ),
                None => println!("  luminance: (none)"),
            }
            if let Some((min, max)) = output.target_luminance {
                println!(
                    "  mastering: {:.4}..{} cd/m^2, maxCLL {:?}",
                    min as f64 / 10_000.0,
                    max,
                    output.max_cll
                );
            }
            let hdr = output.tf == Some(TransferFunction::St2084Pq);
            println!("  => {}", if hdr { "HDR (PQ)" } else { "SDR" });
        }
    }

    /// What has to hold whatever the hardware is.
    fn check(&self) {
        assert!(self.manager_done, "the manager never sent `done`");
        assert!(
            self.intents.contains(&RenderIntent::Perceptual),
            "the perceptual render intent is mandatory"
        );
        assert!(
            self.primaries.contains(&Primaries::Srgb),
            "sRGB primaries must be advertised"
        );
        assert!(
            !self.described.is_empty(),
            "no outputs described -- is anything connected?"
        );

        for output in &self.described {
            assert!(output.complete, "{}: info never completed", output.name);
            assert!(
                output.primaries.is_some(),
                "{}: no primaries -- a client cannot encode for this output",
                output.name
            );
            assert!(output.tf.is_some(), "{}: no transfer function", output.name);
            assert!(
                output.identity.is_some(),
                "{}: the description never became ready",
                output.name
            );

            if output.tf == Some(TransferFunction::St2084Pq) {
                // A PQ output without a luminance range is useless to a client: there is
                // nothing to tone-map against.
                let (min, max, reference) = output
                    .luminances
                    .unwrap_or_else(|| panic!("{}: PQ with no luminance range", output.name));
                assert!(
                    max > 0 && reference > 0 && reference <= max,
                    "{}: implausible luminances {min}/{max}/{reference}",
                    output.name
                );
                assert!(
                    self.transfer_functions
                        .contains(&TransferFunction::St2084Pq),
                    "an output is driven in PQ but PQ is not advertised as supported"
                );
                assert_eq!(
                    output.primaries,
                    Some(Primaries::Bt2020),
                    "{}: PQ content is expected in BT.2020",
                    output.name
                );
            }
        }
        println!("\nall checks passed");
    }
}

fn main() {
    let connection = Connection::connect_to_env().expect("no WAYLAND_DISPLAY");
    let display = connection.display();
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    display.get_registry(&handle, ());

    let mut app = App::default();
    // Registry, then the manager's advertisement burst.
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let Some(manager) = app.manager.clone() else {
        panic!("the compositor does not advertise wp_color_manager_v1");
    };
    queue.roundtrip(&mut app).expect("manager roundtrip");

    // Ask every output what it is. Each answer takes two more roundtrips: the description has
    // to go ready before its information can be fetched.
    for (index, (output, slot)) in app.outputs.clone().into_iter().enumerate() {
        let managed = manager.get_output(&output, &handle, slot);
        let description = managed.get_image_description(&handle, slot);
        let _ = index;
        let _ = description;
    }
    queue.roundtrip(&mut app).expect("description roundtrip");
    queue.roundtrip(&mut app).expect("information roundtrip");

    app.report();
    app.check();

    match std::env::args().nth(1).as_deref() {
        Some("tag") => {
            tag_a_surface(&connection, &mut queue, &handle, &manager, &mut app);
        }
        Some("show") => {
            let seconds = std::env::args()
                .nth(2)
                .and_then(|arg| arg.parse().ok())
                .unwrap_or(20);
            show_a_surface(&mut queue, &handle, &manager, &mut app, seconds);
        }
        _ => {}
    }
}

/// The PQ (SMPTE ST 2084) code value for `nits`, 0..=1.
///
/// Worked out here rather than hard-coded so the number the capture is checked against has a
/// derivation next to it.
fn pq_encode(nits: f64) -> f64 {
    let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
    let (c1, c2, c3) = (
        3424.0 / 4096.0,
        2413.0 / 4096.0 * 32.0,
        2392.0 / 4096.0 * 32.0,
    );
    let y = (nits / 10_000.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// A visible window tagged PQ / BT.2020, painted with its own reference white.
fn show_a_surface(
    queue: &mut wayland_client::EventQueue<App>,
    handle: &QueueHandle<App>,
    manager: &WpColorManagerV1,
    app: &mut App,
    seconds: u64,
) {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsFd;

    const SIDE: i32 = 256;
    const REFERENCE_NITS: u32 = 203;

    let compositor = app.compositor.clone().expect("no wl_compositor");
    let shm = app.shm.clone().expect("no wl_shm");
    let wm_base = app.wm_base.clone().expect("no xdg_wm_base");

    let surface = compositor.create_surface(handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, handle, ());
    let toplevel = xdg_surface.get_toplevel(handle, ());
    toplevel.set_app_id("com.wlrix.test-pq".into());
    toplevel.set_title("PQ reference white".into());

    let slot = app.described.len();
    app.described.push(Described {
        name: "client-shown".into(),
        ..Default::default()
    });
    let creator = manager.create_parametric_creator(handle, ());
    creator.set_tf_named(TransferFunction::St2084Pq);
    creator.set_primaries_named(Primaries::Bt2020);
    creator.set_luminances(1, 1000, REFERENCE_NITS);
    let description = creator.create(handle, slot);
    queue.roundtrip(app).expect("description roundtrip");
    assert!(
        app.described[slot].identity.is_some(),
        "the description never became ready"
    );

    let managed = manager.get_surface(&surface, handle, ());
    managed.set_image_description(&description, RenderIntent::Perceptual);
    surface.commit();
    queue.roundtrip(app).expect("initial configure");
    let serial = app.configure.take().expect("no initial configure");
    xdg_surface.ack_configure(serial);

    // Grey in PQ terms, white in the content's own terms: reference white, encoded.
    let code = (pq_encode(f64::from(REFERENCE_NITS)) * 255.0).round() as u8;
    let len = (SIDE * SIDE * 4) as usize;
    let path = std::env::temp_dir().join(format!("wlrix-test-pq-{}", std::process::id()));
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("temp file");
    let _ = std::fs::remove_file(&path);
    // XRGB8888, little-endian: B, G, R, X.
    let pixels: Vec<u8> = [code, code, code, 0xff]
        .iter()
        .cycle()
        .take(len)
        .copied()
        .collect();
    file.write_all(&pixels).expect("write pixels");
    file.seek(SeekFrom::Start(0)).expect("rewind");
    let pool = shm.create_pool(file.as_fd(), len as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        SIDE,
        SIDE,
        SIDE * 4,
        wayland_client::protocol::wl_shm::Format::Xrgb8888,
        handle,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, SIDE, SIDE);
    surface.commit();
    queue.roundtrip(app).expect("map");

    println!(
        "\nshowing a {SIDE}x{SIDE} PQ / BT.2020 window painted {code}/255 -- the PQ code value of \
         its own {REFERENCE_NITS}-nit reference white.\n\
         A correct SDR capture shows it as white (255). A raw copy shows it as {code} (grey).\n\
         Up for {seconds}s."
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < until {
        queue.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = queue.dispatch_pending(app);
        if let Some(guard) = queue.prepare_read() {
            let _ = guard.read();
        }
        let _ = queue.dispatch_pending(app);
        if let Some(serial) = app.configure.take() {
            xdg_surface.ack_configure(serial);
            surface.commit();
        }
    }
}

/// Build a PQ / BT.2020 description and set it on a surface, as an HDR video player would.
fn tag_a_surface(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<App>,
    handle: &QueueHandle<App>,
    manager: &WpColorManagerV1,
    app: &mut App,
) {
    let compositor = app
        .compositor
        .clone()
        .expect("the compositor does not advertise wl_compositor");
    let surface = compositor.create_surface(handle, ());

    // A slot of its own, appended after the outputs', so the info events land somewhere.
    let slot = app.described.len();
    app.described.push(Described {
        name: "client-tagged".into(),
        ..Default::default()
    });

    let creator = manager.create_parametric_creator(handle, ());
    creator.set_tf_named(TransferFunction::St2084Pq);
    creator.set_primaries_named(Primaries::Bt2020);
    // What a 1000-nit-capable master would say: 0.0001 to 1000 cd/m^2, reference white at 203.
    creator.set_luminances(1, 1000, 203);
    // `create` is a destructor request, so the creator is spent here.
    let description = creator.create(handle, slot);
    queue.roundtrip(app).expect("description roundtrip");

    assert!(
        app.described[slot].identity.is_some(),
        "the parametric description never became ready"
    );

    let managed = manager.get_surface(&surface, handle, ());
    managed.set_image_description(&description, RenderIntent::Perceptual);
    surface.commit();
    queue
        .roundtrip(app)
        .expect("set_image_description roundtrip");

    // Any protocol error would have disconnected us by now; a live connection is the check.
    connection.display().sync(handle, ());
    queue.roundtrip(app).expect("still connected after tagging");

    println!(
        "\ntagged a surface as PQ / BT.2020 (identity {:?}) -- accepted",
        app.described[slot].identity
    );
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wp_color_manager_v1" => {
                // Bind at 1 deliberately: this probe is checking what a v1 client sees.
                app.manager =
                    Some(registry.bind::<WpColorManagerV1, _, _>(name, version.min(1), handle, ()));
            }
            "wl_compositor" => {
                app.compositor = Some(
                    registry.bind::<wayland_client::protocol::wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(4),
                        handle,
                        (),
                    ),
                );
            }
            "wl_shm" => {
                app.shm = Some(registry.bind(name, 1, handle, ()));
            }
            "xdg_wm_base" => {
                app.wm_base = Some(registry.bind(name, 1, handle, ()));
            }
            "wl_output" => {
                let slot = app.described.len();
                app.described.push(Described {
                    name: format!("wl_output@{name}"),
                    ..Default::default()
                });
                let output = registry.bind::<WlOutput, _, _>(name, version.min(4), handle, slot);
                app.outputs.push((output, slot));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for App {
    fn event(
        app: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The connector name is what the config file keys on, so it is what should be printed.
        if let wl_output::Event::Name { name } = event {
            app.entry(*slot).name = name;
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &WpColorManagerV1,
        event: wp_color_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_color_manager_v1::Event::SupportedIntent { render_intent } => {
                if let Ok(intent) = render_intent.into_result() {
                    app.intents.push(intent);
                }
            }
            wp_color_manager_v1::Event::SupportedFeature { feature } => {
                if let Ok(feature) = feature.into_result() {
                    app.features.push(feature);
                }
            }
            wp_color_manager_v1::Event::SupportedTfNamed { tf } => {
                if let Ok(tf) = tf.into_result() {
                    app.transfer_functions.push(tf);
                }
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed { primaries } => {
                if let Ok(primaries) = primaries.into_result() {
                    app.primaries.push(primaries);
                }
            }
            wp_color_manager_v1::Event::Done => app.manager_done = true,
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementOutputV1, usize> for App {
    fn event(
        _: &mut Self,
        _: &WpColorManagementOutputV1,
        _: wp_color_management_output_v1::Event,
        _: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `image_description_changed` only says "ask again", which this one-shot probe does not.
    }
}

impl Dispatch<WpImageDescriptionV1, usize> for App {
    fn event(
        app: &mut Self,
        description: &WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        slot: &usize,
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        match event {
            wp_image_description_v1::Event::Ready { identity } => {
                app.entry(*slot).identity = Some(identity);
                // Only now may the contents be asked for.
                description.get_information(handle, *slot);
            }
            wp_image_description_v1::Event::Failed { cause, msg } => {
                panic!("image description failed: {cause:?}: {msg}");
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, usize> for App {
    fn event(
        app: &mut Self,
        _: &WpImageDescriptionInfoV1,
        event: wp_image_description_info_v1::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let slot = *slot;
        match event {
            wp_image_description_info_v1::Event::PrimariesNamed { primaries } => {
                app.entry(slot).primaries = primaries.into_result().ok();
            }
            wp_image_description_info_v1::Event::TfNamed { tf } => {
                app.entry(slot).tf = tf.into_result().ok();
            }
            wp_image_description_info_v1::Event::Luminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                app.entry(slot).luminances = Some((min_lum, max_lum, reference_lum));
            }
            wp_image_description_info_v1::Event::TargetLuminance { min_lum, max_lum } => {
                app.entry(slot).target_luminance = Some((min_lum, max_lum));
            }
            wp_image_description_info_v1::Event::TargetMaxCll { max_cll } => {
                app.entry(slot).max_cll = Some(max_cll);
            }
            wp_image_description_info_v1::Event::Done => {
                app.entry(slot).complete = true;
            }
            _ => {}
        }
    }
}

// The write half needs a surface to tag, and these interfaces carry no events this probe acts on.
impl Dispatch<wayland_client::protocol::wl_compositor::WlCompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_compositor::WlCompositor,
        _: wayland_client::protocol::wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_surface::WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_surface::WlSurface,
        _: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for App {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpImageDescriptionCreatorParamsV1,
        _: wp_image_description_creator_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The creator has no events; every parameter is validated as it is sent.
    }
}

impl Dispatch<WpColorManagementSurfaceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpColorManagementSurfaceV1,
        _: wp_color_management_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// `show` only: a window needs a role and a buffer, and only the ping and configure matter.
impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            app.configure = Some(serial);
        }
    }
}

wayland_client::delegate_noop!(App: ignore XdgToplevel);
wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_buffer::WlBuffer);
