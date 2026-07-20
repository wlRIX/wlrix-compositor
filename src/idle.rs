// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-idle-notify-v1`: telling clients when the user has stopped touching things.
//!
//! This is what an idle daemon watches to dim the screen, start a lock, or suspend.
//! A client asks for a notification after some number of milliseconds without input;
//! it gets `idled` when that passes and `resumed` on the next input.
//!
//! Smithay has an implementation. This predates the event loop carrying `Wlrix` as its
//! data: `IdleNotifierState<D>` keeps a `LoopHandle<'static, D>` and needs that same `D`
//! to be the protocol dispatch state, which was two different types here at the time.
//! It no longer is, so this could now be replaced by Smithay's -- left alone for the
//! moment because it works and is covered by `examples/test_idle.rs`.
//!
//! Idle inhibitors are Smithay's (`zwp_idle_inhibit_manager_v1`, which needs no loop
//! handle) and feed in through [`set_inhibited`].

use std::time::Duration;

use smithay::reexports::{
    calloop::{
        RegistrationToken,
        timer::{TimeoutAction, Timer},
    },
    wayland_protocols::ext::idle_notify::v1::server::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
    },
    wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
        backend::{ClientId, GlobalId},
    },
};

use crate::Wlrix;

/// Version 2 adds `get_input_idle_notification`, which ignores inhibitors.
const VERSION: u32 = 2;

/// Resource data for a notification.
///
/// Empty: what a notification needs is kept in [`IdleState`] alongside its timer, and
/// the seat it was created for is ignored because wlRIX has exactly one. That has to
/// change the day it has more, since notifications are per-seat.
pub struct NotificationData;

/// One outstanding notification and its timer.
struct Notification {
    resource: ExtIdleNotificationV1,
    timeout: Duration,
    ignore_inhibitor: bool,
    /// Whether `idled` has been sent and not yet followed by `resumed`.
    idle: bool,
    timer: Option<RegistrationToken>,
}

#[derive(Default)]
pub struct IdleState {
    notifications: Vec<Notification>,
    /// Surfaces asking to keep the session awake, by `zwp_idle_inhibitor_v1`.
    inhibitors: usize,
}

impl IdleState {
    fn inhibited(&self) -> bool {
        self.inhibitors > 0
    }
}

pub struct IdleNotifierState;

impl IdleNotifierState {
    pub fn create_global(display: &DisplayHandle) -> GlobalId {
        display.create_global::<Wlrix, ExtIdleNotifierV1, _>(VERSION, ())
    }
}

/// Start (or restart) the countdown for one notification.
fn arm(state: &mut Wlrix, index: usize) {
    let Some(notification) = state.idle.notifications.get(index) else {
        return;
    };
    // An inhibited notification simply does not run; it resumes counting when the
    // inhibitor goes away.
    if state.idle.inhibited() && !notification.ignore_inhibitor {
        return;
    }
    let resource = notification.resource.clone();
    let timeout = notification.timeout;

    let token = state
        .loop_handle
        .insert_source(Timer::from_duration(timeout), move |_, _, data| {
            mark_idle(data, &resource);
            TimeoutAction::Drop
        })
        .ok();
    if let Some(notification) = state.idle.notifications.get_mut(index) {
        notification.timer = token;
    }
}

/// Cancel a notification's countdown, leaving its idle state alone.
fn disarm(state: &mut Wlrix, index: usize) {
    let Some(notification) = state.idle.notifications.get_mut(index) else {
        return;
    };
    if let Some(token) = notification.timer.take() {
        state.loop_handle.remove(token);
    }
}

/// A countdown ran out.
fn mark_idle(state: &mut Wlrix, resource: &ExtIdleNotificationV1) {
    let Some(notification) = state
        .idle
        .notifications
        .iter_mut()
        .find(|notification| &notification.resource == resource)
    else {
        return;
    };
    notification.timer = None;
    if !notification.idle {
        notification.idle = true;
        notification.resource.idled();
    }
}

/// The user did something. Everything idle resumes, and every countdown restarts.
///
/// Called for every input event, so it must stay cheap: with no notifications
/// outstanding -- the usual case -- it does nothing at all.
pub fn notify_activity(state: &mut Wlrix) {
    if state.idle.notifications.is_empty() {
        return;
    }
    for index in 0..state.idle.notifications.len() {
        let was_idle = state.idle.notifications[index].idle;
        if was_idle {
            state.idle.notifications[index].idle = false;
            state.idle.notifications[index].resource.resumed();
        }
        disarm(state, index);
        arm(state, index);
    }
}

/// An inhibitor appeared or went away.
pub fn set_inhibited(state: &mut Wlrix, inhibited: bool) {
    if inhibited {
        state.idle.inhibitors += 1;
    } else {
        state.idle.inhibitors = state.idle.inhibitors.saturating_sub(1);
    }

    // Inhibited notifications stop counting; released ones start again from the full
    // timeout, since being inhibited is itself a sign the session is in use.
    for index in 0..state.idle.notifications.len() {
        if state.idle.notifications[index].ignore_inhibitor {
            continue;
        }
        if state.idle.inhibited() {
            disarm(state, index);
        } else {
            if state.idle.notifications[index].idle {
                state.idle.notifications[index].idle = false;
                state.idle.notifications[index].resource.resumed();
            }
            disarm(state, index);
            arm(state, index);
        }
    }
}

impl GlobalDispatch<ExtIdleNotifierV1, ()> for Wlrix {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        resource: New<ExtIdleNotifierV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Wlrix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // The two differ only in whether an inhibitor suppresses them.
        let (id, timeout, seat, ignore_inhibitor) = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification { id, timeout, seat } => {
                (id, timeout, seat, false)
            }
            ext_idle_notifier_v1::Request::GetInputIdleNotification { id, timeout, seat } => {
                (id, timeout, seat, true)
            }
            _ => return,
        };

        let timeout = Duration::from_millis(timeout as u64);
        let _ = seat;
        let resource = data_init.init(id, NotificationData);

        state.idle.notifications.push(Notification {
            resource,
            timeout,
            ignore_inhibitor,
            idle: false,
            timer: None,
        });
        // Counting starts now, not at the next input: a client that asks while the
        // user is already away should still be told.
        let index = state.idle.notifications.len() - 1;
        arm(state, index);
    }
}

impl Dispatch<ExtIdleNotificationV1, NotificationData> for Wlrix {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotificationV1,
        _request: ext_idle_notification_v1::Request,
        _data: &NotificationData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy`, which is handled in `destroyed`.
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtIdleNotificationV1,
        _data: &NotificationData,
    ) {
        // The timer outlives the resource otherwise, and would fire against a dead
        // object.
        if let Some(index) = state
            .idle
            .notifications
            .iter()
            .position(|notification| &notification.resource == resource)
        {
            disarm(state, index);
            state.idle.notifications.remove(index);
        }
    }
}
