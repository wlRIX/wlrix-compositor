// SPDX-License-Identifier: GPL-3.0-or-later
//! `zwlr_gamma_control_manager_v1`: night-light tools setting a per-output color ramp.
//!
//! `gammastep` and `wlsunset` warm the screen towards evening by handing the compositor a
//! gamma table -- three arrays of 16-bit values, one per channel -- which the CRTC applies to
//! everything it scans out.
//!
//! Hand-written twice over: Smithay has no implementation of the protocol, and no gamma support
//! in its DRM layer either, so the ramp goes to the kernel through the raw `drm` crate's
//! `set_gamma`. **udev only** -- nested there is no CRTC, so a client is told the ramp size is
//! zero, which the protocol defines as "unsupported".

use std::io::Read;
use std::os::fd::OwnedFd;

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
            backend::{ClientId, GlobalId},
        },
    },
};
use tracing::{info, warn};

use crate::Wlrix;

const VERSION: u32 = 1;

#[derive(Default)]
pub struct GammaState {
    /// The controls clients hold, at most one per output (a second is told it failed).
    controls: Vec<Control>,
}

struct Control {
    output: Output,
    resource: ZwlrGammaControlV1,
}

impl GammaState {
    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ZwlrGammaControlManagerV1, _>(VERSION, ())
    }
}

impl Wlrix {
    /// Read `3 * size` 16-bit values from `fd` and hand them to the CRTC.
    ///
    /// The client writes red, then green, then blue. A short or over-long table is a protocol
    /// error on its part, which the caller turns into `failed`.
    fn apply_gamma_from(&mut self, output: &Output, fd: OwnedFd, size: usize) -> Result<(), ()> {
        let mut raw = Vec::new();
        let mut file = std::fs::File::from(fd);
        file.read_to_end(&mut raw).map_err(|err| {
            warn!(?err, "could not read the gamma table");
        })?;

        let expected = size * 3 * std::mem::size_of::<u16>();
        if raw.len() != expected {
            warn!(
                got = raw.len(),
                expected, "gamma table is the wrong size for this output"
            );
            return Err(());
        }

        let values: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        let (red, rest) = values.split_at(size);
        let (green, blue) = rest.split_at(size);
        crate::backend::udev::set_gamma(self, output, Some((red, green, blue)))
    }

    /// Put an output back to a linear ramp, when its control goes away.
    fn reset_gamma(&mut self, output: &Output) {
        let _ = crate::backend::udev::set_gamma(self, output, None);
    }
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };
        let Some(output) = Output::from_resource(&output) else {
            let control = data_init.init(id, None);
            control.failed();
            return;
        };

        // One controller per output: a second is told it lost, as the protocol says.
        if state
            .gamma
            .controls
            .iter()
            .any(|control| control.output == output)
        {
            let control = data_init.init(id, None);
            control.failed();
            return;
        }

        // On an HDR output the CRTC's gamma table sits *after* the PQ encode, so it would be
        // applying an sRGB-shaped warming curve to PQ code values -- which does not warm the
        // picture, it wrecks it. Refusing is the honest answer: `gammastep` then leaves this
        // screen alone instead of tinting it wrongly, and still warms the SDR ones.
        if state.hdr.active(&output) {
            info!(
                output = %output.name(),
                "refusing gamma control: this output is in HDR"
            );
            let control = data_init.init(id, None);
            control.failed();
            return;
        }

        // A ramp size of zero means the output cannot do gamma -- which is the honest answer
        // under the nested backend, and for a CRTC without a gamma table.
        let size = crate::backend::udev::gamma_size(state, &output);
        let control = data_init.init(id, Some(output.clone()));
        if size == 0 {
            control.failed();
            return;
        }
        control.gamma_size(size as u32);
        state.gamma.controls.push(Control {
            output,
            resource: control,
        });
    }
}

impl Dispatch<ZwlrGammaControlV1, Option<Output>> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        output: &Option<Output>,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };
        let Some(output) = output.clone() else {
            return;
        };
        // Also checked here, not just at bind: an output can be switched into HDR while a
        // client already holds a control, and that client would keep pushing ramps.
        let size = crate::backend::udev::gamma_size(state, &output);
        if state.hdr.active(&output)
            || size == 0
            || state.apply_gamma_from(&output, fd, size).is_err()
        {
            resource.failed();
            state
                .gamma
                .controls
                .retain(|control| &control.resource != resource);
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrGammaControlV1,
        output: &Option<Output>,
    ) {
        state
            .gamma
            .controls
            .retain(|control| &control.resource != resource);
        // The client is gone, so the screen must not stay tinted.
        if let Some(output) = output {
            state.reset_gamma(output);
        }
    }
}
