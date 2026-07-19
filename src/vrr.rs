// SPDX-License-Identifier: GPL-3.0-or-later
//! Variable refresh rate (adaptive sync), per output.
//!
//! VRR lets the screen refresh when a frame is ready instead of on a fixed cadence,
//! which removes tearing and stutter for anything that does not hit the refresh rate
//! exactly. On DRM it is a crtc property; whether it can be set at all depends on the
//! monitor, the connector, and the driver.
//!
//! Only the hardware backend can answer any of that, so what is known is cached here
//! for the protocol code to report: `wlr-output-management` has to tell clients whether
//! a head has adaptive sync on, and refuse a request to enable it where it cannot work.
//! Under the nested backend nothing supports it, which is the correct answer there.

use std::collections::HashMap;

use smithay::output::Output;

/// What is known about each output's adaptive sync, keyed by output name.
#[derive(Default)]
pub struct VrrState {
    supported: HashMap<String, bool>,
    enabled: HashMap<String, bool>,
}

impl VrrState {
    /// Whether adaptive sync can be turned on for this output at all.
    ///
    /// False until the backend says otherwise, so a client cannot be told VRR is
    /// available on a screen that has never been asked.
    pub fn supported(&self, output: &Output) -> bool {
        self.supported.get(&output.name()).copied().unwrap_or(false)
    }

    /// Whether adaptive sync is currently on.
    pub fn enabled(&self, output: &Output) -> bool {
        self.enabled.get(&output.name()).copied().unwrap_or(false)
    }

    pub fn set_supported(&mut self, output: &Output, supported: bool) {
        self.supported.insert(output.name(), supported);
    }

    pub fn set_enabled(&mut self, output: &Output, enabled: bool) {
        self.enabled.insert(output.name(), enabled);
    }

    /// Drop an output that has gone away, so a reconnected one is not believed to
    /// support VRR on the strength of a stale entry.
    pub fn forget(&mut self, output: &Output) {
        self.supported.remove(&output.name());
        self.enabled.remove(&output.name());
    }
}
