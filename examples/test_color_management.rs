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
//! Usage, with `WAYLAND_DISPLAY` pointing at the compositor under test:
//!   cargo run --example test_color_management
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
    wp_color_manager_v1::{
        self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
    },
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
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
    manager: Option<WpColorManagerV1>,
    intents: Vec<RenderIntent>,
    features: Vec<Feature>,
    transfer_functions: Vec<TransferFunction>,
    primaries: Vec<Primaries>,
    /// Set once the manager's `done` has arrived and the advertised set is complete.
    manager_done: bool,
    outputs: Vec<(WlOutput, usize)>,
    described: Vec<Described>,
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
