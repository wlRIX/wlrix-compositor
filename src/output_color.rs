// SPDX-License-Identifier: GPL-3.0-or-later
//! Server side of `wlrix-output-color` (`src/protocols/wlrix-output-color.xml`).
//!
//! `wlr-output-management` cannot say whether a head can do HDR, or turn it on, and reports
//! only whether adaptive sync is *on* -- so a display settings panel built on it alone could not
//! disable a checkbox for a feature the screen lacks. This fills those gaps without a second
//! configuration path: a head extension reads [`crate::hdr::HdrState`] and
//! [`crate::vrr::VrrState`], and a configuration head extension stages into the
//! `wlr-output-management` configuration it belongs to (see [`output_management::stage`]), so
//! HDR is validated and applied atomically with the mode, scale and position.
//!
//! Head extensions are a snapshot: `wlr-output-management` replaces its heads on every change,
//! and a client asks again for each new one. So nothing here is tracked or re-sent.

use smithay::{
    output::Output,
    reexports::wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        backend::GlobalId,
    },
};
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;

use crate::{
    Wlrix, output_management,
    protocols::wlrix_output_color::{
        wlrix_output_color_configuration_head_v1::{self, WlrixOutputColorConfigurationHeadV1},
        wlrix_output_color_head_v1::{self, WlrixOutputColorHeadV1},
        wlrix_output_color_manager_v1::{self, WlrixOutputColorManagerV1},
    },
};

/// Protocol version we implement.
const VERSION: u32 = 1;

/// The SDR white level a client may ask for, in cd/m². Below 80 SDR content is dimmer than an
/// SDR monitor at its dimmest; above 500 white text glares, and burns in on an OLED.
pub const SDR_WHITE_RANGE: std::ops::RangeInclusive<u32> = 80..=500;

pub fn create_global(display: &DisplayHandle) -> GlobalId {
    display.create_global::<Wlrix, WlrixOutputColorManagerV1, _>(VERSION, ())
}

impl GlobalDispatch<WlrixOutputColorManagerV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<WlrixOutputColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlrixOutputColorManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &WlrixOutputColorManagerV1,
        request: wlrix_output_color_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wlrix_output_color_manager_v1::Request::GetHead { id, head } => {
                let extension = data_init.init(id, ());
                // A head finished since the client saw it may name an output that has been
                // unplugged; that one supports nothing.
                let output = head
                    .data::<Output>()
                    .filter(|output| state.knows_output(output))
                    .cloned();
                describe(state, &extension, output.as_ref());
            }
            wlrix_output_color_manager_v1::Request::GetConfigurationHead { id, head } => {
                data_init.init(id, head);
            }
            wlrix_output_color_manager_v1::Request::Destroy => {}
        }
    }
}

/// Send everything a head extension carries, then `done`.
fn describe(state: &Wlrix, extension: &WlrixOutputColorHeadV1, output: Option<&Output>) {
    let hdr_supported = output.is_some_and(|output| state.hdr.supported(output));
    extension.hdr_supported(hdr_supported as u32);
    extension.hdr(output.is_some_and(|output| state.hdr.active(output)) as u32);
    let white = output.map_or(crate::hdr::DEFAULT_SDR_WHITE_NITS, |output| {
        state.hdr.sdr_white(output)
    });
    extension.sdr_white_level(white.round() as u32);
    if hdr_supported && let Some(mastering) = output.and_then(|output| state.hdr.mastering(output))
    {
        extension.luminance(
            (mastering.min_luminance * 10_000.0).round() as u32,
            mastering.max_luminance.round() as u32,
        );
    }
    extension
        .adaptive_sync_supported(output.is_some_and(|output| state.vrr.supported(output)) as u32);
    extension.done();
}

impl Dispatch<WlrixOutputColorHeadV1, ()> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlrixOutputColorHeadV1,
        _request: wlrix_output_color_head_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy`, which needs nothing.
    }
}

impl Dispatch<WlrixOutputColorConfigurationHeadV1, ZwlrOutputConfigurationHeadV1> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &WlrixOutputColorConfigurationHeadV1,
        request: wlrix_output_color_configuration_head_v1::Request,
        head: &ZwlrOutputConfigurationHeadV1,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Whether the head can do it is checked when the configuration is tested or
            // applied, with everything else, so the refusal is the configuration's `failed`.
            wlrix_output_color_configuration_head_v1::Request::SetHdr { enabled } => {
                output_management::stage(head, |config| config.hdr = Some(enabled != 0));
            }
            wlrix_output_color_configuration_head_v1::Request::SetSdrWhiteLevel { nits } => {
                if !SDR_WHITE_RANGE.contains(&nits) {
                    resource.post_error(
                        wlrix_output_color_configuration_head_v1::Error::InvalidSdrWhiteLevel,
                        format!(
                            "{nits} cd/m² is outside {}..={}",
                            SDR_WHITE_RANGE.start(),
                            SDR_WHITE_RANGE.end()
                        ),
                    );
                    return;
                }
                output_management::stage(head, |config| config.sdr_white = Some(nits as f32));
            }
            _ => {}
        }
    }
}
