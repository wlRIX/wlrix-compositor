// SPDX-License-Identifier: GPL-3.0-or-later
//! `wp_color_manager_v1`: telling clients what each output actually is.
//!
//! Clients ask two things of this protocol. What color is this screen? -- so a video player or
//! a color-managed application can convert its content appropriately. And: here is what color
//! *my* content is -- so the compositor can convert it instead.
//!
//! This implements the first honestly and records the second. An output that
//! [`crate::hdr`] has switched into HDR is described as PQ / BT.2020 with the panel's real
//! luminance range; an SDR one as sRGB. What a client says about its own surface is stored, the
//! way `wp_content_type_v1`'s tag is stored, and nothing reads it yet: acting on it needs
//! per-surface decode shaders and linear compositing, which is the next milestone. Advertising
//! the protocol while ignoring client descriptions is explicitly allowed -- a compositor is
//! never obliged to honor a description, only to be truthful about its outputs.
//!
//! Unlike most of the hand-written protocol code here, the *bindings* come free: the
//! `wayland-protocols` version smithay already pins ships `wp::color_management::v1::server`
//! under its `staging` feature. Only the dispatch is ours.
//!
//! Bound at **version 1**. The rule the roadmap records the hard way -- `damage` disconnecting
//! `grim`, `make`/`model` disconnecting `wlr-output-management` -- applies here too: everything
//! from version 2 on (`ready2`, `preferred_changed2`, `get_image_description` on a reference,
//! `create_windows_bt2100`) is simply not reachable at the version advertised, which is the
//! cheapest way to be sure it is never sent.

use std::sync::Mutex;

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::wp::color_management::v1::server::{
            wp_color_management_output_v1::{self, WpColorManagementOutputV1},
            wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
            wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
            wp_color_manager_v1::{
                self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
            },
            wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
            wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
            wp_image_description_info_v1::WpImageDescriptionInfoV1,
            wp_image_description_v1::{self, WpImageDescriptionV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId, protocol::wl_surface::WlSurface,
        },
    },
    wayland::{compositor::with_states, seat::WaylandFocus},
};
use tracing::info;

use crate::Wlrix;

/// Version 1. See the module docs for why this does not move with the protocol.
const VERSION: u32 = 1;

/// Chromaticities cross the wire as x/y * 1000000.
const CHROMA_SCALE: f32 = 1_000_000.0;
/// Minimum luminances cross the wire as cd/m² * 10000; maxima as plain cd/m².
const MIN_LUM_SCALE: f32 = 10_000.0;

/// What an image description says.
///
/// Only the parametric subset wlRIX can actually describe or be told about. An ICC profile is a
/// description too, and is deliberately not supported -- `icc_v2_v4` is not advertised, so a
/// client cannot ask for one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Description {
    pub primaries: Primaries,
    pub tf: TransferFunction,
    /// Minimum luminance, cd/m² * 10000.
    pub min_luminance: u32,
    /// Maximum luminance, cd/m².
    pub max_luminance: u32,
    /// Reference white, cd/m².
    pub reference_luminance: u32,
    /// The mastering display, when the description came from a real panel.
    pub mastering: Option<crate::hdr::Mastering>,
}

impl Description {
    /// The sRGB description: what an SDR output is, and the default for untagged content.
    ///
    /// The luminances are the protocol's own defaults for sRGB -- 0.2 to 80 cd/m² with
    /// reference white at 80 -- not a measurement of anything. That is the honest answer for a
    /// panel wlRIX has not been told the brightness of.
    fn srgb() -> Self {
        Self {
            primaries: Primaries::Srgb,
            tf: TransferFunction::Srgb,
            min_luminance: 2_000,
            max_luminance: 80,
            reference_luminance: 80,
            mastering: None,
        }
    }

    /// What an output currently is.
    ///
    /// Under the nested backend, and on any output not switched into HDR, this is sRGB -- which
    /// is the truth, not a placeholder.
    pub fn for_output(state: &Wlrix, output: &Output) -> Self {
        if !state.hdr.active(output) {
            return Self::srgb();
        }
        let mastering = state.hdr.mastering(output);
        Self {
            primaries: Primaries::Bt2020,
            tf: TransferFunction::St2084Pq,
            min_luminance: mastering
                .map(|m| (m.min_luminance * MIN_LUM_SCALE).round() as u32)
                .unwrap_or(0),
            max_luminance: mastering
                .map(|m| m.max_luminance.round() as u32)
                .unwrap_or(10_000),
            // SDR white is where the desktop's white lands, which is exactly what a client
            // needs in order to match it.
            reference_luminance: state.hdr.sdr_white(output).round() as u32,
            mastering,
        }
    }

    /// Send this description's contents over a `wp_image_description_info_v1`.
    fn describe(&self, info: &WpImageDescriptionInfoV1) {
        info.primaries_named(self.primaries);
        info.tf_named(self.tf);
        info.luminances(
            self.min_luminance,
            self.max_luminance,
            self.reference_luminance,
        );

        if let Some(mastering) = self.mastering {
            let coord = |value: f32| (value * CHROMA_SCALE).round() as i32;
            info.target_primaries(
                coord(mastering.red.x),
                coord(mastering.red.y),
                coord(mastering.green.x),
                coord(mastering.green.y),
                coord(mastering.blue.x),
                coord(mastering.blue.y),
                coord(mastering.white.x),
                coord(mastering.white.y),
            );
            info.target_luminance(
                (mastering.min_luminance * MIN_LUM_SCALE).round() as u32,
                mastering.max_luminance.round() as u32,
            );
            info.target_max_cll(mastering.max_luminance.round() as u32);
            info.target_max_fall(mastering.max_frame_average.round() as u32);
        }

        // `done` is a destructor event: the info object is finished once it has been sent.
        info.done();
    }
}

/// Protocol state: who is watching what, and the identities handed out so far.
#[derive(Default)]
pub struct ColorManagementState {
    /// Live `wp_color_management_output_v1` objects, so an output flipping between SDR and HDR
    /// can tell the clients watching it.
    outputs: Vec<(Output, WpColorManagementOutputV1)>,
    /// Live surface-feedback objects, for the same reason.
    feedback: Vec<(WlSurface, WpColorManagementSurfaceFeedbackV1)>,
    /// Information bursts waiting to be sent, and why they wait: see [`Wlrix::flush_image_description_info`].
    pending_info: Vec<(WpImageDescriptionInfoV1, Description)>,
    /// Every distinct description handed out, in identity order.
    ///
    /// The protocol requires identical descriptions to share an identity and different ones not
    /// to, so identity is assigned by looking the contents up rather than by counting: a client
    /// comparing two outputs' descriptions has to be able to tell "these are the same" from the
    /// number alone.
    identities: Vec<Description>,
}

impl ColorManagementState {
    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, WpColorManagerV1, _>(VERSION, ())
    }

    /// The identity for a description, minting one if it has not been seen.
    ///
    /// Identity 0 is not valid in the protocol, so these start at 1.
    fn identity(&mut self, description: &Description) -> u32 {
        if let Some(index) = self.identities.iter().position(|kept| kept == description) {
            return index as u32 + 1;
        }
        self.identities.push(*description);
        self.identities.len() as u32
    }
}

impl Wlrix {
    /// Send any image-description information a client asked for during the last dispatch.
    ///
    /// Called once per event-loop iteration. See the comment at the push site for why this
    /// cannot happen inline.
    pub fn flush_image_description_info(&mut self) {
        for (info, description) in std::mem::take(&mut self.color_management.pending_info) {
            description.describe(&info);
        }
    }

    /// An output changed color: tell everyone watching it, and everyone on it.
    ///
    /// Called when HDR is switched on or off. The protocol only says "it changed" -- the client
    /// fetches the new description itself -- so there is nothing to compute here.
    pub fn color_description_changed(&mut self, output: &Output) {
        for (kept, resource) in &self.color_management.outputs {
            if kept == output {
                resource.image_description_changed();
            }
        }
        // A surface's preferred description follows the output it is on. Which output that is
        // changes as windows move, so rather than track it, every feedback object is told --
        // the client's response is to ask again, and asking again is cheap.
        for (_, resource) in &self.color_management.feedback {
            resource.preferred_changed(0);
        }
    }

    /// Hand a client a `wp_image_description_v1` for `description`, already ready.
    fn send_description(
        &mut self,
        resource: WpImageDescriptionV1,
        description: Option<Description>,
    ) {
        match description {
            Some(description) => {
                let identity = self.color_management.identity(&description);
                resource.ready(identity);
            }
            // `no_output` is the protocol's cause for "the thing this described is gone".
            None => resource.failed(
                wp_image_description_v1::Cause::NoOutput,
                "the output is gone".into(),
            ),
        }
    }
}

/// What a client has said about one of its surfaces.
///
/// Recorded and not yet acted on -- see the module docs. Kept in the surface's own data map so
/// it dies with the surface rather than needing to be reaped.
#[derive(Default)]
pub struct SurfaceColorState {
    pub description: Option<Description>,
    pub render_intent: Option<RenderIntent>,
    /// Whether a `wp_color_management_surface_v1` already exists for this surface. The protocol
    /// allows only one, and a second is a `surface_exists` error rather than a replacement.
    has_manager: bool,
}

/// Read a surface's recorded color state.
///
/// Unused for now by design: this milestone records what clients say and acts on none of it
/// (see the module docs). Kept because it is the seam the next one renders through, and because
/// it makes the recording observable rather than write-only.
#[allow(dead_code)]
pub fn surface_description(surface: &WlSurface) -> Option<Description> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<Mutex<SurfaceColorState>>()
            .and_then(|state| state.lock().unwrap().description)
    })
}

fn update_surface_state(surface: &WlSurface, update: impl FnOnce(&mut SurfaceColorState)) {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| Mutex::new(SurfaceColorState::default()));
        let mut state = states
            .data_map
            .get::<Mutex<SurfaceColorState>>()
            .expect("just inserted")
            .lock()
            .unwrap();
        update(&mut state);
    });
}

// -- wp_color_manager_v1 ---------------------------------------------------------------------

impl GlobalDispatch<WpColorManagerV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());

        // What is advertised here is a promise: anything not in this list must be refused with
        // `unsupported_feature` when a client tries to use it, so the list is deliberately
        // short and matches what the code below actually does.
        manager.supported_intent(RenderIntent::Perceptual);
        manager.supported_feature(Feature::Parametric);
        manager.supported_feature(Feature::SetPrimaries);
        manager.supported_feature(Feature::SetLuminances);
        // The two transfer functions wlRIX can name: what SDR is, and what its HDR mode is.
        manager.supported_tf_named(TransferFunction::Srgb);
        manager.supported_tf_named(TransferFunction::St2084Pq);
        manager.supported_primaries_named(Primaries::Srgb);
        manager.supported_primaries_named(Primaries::Bt2020);
        manager.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_manager_v1::Request::GetOutput { id, output } => {
                let managed = data_init.init(id, Output::from_resource(&output));
                if let Some(output) = Output::from_resource(&output) {
                    state.color_management.outputs.push((output, managed));
                }
            }
            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                // One color-management surface per wl_surface, as the protocol requires.
                if surface_has_manager(&surface) {
                    resource.post_error(
                        wp_color_manager_v1::Error::SurfaceExists,
                        "this surface already has a wp_color_management_surface_v1",
                    );
                    return;
                }
                update_surface_state(&surface, |state| state.has_manager = true);
                data_init.init(id, surface);
            }
            wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
                let managed = data_init.init(id, surface.clone());
                state.color_management.feedback.push((surface, managed));
            }
            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                data_init.init(obj, Mutex::new(Params::default()));
            }
            // ICC profiles are not supported, and `icc_v2_v4` is not advertised, so asking is
            // a protocol error rather than a soft failure.
            wp_color_manager_v1::Request::CreateIccCreator { obj } => {
                resource.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "ICC profiles are not supported",
                );
                // The object still has to be initialized, or the id leaks in the client's map.
                data_init.init(obj, ());
            }
            wp_color_manager_v1::Request::CreateWindowsScrgb { image_description } => {
                resource.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "the Windows scRGB image description is not supported",
                );
                let description = data_init.init(image_description, None);
                let _ = description;
            }
            _ => {}
        }
    }
}

/// Whether this surface already has a `wp_color_management_surface_v1`.
fn surface_has_manager(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<Mutex<SurfaceColorState>>()
            .is_some_and(|state| state.lock().unwrap().has_manager)
    })
}

// -- wp_color_management_output_v1 -----------------------------------------------------------

impl Dispatch<WpColorManagementOutputV1, Option<Output>> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        output: &Option<Output>,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let wp_color_management_output_v1::Request::GetImageDescription { image_description } =
            request
        else {
            return;
        };
        // Snapshotted now rather than referenced: an image description is immutable once
        // created, and the output can be switched into HDR a moment later.
        let description = output
            .as_ref()
            .map(|output| Description::for_output(state, output));
        let resource = data_init.init(image_description, description);
        state.send_description(resource, description);
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &WpColorManagementOutputV1,
        _data: &Option<Output>,
    ) {
        state
            .color_management
            .outputs
            .retain(|(_, kept)| kept != resource);
    }
}

// -- wp_color_management_surface_v1 ----------------------------------------------------------

impl Dispatch<WpColorManagementSurfaceV1, WlSurface> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        surface: &WlSurface,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_color_management_surface_v1::Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                // Only the perceptual intent is advertised, so anything else is refused rather
                // than silently treated as perceptual.
                let Ok(render_intent) = render_intent.into_result() else {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::RenderIntent,
                        "unknown render intent",
                    );
                    return;
                };
                if render_intent != RenderIntent::Perceptual {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::RenderIntent,
                        "only the perceptual render intent is supported",
                    );
                    return;
                }
                let description = image_description
                    .data::<Option<Description>>()
                    .copied()
                    .flatten();
                let Some(description) = description else {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::ImageDescription,
                        "that image description never became ready",
                    );
                    return;
                };
                update_surface_state(surface, |state| {
                    state.description = Some(description);
                    state.render_intent = Some(render_intent);
                });
            }
            wp_color_management_surface_v1::Request::UnsetImageDescription => {
                update_surface_state(surface, |state| {
                    state.description = None;
                    state.render_intent = None;
                });
            }
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &WpColorManagementSurfaceV1,
        surface: &WlSurface,
    ) {
        // The surface goes back to being untagged, and another manager may be created for it.
        if surface.is_alive() {
            update_surface_state(surface, |state| {
                state.description = None;
                state.render_intent = None;
                state.has_manager = false;
            });
        }
    }
}

// -- wp_color_management_surface_feedback_v1 -------------------------------------------------

impl Dispatch<WpColorManagementSurfaceFeedbackV1, WlSurface> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        surface: &WlSurface,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // `get_preferred` and `get_preferred_parametric` differ only in whether an ICC result is
        // acceptable. Everything wlRIX describes is parametric, so both answer the same.
        let image_description = match request {
            wp_color_management_surface_feedback_v1::Request::GetPreferred {
                image_description,
            } => image_description,
            wp_color_management_surface_feedback_v1::Request::GetPreferredParametric {
                image_description,
            } => image_description,
            _ => return,
        };

        // The description of the output this surface is on, which is what the client should
        // encode for. A surface on no output at all gets sRGB rather than a failure -- it has
        // to render something, and guessing SDR is the safe guess.
        let description = state
            .output_for_surface(surface)
            .map(|output| Description::for_output(state, &output))
            .unwrap_or_else(Description::srgb);
        let resource = data_init.init(image_description, Some(description));
        state.send_description(resource, Some(description));
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &WpColorManagementSurfaceFeedbackV1,
        _surface: &WlSurface,
    ) {
        state
            .color_management
            .feedback
            .retain(|(_, kept)| kept != resource);
    }
}

// -- wp_image_description_v1 -----------------------------------------------------------------

impl Dispatch<WpImageDescriptionV1, Option<Description>> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        description: &Option<Description>,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let wp_image_description_v1::Request::GetInformation { information } = request else {
            return;
        };
        let Some(description) = description else {
            resource.post_error(
                wp_image_description_v1::Error::NotReady,
                "this image description never became ready",
            );
            return;
        };
        // Deferred rather than sent here, and this is not a style choice.
        //
        // `wp_image_description_info_v1.done` is a *destructor* event, and this object was
        // created by this very dispatch. wayland-backend's C path creates the child resource
        // before invoking this callback and writes its object data in afterwards
        // (`resource_dispatcher`, the `match (created, ret)` at the end): destroying the child
        // in between leaves that write pointing at freed memory. It segfaults the compositor,
        // some way from here, when the next client message arrives.
        //
        // So the burst goes out once the dispatch has returned.
        let info = data_init.init(information, ());
        state
            .color_management
            .pending_info
            .push((info, *description));
    }
}

/// ICC profiles are refused at `create_icc_creator`, but the object still has to exist for the
/// client's id to be valid. It never does anything.
impl Dispatch<WpImageDescriptionCreatorIccV1, ()> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionCreatorIccV1,
        _request: <WpImageDescriptionCreatorIccV1 as Resource>::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // The interface has no requests: it emits its burst of events and destroys itself.
    }
}

// -- wp_image_description_creator_params_v1 --------------------------------------------------

/// A description being built up request by request, before `create` turns it into an object.
#[derive(Default)]
struct Params {
    primaries: Option<Primaries>,
    tf: Option<TransferFunction>,
    luminances: Option<(u32, u32, u32)>,
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, Mutex<Params>> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &Mutex<Params>,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};

        let mut params = data.lock().unwrap();
        match request {
            Request::SetPrimariesNamed { primaries } => {
                let Ok(primaries) = primaries.into_result() else {
                    resource.post_error(Error::InvalidPrimariesNamed, "unknown named primaries");
                    return;
                };
                if params.primaries.is_some() {
                    resource.post_error(Error::AlreadySet, "primaries have already been set");
                    return;
                }
                params.primaries = Some(primaries);
            }
            Request::SetTfNamed { tf } => {
                let Ok(tf) = tf.into_result() else {
                    resource.post_error(Error::InvalidTf, "unknown named transfer function");
                    return;
                };
                // Only the two advertised by `supported_tf_named` are accepted. A client that
                // ignored that list finds out here rather than by getting a wrong picture.
                if !matches!(tf, TransferFunction::Srgb | TransferFunction::St2084Pq) {
                    resource
                        .post_error(Error::InvalidTf, "that transfer function is not supported");
                    return;
                }
                if params.tf.is_some() {
                    resource.post_error(Error::AlreadySet, "the transfer function is already set");
                    return;
                }
                params.tf = Some(tf);
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                if params.luminances.is_some() {
                    resource.post_error(Error::AlreadySet, "luminances have already been set");
                    return;
                }
                // The protocol's own consistency rule, and worth enforcing: a reference white
                // above the maximum describes a display that cannot show its own white.
                if reference_lum > max_lum {
                    resource.post_error(
                        Error::InvalidLuminance,
                        "reference white is above the maximum luminance",
                    );
                    return;
                }
                params.luminances = Some((min_lum, max_lum, reference_lum));
            }
            Request::Create { image_description } => {
                // Both are mandatory: a description with no transfer function or no primaries
                // does not describe anything.
                let (Some(primaries), Some(tf)) = (params.primaries, params.tf) else {
                    resource.post_error(
                        Error::IncompleteSet,
                        "an image description needs both primaries and a transfer function",
                    );
                    return;
                };

                // Defaults follow the transfer function when the client did not say.
                let (min_luminance, max_luminance, reference_luminance) =
                    params.luminances.unwrap_or(match tf {
                        TransferFunction::St2084Pq => (0, 10_000, 203),
                        _ => (2_000, 80, 80),
                    });
                let description = Description {
                    primaries,
                    tf,
                    min_luminance,
                    max_luminance,
                    reference_luminance,
                    mastering: None,
                };
                // Released before the description is registered, which needs the compositor
                // state and would otherwise be holding this lock across it.
                drop(params);

                let resource = data_init.init(image_description, Some(description));
                // Ready immediately: there is nothing to validate asynchronously, since every
                // parameter was checked as it arrived.
                let identity = state.color_management.identity(&description);
                info!(
                    ?description,
                    identity, "client created an image description"
                );
                resource.ready(identity);
            }
            // `set_tf_power` and the mastering-display requests are not advertised as features,
            // so a client asking for them is told plainly rather than quietly ignored.
            _ => {
                resource.post_error(
                    Error::UnsupportedFeature,
                    "that image description parameter is not supported",
                );
            }
        }
    }
}

impl Wlrix {
    /// The output a surface is being shown on, as best as can be told.
    ///
    /// A hint, and treated as one: it drives the *preferred* description a client is offered,
    /// and being wrong costs a client encoding for the neighboring monitor for a frame, not a
    /// broken picture. Layer surfaces and subsurfaces fall through to the first output, which
    /// is where the shell components live anyway.
    fn output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        self.space
            .elements()
            .find(|window| window.wl_surface().as_deref() == Some(surface))
            .and_then(|window| self.space.outputs_for_element(window).into_iter().next())
            .or_else(|| self.space.outputs().next().cloned())
    }
}
