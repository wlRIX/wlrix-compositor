// SPDX-License-Identifier: GPL-3.0-or-later
//! Starting a helper program from a keybind.
//!
//! One user today -- `wlrix-screenshot`, bound to Print -- and the machinery is here rather
//! than inline in [`crate::input`] because two parts of it are easy to get wrong and neither is
//! about screenshots.
//!
//! **By name off `PATH`**, not by an absolute path, matching how `wlrix-session` starts every
//! other wlRIX component: a development build earlier in `PATH` is then picked up without
//! reinstalling.
//!
//! **Children have to be reaped.** A compositor that spawns and forgets accumulates a zombie
//! per screenshot for the life of the session. The obvious fix -- `signal(SIGCHLD, SIG_IGN)`,
//! which makes the kernel reap them -- would be a bug here: `main` polls the `-c` child with
//! `try_wait` and stops the compositor when it exits, and that is what lets greetd hand over
//! promptly at login. Auto-reaping would make that `try_wait` never see an exit, so the
//! greeter's compositor would outlive the greeter and greetd would sit through its kill
//! timeout. So the children are kept and reaped explicitly, once per dispatch.

use std::process::{Command, Stdio};

use tracing::{info, warn};

use crate::Wlrix;

/// The screenshot tool. See [`crate::keybinds::ShotMode`] for what the arguments mean.
pub const SCREENSHOT: &str = "wlrix-screenshot";

impl Wlrix {
    /// Start a helper, and remember it so it can be reaped.
    pub fn spawn_helper(&mut self, program: &str, args: &[String]) {
        // stdout and stderr are inherited, so the helper's own logging joins the compositor's
        // in the session log -- the same arrangement `wlrix-session` gives the apps it starts.
        // stdin is closed: nothing here is interactive, and a helper that read from the
        // compositor's stdin would be reading the terminal it was launched from.
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                info!(program, pid = child.id(), "spawned");
                self.children.push(child);
            }
            Err(err) => warn!(program, "could not run it: {err} (is it installed?)"),
        }
    }

    /// Clear out any helper that has finished. Called once per dispatch.
    ///
    /// Cheap when nothing has exited: `try_wait` on a live child is one `waitid` that returns
    /// immediately, and the list is empty on a session where nobody has pressed Print.
    pub fn reap_helpers(&mut self) {
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(Some(status)) => {
                // A non-zero exit is worth a line but not a fuss: `wlrix-screenshot` answers 1
                // for "the user pressed Escape", which is not a failure of anything.
                if !status.success() {
                    info!(pid = child.id(), ?status, "a helper exited");
                }
                false
            }
            Ok(None) => true,
            Err(err) => {
                warn!(pid = child.id(), "could not check on a helper: {err}");
                false
            }
        });
    }

    /// The arguments that describe a screenshot, given what the binding asked for.
    ///
    /// `ActiveWindow` is the interesting one. The tool cannot work out which window is focused
    /// or where its frame is -- `ext-image-capture-source-v1`'s per-toplevel source would give
    /// it the client's surface tree *without* the 4Dwm frame the compositor draws around it --
    /// so this hands over the rectangle instead. Everything the tool needs is one `--select`.
    ///
    /// With nothing focused it falls back to the whole desktop rather than doing nothing: a key
    /// that silently does nothing reads as a broken keyboard.
    pub fn screenshot_args(&self, mode: crate::keybinds::ShotMode) -> Vec<String> {
        use crate::keybinds::ShotMode;
        match mode {
            ShotMode::Region => Vec::new(),
            ShotMode::Screen => vec!["--all".to_string()],
            ShotMode::ActiveWindow => match self.focused_frame_rect() {
                Some(rect) => vec![
                    "--select".to_string(),
                    format!(
                        "{},{},{},{}",
                        rect.loc.x, rect.loc.y, rect.size.w, rect.size.h
                    ),
                ],
                None => {
                    info!("no focused window to shoot; taking the whole desktop instead");
                    vec!["--all".to_string()]
                }
            },
        }
    }

    /// The focused window's rectangle on the desktop, decorations included.
    ///
    /// `None` when nothing is focused, or when the focused window is not in the space -- a
    /// minimized window has an icon rather than a rectangle, and shooting the icon is not what
    /// "screenshot the active window" means.
    fn focused_frame_rect(
        &self,
    ) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
        let window = self.focused_window()?;
        let client = self.space.element_geometry(&window)?;
        let style = crate::frame::frame_style(&window)?;
        Some(crate::decoration::frame_rect(client, style))
    }
}
