// SPDX-License-Identifier: GPL-3.0-or-later
//! IRIX's iconbox: "Display Windows Overview" in Window Settings.
//!
//! With `[windows] iconbox` on, every window gets a tile in one window, `wlrix-iconbox` from
//! `wlrix-apps`, instead of minimizing to a tile on the desktop. The client draws the tiles and
//! acts on them through `wlrix-desks`, the same protocol the Desks overview uses; what is here is
//! the compositor's half:
//!
//! - **The desktop grid is emptied** while the setting is on -- see
//!   [`crate::minimized`] -- and filled again when it goes off.
//! - **The compositor starts the client**, because the compositor is what reads the setting.
//!   `wlrix-session`'s app list is read once at login, and 4Dwm's checkbox needed a window
//!   manager restart; here the setting applies on the `SIGHUP` that saved it.
//! - **The iconbox cannot be closed or minimized** ([`crate::placement::ShellFrame::Permanent`]).
//!   Turning the setting off is how it goes away, and that close is sent here, past the check
//!   that refuses the user's.
//!
//! A client that exits while the setting is on is started again, a few times: one that keeps
//! failing -- not installed, or crashing on start -- would otherwise be respawned on every
//! dispatch for the rest of the session.

use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use smithay::desktop::Window;
use tracing::{info, warn};

use crate::Wlrix;

/// The iconbox's `app_id`.
pub const APP_ID: &str = "com.wlrix.iconbox";

/// The program, off `PATH` like every other helper (see [`crate::spawn`]).
const PROGRAM: &str = "wlrix-iconbox";

/// How many quick exits in a row before the compositor gives up until the setting is next
/// turned on.
const MAX_RESTARTS: u32 = 3;

/// A client that ran at least this long before exiting was working; its exit does not count
/// toward [`MAX_RESTARTS`].
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// How long to wait before starting it again after an exit.
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// The compositor's record of the iconbox client.
#[derive(Default)]
pub struct Iconbox {
    /// The running client, and when it was started.
    child: Option<(Child, Instant)>,
    /// Quick exits since the setting was last turned on.
    failures: u32,
    /// No start before this, after an exit.
    not_before: Option<Instant>,
}

impl Wlrix {
    /// Start the iconbox if the setting is on and it is not running, and reap it if it exited.
    /// Called once per dispatch.
    pub fn refresh_iconbox(&mut self) {
        if let Some((child, started)) = &mut self.iconbox.child {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) => {
                    if started.elapsed() < STABLE_AFTER {
                        self.iconbox.failures += 1;
                    } else {
                        self.iconbox.failures = 0;
                    }
                    info!(
                        ?status,
                        failures = self.iconbox.failures,
                        "the iconbox exited"
                    );
                }
                Err(err) => warn!("could not check on the iconbox: {err}"),
            }
            self.iconbox.child = None;
            self.iconbox.not_before = Some(Instant::now() + RESTART_DELAY);
        }

        if !self.config.windows.iconbox || self.iconbox.failures >= MAX_RESTARTS {
            return;
        }
        if self
            .iconbox
            .not_before
            .is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        // One already on screen -- started by hand, or left from before a restart of this
        // loop -- is the iconbox; a second would only show the same tiles twice.
        if self.iconbox_window().is_some() {
            return;
        }

        self.iconbox.not_before = None;
        match Command::new(PROGRAM).stdin(Stdio::null()).spawn() {
            Ok(child) => {
                info!(program = PROGRAM, pid = child.id(), "spawned");
                self.iconbox.child = Some((child, Instant::now()));
            }
            Err(err) => {
                warn!(
                    program = PROGRAM,
                    "could not run it: {err} (is it installed?)"
                );
                // Not installed will not fix itself; stop trying until the setting is turned
                // on again.
                self.iconbox.failures = MAX_RESTARTS;
            }
        }
    }

    /// Apply a change to `[windows] iconbox` from a config reload. `was` is the old value.
    pub fn iconbox_setting_changed(&mut self, was: bool) {
        let now = self.config.windows.iconbox;
        if now == was {
            return;
        }
        info!(iconbox = now, "iconbox setting changed");
        if now {
            // A fresh go: the client may have been installed since it last failed.
            self.iconbox.failures = 0;
            self.iconbox.not_before = None;
            // A tile being dragged on the desktop is about to stop existing.
            self.icon_drag = None;
        } else {
            if let Some(window) = self.iconbox_window()
                && let Some(toplevel) = window.toplevel()
            {
                // Directly, not through `close_window`, which refuses to close the iconbox.
                toplevel.send_close();
            }
            self.assign_icon_slots();
        }
        // The desktop tiles appeared or went; the windows' reported geometry changed with them.
        self.desks_changed();
        self.request_redraw();
    }

    /// The iconbox's window, mapped or held aside, if it is open.
    fn iconbox_window(&self) -> Option<Window> {
        self.space
            .elements()
            .chain(self.desks.hidden().iter())
            .find(|window| crate::placement::app_id(window).as_deref() == Some(APP_ID))
            .cloned()
    }
}
