// SPDX-License-Identifier: GPL-3.0-or-later
//! Starting a helper program from a keybind.
//!
//! Three users today -- `wlrix-screenshot`, bound to Print, `wlrix-clipboard-history`, bound to
//! Super+V, and `wlrix-lock`, bound to Super+L -- and the machinery is here rather than inline in
//! [`crate::input`] because two parts of it are easy to get wrong and neither is about any one
//! program.
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
//!
//! **Children are told the pointer theme.** The compositor announces `XCURSOR_THEME` and
//! `XCURSOR_SIZE` to `wlrix-session`, which is how the apps the session starts get them, but
//! its own environment never had them: a helper started from here inherited neither, and a
//! toolkit that loads its own pointer drew its built-in default over the IRIX one. See
//! [`Wlrix::helper_command`].

use std::process::{Command, Stdio};

use tracing::{info, warn};

use crate::Wlrix;

/// The screenshot tool. See [`crate::keybinds::ShotMode`] for what the arguments mean.
pub const SCREENSHOT: &str = "wlrix-screenshot";

/// The clipboard history popup, from `wlrix-apps`. Started with no arguments; a second start
/// while one is open closes it, so the same key opens and dismisses it.
pub const CLIPBOARD_HISTORY: &str = "wlrix-clipboard-history";

/// The session locker, from `wlrix-lock`. It stays running until the session is unlocked, so it
/// is reaped like any other helper once it has done its job.
pub const LOCK: &str = "wlrix-lock";

impl Wlrix {
    /// A command for a program the compositor starts, with the pointer theme in its
    /// environment.
    ///
    /// From the running config rather than from the handshake values sent at startup, so a
    /// helper started after a theme change reloaded from `compositor.toml` gets the new one.
    /// stdin is closed: nothing started from here is interactive, and a child reading the
    /// compositor's stdin would be reading the terminal it was launched from.
    pub fn helper_command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("XCURSOR_THEME", self.config.cursor.theme())
            .env("XCURSOR_SIZE", self.config.cursor.size().to_string())
            .stdin(Stdio::null());
        command
    }

    /// Start a helper, and remember it so it can be reaped.
    pub fn spawn_helper(&mut self, program: &str, args: &[String]) {
        // stdout and stderr are inherited, so the helper's own logging joins the compositor's
        // in the session log -- the same arrangement `wlrix-session` gives the apps it starts.
        match self.helper_command(program).args(args).spawn() {
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
