// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying `[mouse]` to the pointing devices libinput hands us.
//!
//! libinput keeps acceleration and handedness per device and forgets them when a device goes
//! away, so each device is configured as it is added -- a mouse plugged in after login gets the
//! same settings as the one that was there -- and every known device again when a reload changes
//! the section. Only the udev backend has libinput devices; under the nested backend the host
//! compositor owns both settings and this has nothing to do.

use smithay::reexports::input::{Device, DeviceCapability};
use tracing::{debug, warn};

use crate::config::MouseConfig;

/// Bring one device in line with `config`.
///
/// A setting the device does not support is skipped rather than reported: a keyboard has no
/// acceleration, and a trackpoint may have no left-handed mode. An absent setting puts the
/// device back to its own default, so removing a key from the file is undone by a reload rather
/// than left at whatever was last written.
pub fn configure(device: &mut Device, config: &MouseConfig) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }

    if device.config_accel_is_available() {
        let speed = config
            .acceleration
            .map_or_else(|| device.config_accel_default_speed(), clamp_speed);
        if let Err(err) = device.config_accel_set_speed(speed) {
            warn!(device = %device.name(), speed, ?err, "could not set the pointer acceleration");
        }
    }

    if device.config_left_handed_is_available() {
        let left = config
            .left_handed
            .unwrap_or_else(|| device.config_left_handed_default());
        if let Err(err) = device.config_left_handed_set(left) {
            warn!(device = %device.name(), left, ?err, "could not set left-handed mode");
        }
    }

    debug!(device = %device.name(), ?config, "pointer configured");
}

/// libinput's range, which it refuses rather than clamps. NaN -- which TOML can spell -- is the
/// device's default rather than either end.
fn clamp_speed(speed: f64) -> f64 {
    if speed.is_nan() {
        0.0
    } else {
        speed.clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_is_held_to_libinputs_range() {
        assert_eq!(clamp_speed(0.5), 0.5);
        assert_eq!(clamp_speed(1.5), 1.0);
        assert_eq!(clamp_speed(-3.0), -1.0);
        assert_eq!(clamp_speed(f64::NAN), 0.0);
    }

    #[test]
    fn a_double_click_of_nothing_is_the_default() {
        let unset = MouseConfig::default();
        let zero = MouseConfig {
            double_click_ms: Some(0),
            ..Default::default()
        };
        let set = MouseConfig {
            double_click_ms: Some(250),
            ..Default::default()
        };
        assert_eq!(unset.double_click().as_millis(), 400);
        assert_eq!(zero.double_click().as_millis(), 400);
        assert_eq!(set.double_click().as_millis(), 250);
    }
}
