// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying `[mouse]` and `[touchpad]` to the pointing devices libinput hands us.
//!
//! libinput keeps acceleration and handedness per device and forgets them when a device goes
//! away, so each device is configured as it is added -- a mouse plugged in after login gets the
//! same settings as the one that was there -- and every known device again when a reload changes
//! either section. Only the udev backend has libinput devices; under the nested backend the host
//! compositor owns all of it and this has nothing to do.
//!
//! # Which devices are touchpads
//!
//! The ones that can be tapped: libinput gives a tap finger count only to touchpads, which is
//! the test sway and Mutter use too. A touchpad takes its speed from `[touchpad]` and its
//! handedness from `[mouse]`; everything else that points takes both from `[mouse]`.

use smithay::reexports::input::{
    ClickMethod, Device, DeviceCapability, DragLockState, ScrollMethod, SendEventsMode,
};
use tracing::{debug, warn};

use crate::config::{MouseConfig, SecondaryClick, TouchpadConfig, TouchpadScroll};

/// Whether libinput treats this device as a touchpad.
pub fn is_touchpad(device: &Device) -> bool {
    device.config_tap_finger_count() > 0
}

/// Bring one device in line with `mouse` and, for a touchpad, `touchpad`.
///
/// A setting the device does not support is skipped rather than reported: a keyboard has no
/// acceleration, and a trackpoint may have no left-handed mode. An absent setting puts the
/// device back to its own default, so removing a key from the file is undone by a reload rather
/// than left at whatever was last written.
pub fn configure(device: &mut Device, mouse: &MouseConfig, touchpad: &TouchpadConfig) {
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let touch = is_touchpad(device);

    if device.config_accel_is_available() {
        let wanted = if touch {
            touchpad.acceleration
        } else {
            mouse.acceleration
        };
        let speed = wanted.map_or_else(|| device.config_accel_default_speed(), clamp_speed);
        if let Err(err) = device.config_accel_set_speed(speed) {
            warn!(device = %device.name(), speed, ?err, "could not set the pointer acceleration");
        }
    }

    if device.config_left_handed_is_available() {
        let left = mouse
            .left_handed
            .unwrap_or_else(|| device.config_left_handed_default());
        if let Err(err) = device.config_left_handed_set(left) {
            warn!(device = %device.name(), left, ?err, "could not set left-handed mode");
        }
    }

    if touch {
        configure_touchpad(device, touchpad);
    }

    debug!(device = %device.name(), touch, ?mouse, ?touchpad, "pointer configured");
}

fn configure_touchpad(device: &mut Device, config: &TouchpadConfig) {
    let name = String::from(device.name());
    let report = |what: &str, result: smithay::reexports::input::DeviceConfigResult| {
        if let Err(err) = result {
            warn!(device = %name, ?err, "could not set {what}");
        }
    };

    // Tapping, and dragging and drag lock with it, only where there is tapping at all.
    report("tap-to-click", device.config_tap_set_enabled(config.tap()));
    let drag = config
        .tap_drag
        .unwrap_or_else(|| device.config_tap_default_drag_enabled());
    report("tap-and-drag", device.config_tap_set_drag_enabled(drag));
    let lock = match config.drag_lock {
        Some(true) => DragLockState::EnabledTimeout,
        Some(false) => DragLockState::Disabled,
        None => device.config_tap_default_drag_lock_enabled(),
    };
    report("drag lock", device.config_tap_set_drag_lock_enabled(lock));

    if device.config_scroll_has_natural_scroll() {
        let natural = config
            .natural_scroll
            .unwrap_or_else(|| device.config_scroll_default_natural_scroll_enabled());
        report(
            "natural scrolling",
            device.config_scroll_set_natural_scroll_enabled(natural),
        );
    }

    if device.config_dwt_is_available() {
        let dwt = config
            .disable_while_typing
            .unwrap_or_else(|| device.config_dwt_default_enabled());
        report("disable-while-typing", device.config_dwt_set_enabled(dwt));
    }

    let scroll = match config.scroll {
        Some(TouchpadScroll::TwoFinger) => Some(ScrollMethod::TwoFinger),
        Some(TouchpadScroll::Edge) => Some(ScrollMethod::Edge),
        None => device.config_scroll_default_method(),
    };
    // A single-touch pad cannot scroll with two fingers; it keeps what it has rather than being
    // told to do something it cannot.
    if let Some(scroll) = scroll.filter(|method| device.config_scroll_methods().contains(method)) {
        report("the scroll method", device.config_scroll_set_method(scroll));
    }

    let click = match config.secondary_click {
        Some(SecondaryClick::Fingers) => Some(ClickMethod::Clickfinger),
        Some(SecondaryClick::Corner) => Some(ClickMethod::ButtonAreas),
        None => device.config_click_default_method(),
    };
    // Only clickpads -- pads with no separate buttons -- have a click method.
    if let Some(click) = click.filter(|method| device.config_click_methods().contains(method)) {
        report("the click method", device.config_click_set_method(click));
    }

    let modes = device.config_send_events_modes();
    let mode = if config.disable_with_mouse == Some(true)
        && modes.contains(SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE)
    {
        SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE
    } else {
        SendEventsMode::ENABLED
    };
    report(
        "the send-events mode",
        device.config_send_events_set_mode(mode),
    );
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
