// SPDX-License-Identifier: GPL-3.0-or-later
//! Switching monitors off and on: `zwlr_output_power_management_v1`, and the compositor's own
//! "blank the screen after a while" timeout.
//!
//! Two ways in, one mechanism. A client such as `wlopm` (or `swayidle`'s DPMS action) sets a
//! mode over the protocol; and, if `[idle] blank_after_secs` is configured, the compositor
//! blanks the screen itself once the session has been idle that long. Either way it ends at
//! [`crate::backend::udev::set_output_power`], which is the only thing that can actually
//! switch a connector off -- under the nested backend there is no connector, so the state is
//! tracked and reported but nothing happens on screen.
//!
//! Hand-written dispatch, like [`crate::output_management`]: Smithay has no implementation.

use std::time::Duration;

use smithay::{
    output::Output,
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols_wlr::output_power_management::v1::server::{
            zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
            zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
            backend::{ClientId, GlobalId},
        },
    },
};
use tracing::info;

use crate::Wlrix;

const VERSION: u32 = 1;

#[derive(Default)]
pub struct PowerState {
    /// Outputs currently switched off. Absent means on, so a fresh output starts on.
    off: Vec<Output>,
    /// The protocol objects clients hold, at most one per output (the protocol says a second
    /// one for the same output must fail).
    controls: Vec<Control>,
    /// The idle countdown, when one is configured and running.
    blank_timer: Option<RegistrationToken>,
    /// Whether the current blank was the idle timeout's doing. A blank a *client* asked for is
    /// left alone by input, or moving the mouse would undo `wlopm --off`.
    blanked_by_idle: bool,
}

struct Control {
    output: Output,
    resource: ZwlrOutputPowerV1,
}

impl PowerState {
    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ZwlrOutputPowerManagerV1, _>(VERSION, ())
    }
}

impl Wlrix {
    /// Whether `output` is switched on.
    pub fn output_powered(&self, output: &Output) -> bool {
        !self.power.off.contains(output)
    }

    /// Switch one output off or on, telling any watching client.
    pub fn set_output_power(&mut self, output: &Output, on: bool) {
        if self.output_powered(output) == on {
            return;
        }
        if on {
            self.power.off.retain(|candidate| candidate != output);
        } else {
            self.power.off.push(output.clone());
        }

        crate::backend::udev::set_output_power(self, output, on);

        let mode = if on { Mode::On } else { Mode::Off };
        for control in &self.power.controls {
            if &control.output == output {
                control.resource.mode(mode);
            }
        }
    }

    /// Switch every output off or on at once, which is what the idle timeout wants.
    fn set_all_outputs_power(&mut self, on: bool) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in outputs {
            self.set_output_power(&output, on);
        }
    }

    /// How long the session may be idle before the screen blanks, if that is configured.
    fn blank_after(&self) -> Option<Duration> {
        self.config
            .idle
            .blank_after_secs
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
    }

    /// Restart the idle countdown, and undo an idle blank. Called on every input event, so it
    /// does nothing at all when no timeout is configured -- the usual case.
    pub fn notice_activity_for_blanking(&mut self) {
        if self.blank_after().is_none() {
            return;
        }
        if self.power.blanked_by_idle {
            self.power.blanked_by_idle = false;
            self.set_all_outputs_power(true);
        }
        self.arm_blank_timer();
    }

    /// Start (or restart) the countdown. An idle inhibitor holds it off, the same way it holds
    /// off client idle notifications.
    pub fn arm_blank_timer(&mut self) {
        self.disarm_blank_timer();
        let Some(timeout) = self.blank_after() else {
            return;
        };
        if self.idle.inhibited() {
            return;
        }
        self.power.blank_timer = self
            .loop_handle
            .insert_source(
                Timer::from_duration(timeout),
                move |_, _, state: &mut Wlrix| {
                    state.blank_for_idle();
                    TimeoutAction::Drop
                },
            )
            .ok();
    }

    fn disarm_blank_timer(&mut self) {
        if let Some(token) = self.power.blank_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// The countdown ran out.
    fn blank_for_idle(&mut self) {
        self.power.blank_timer = None;
        if self.idle.inhibited() {
            return;
        }
        info!("idle timeout reached; switching the displays off");
        self.power.blanked_by_idle = true;
        self.set_all_outputs_power(false);
    }
}

//
// Manager
//

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request else {
            return;
        };
        let Some(output) = Output::from_resource(&output) else {
            // The output is already gone; hand back an object that immediately fails, as the
            // protocol requires, rather than dropping the request on the floor.
            let control = data_init.init(id, None);
            control.failed();
            return;
        };

        // One controller per output: a second one is told it lost.
        if state
            .power
            .controls
            .iter()
            .any(|control| control.output == output)
        {
            let control = data_init.init(id, None);
            control.failed();
            return;
        }

        let control = data_init.init(id, Some(output.clone()));
        control.mode(if state.output_powered(&output) {
            Mode::On
        } else {
            Mode::Off
        });
        state.power.controls.push(Control {
            output,
            resource: control,
        });
    }
}

//
// Per-output power control
//

impl Dispatch<ZwlrOutputPowerV1, Option<Output>> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        output: &Option<Output>,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_output_power_v1::Request::SetMode { mode } = request else {
            return;
        };
        let Some(output) = output.clone() else {
            return;
        };
        let on = matches!(mode.into_result(), Ok(Mode::On));
        // A client switching the screen on or off outranks the idle countdown: clear the flag
        // so the next keypress does not immediately undo what it asked for.
        state.power.blanked_by_idle = false;
        state.set_output_power(&output, on);
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
        _data: &Option<Output>,
    ) {
        state
            .power
            .controls
            .retain(|control| &control.resource != resource);
    }
}
