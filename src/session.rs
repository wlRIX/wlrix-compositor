// SPDX-License-Identifier: GPL-3.0-or-later
//! Saving the open windows at logout and putting them back at the next login: IRIX's
//! **Save Windows & Desks**.
//!
//! Two snapshots, both machine-written under `$XDG_STATE_HOME/wlrix/` beside `desks.toml`:
//!
//! - `session.toml`, written as the session ends, when `[session] save = "continuously"`.
//!   "Windows onscreen at logout will be reopened."
//! - `home-session.toml`, written only when the user presses **Set Home Session** (the
//!   compositor gets `SIGUSR2`), and read at every login under `save = "explicitly"`. It
//!   carries the desk row as well, so the home session is the desks *and* their windows.
//!
//! The compositor is the only process that knows where everything is, so it writes the
//! snapshot and, when the windows come back, puts each one where it was. It does not start
//! anything: it tells `wlrix-session` which file to read (the `WLRIX_RESTORE` handshake line)
//! and the session runs each `[[app]]`'s command.
//!
//! **Which command.** Worked out at save time, while the process is still there to ask: the
//! window's name is looked up among the installed `.desktop` files, and the process's own
//! command line is kept when it is the program the entry runs, so its files reopen with it.
//! See [`crate::desktop`]. A window with no launcher is not saved -- there would be no way to
//! bring it back.
//!
//! **Which window is which.** A relaunched application's windows are matched to the saved
//! ones as they first appear: by the X11 startup id the session hands each launch, else by
//! application and exact title, else by application in saved stacking order. A window gets
//! its saved monitor, position, size, desk and state *before* it is first shown -- a Wayland
//! window is sized in its initial configure -- so it opens where it was rather than opening
//! somewhere and jumping. Monitors are recognized by their EDID first and their connector
//! second, so a window follows its monitor to another port.
//!
//! **When.** At logout the quit signal saves before the loop stops; the clients are all
//! still connected then, because `wlrix-session` stops the apps only once the compositor has
//! gone. A shutdown is different: logind signals every process in the session at once, and
//! applications may be gone before the compositor gets to save. So `wlrix-idle`, which holds
//! a logind delay inhibitor, sends `SIGUSR1` on `PrepareForShutdown` and waits for the file;
//! that save *freezes* the snapshot for [`FREEZE`], so the windows dying a moment later do not
//! overwrite it with an emptier one.

use std::{
    cell::Cell,
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use smithay::{
    desktop::Window,
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};
use tracing::{info, warn};

use crate::{
    Wlrix,
    config::SaveMode,
    desks::{self, DeskId},
};

/// The continuous snapshot, relative to the state directory.
const SESSION_NAME: &str = "wlrix/session.toml";
/// The snapshot Set Home Session writes.
const HOME_NAME: &str = "wlrix/home-session.toml";

/// How long after startup a new window may still be matched to a saved one.
///
/// Generous, because a cold login starts every application at once and a browser restoring
/// its own tabs can take a while to show its first window. Past it, nothing more is expected:
/// a window the user opens by hand an hour later must not be dragged to where one of its
/// predecessors was.
const RESTORE_PERIOD: Duration = Duration::from_secs(120);

/// How long a save made for a shutdown stands before a later save may replace it.
///
/// Long enough to cover the rest of the shutdown, short enough that a shutdown which was
/// cancelled -- an inhibitor, or the user changing their mind -- does not leave the session
/// unable to save at the next logout.
const FREEZE: Duration = Duration::from_secs(30);

/// The prefix of the `DESKTOP_STARTUP_ID` `wlrix-session` gives each launch, followed by the
/// `[[app]]`'s index in the file.
pub const STARTUP_ID_PREFIX: &str = "wlrix-restore-";

/// Windows never saved, whatever the user's own list says: the Toolchest, which the session
/// always starts, and the ones that are transient by nature.
const SESSION_EXCLUDED: &[&str] = &[
    "com.wlrix.toolchest",
    "com.wlrix.greeter",
    "com.wlrix.shutdown",
    "com.wlrix.clipboard-history",
];

/// Left out of the home session only: Window Settings is open when Set Home Session is pressed
/// because that is where the button is, not because it belongs in the session. IRIX left it
/// out of both modes; a Continuously session keeps it here, since a panel left open at logout
/// is an open window like any other.
const HOME_EXCLUDED: &[&str] = &["com.wlrix.settings.windows"];

/// A whole snapshot.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFile {
    #[serde(default)]
    version: u32,
    /// The desk row. Only in the home session; the continuous one leaves desks to
    /// `desks.toml`, which is already kept up to date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desks: Option<desks::DesksFile>,
    /// One per process, in the stacking order of each one's first window.
    #[serde(default, rename = "app")]
    apps: Vec<SavedApp>,
}

/// One process to start, and the windows it had.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SavedApp {
    argv: Vec<String>,
    /// The `.desktop` file the command came from. For people reading the file and the logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desktop_id: Option<String>,
    #[serde(default, rename = "window")]
    windows: Vec<SavedWindow>,
}

/// Where one window was, and how.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWindow {
    /// The xdg `app_id`, or the X11 class.
    app_id: String,
    #[serde(default)]
    title: String,
    /// The client rectangle's origin, relative to its monitor's top-left corner.
    ///
    /// Always the *normal* geometry -- what a maximized or minimized window goes back to --
    /// so a restored window can be put there first and have its state applied on top.
    offset: [i32; 2],
    size: [i32; 2],
    #[serde(default, skip_serializing_if = "is_false")]
    maximized: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    fullscreen: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    minimized: bool,
    /// An index into the desk row, as `desks.toml`'s `active` is: desk ids are not saved.
    #[serde(default)]
    desk: usize,
    /// On the global desk, visible everywhere. `desk` is then meaningless.
    #[serde(default, skip_serializing_if = "is_false")]
    global: bool,
    /// Back to front across the whole session.
    stack: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon_slot: Option<usize>,
    output: SavedOutput,
}

/// Which monitor a window was on.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedOutput {
    /// The connector, `DP-1`.
    name: String,
    /// The EDID identity, which follows the monitor to another port.
    #[serde(default)]
    make: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    serial: String,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// The saved windows still waiting for their application to show them.
#[derive(Debug, Default)]
pub struct PendingRestore {
    deadline: Option<Instant>,
    /// With the index of the `[[app]]` each came from, sorted back to front.
    windows: Vec<(usize, SavedWindow)>,
}

impl PendingRestore {
    fn new(file: &SessionFile) -> Self {
        let mut windows: Vec<(usize, SavedWindow)> = file
            .apps
            .iter()
            .enumerate()
            .flat_map(|(app, saved)| saved.windows.iter().map(move |w| (app, w.clone())))
            .collect();
        windows.sort_by_key(|(_, window)| window.stack);
        Self {
            deadline: Some(Instant::now() + RESTORE_PERIOD),
            windows,
        }
    }

    /// The saved window a new one stands for, taken off the list so it is matched once.
    fn claim(
        &mut self,
        app_id: &str,
        title: &str,
        startup_id: Option<&str>,
    ) -> Option<SavedWindow> {
        if self.windows.is_empty() {
            return None;
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            info!(
                unmatched = self.windows.len(),
                "the restore period is over; the rest of the saved windows never appeared"
            );
            self.windows.clear();
            return None;
        }

        let key = app_id.to_lowercase();
        let same_app = |window: &SavedWindow| window.app_id.to_lowercase() == key;

        // The startup id names the launch exactly, but not which of its windows this is.
        let launch = startup_id
            .and_then(|id| id.strip_prefix(STARTUP_ID_PREFIX))
            .and_then(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<usize>().ok()
            });
        let by_launch = launch.and_then(|launch| {
            self.windows
                .iter()
                .position(|(app, window)| *app == launch && same_app(window))
                .or_else(|| self.windows.iter().position(|(app, _)| *app == launch))
        });

        let index = by_launch
            .or_else(|| {
                self.windows
                    .iter()
                    .position(|(_, window)| same_app(window) && window.title == title)
            })
            .or_else(|| self.windows.iter().position(|(_, window)| same_app(window)))?;
        Some(self.windows.remove(index).1)
    }
}

/// What a restored window carries from the moment it is matched, hung off its `user_data`.
///
/// Placement reads it ([`restoring`]), so the window's first position is its saved one.
pub struct Restoring {
    pub output: Output,
    /// Where the client rectangle goes, in the space's coordinates.
    pub position: Point<i32, Logical>,
    /// The size it is going back to, which may not be what it first draws at; see
    /// [`crate::placement::place_if_new`].
    pub size: Size<i32, Logical>,
    saved: SavedWindow,
    /// Whether it has drawn since, while on screen. Until it has, its own size says nothing: a
    /// window restored to a desk nobody has switched to yet gets no frame callbacks, so a
    /// client that drew its first frame at its own size has not redrawn at this one.
    settled: Cell<bool>,
}

/// Marks a window as already offered to [`PendingRestore::claim`].
struct RestoreChecked;

/// A visible, placed window has committed: whatever size it now has is its own. See
/// [`Restoring::settled`].
pub fn note_commit(window: &Window) {
    if let Some(restoring) = restoring(window)
        && crate::placement::is_placed(window)
    {
        restoring.settled.set(true);
    }
}

/// The restore a window is part of, if it is one.
pub fn restoring(window: &Window) -> Option<&Restoring> {
    window.user_data().get::<Restoring>()
}

/// The snapshot to bring back at this login, and where it is.
///
/// Only for a compositor `wlrix-session` started: the greeter runs this same binary, and a
/// nested one started by hand has no session to relaunch anything.
pub fn startup(mode: SaveMode) -> Option<(PathBuf, SessionFile)> {
    if !crate::handshake::enabled() {
        return None;
    }
    let path = state_file(match mode {
        SaveMode::Continuously => SESSION_NAME,
        SaveMode::Explicitly => HOME_NAME,
    })?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("could not read {}: {err}", path.display());
            return None;
        }
    };
    match toml::from_str::<SessionFile>(&text) {
        Ok(file) => {
            info!(
                path = %path.display(),
                apps = file.apps.len(),
                "restoring the saved session"
            );
            Some((path, file))
        }
        Err(err) => {
            warn!("{} is not valid: {err}; not restoring", path.display());
            None
        }
    }
}

impl SessionFile {
    /// Whether there is anything for the session to start.
    pub fn has_apps(&self) -> bool {
        !self.apps.is_empty()
    }

    /// The desk row a home session carries.
    pub fn take_desks(&mut self) -> Option<desks::DesksFile> {
        self.desks.take()
    }

    pub fn pending(&self) -> PendingRestore {
        PendingRestore::new(self)
    }

    fn window_count(&self) -> usize {
        self.apps.iter().map(|app| app.windows.len()).sum()
    }
}

impl Wlrix {
    /// `SIGTERM`: the session is ending. Save what is open, unless a shutdown already did.
    pub fn save_session_at_quit(&mut self) {
        if !crate::handshake::enabled() || self.config.session.save != SaveMode::Continuously {
            return;
        }
        if self
            .session_frozen_until
            .is_some_and(|until| Instant::now() < until)
        {
            info!("keeping the session saved for the shutdown");
            return;
        }
        self.save_session(false);
    }

    /// `SIGUSR1`: the machine is about to shut down. Save now, while every application is
    /// still here, and let nothing overwrite it for a while.
    pub fn save_session_for_shutdown(&mut self) {
        if !crate::handshake::enabled() || self.config.session.save != SaveMode::Continuously {
            return;
        }
        self.save_session(false);
        self.session_frozen_until = Some(Instant::now() + FREEZE);
    }

    /// `SIGUSR2`: Set Home Session.
    pub fn save_home_session(&mut self) {
        self.save_session(true);
    }

    fn save_session(&self, home: bool) {
        let Some(path) = state_file(if home { HOME_NAME } else { SESSION_NAME }) else {
            warn!("no state directory ($XDG_STATE_HOME / $HOME); not saving the session");
            return;
        };
        let file = self.session_snapshot(home);
        let text = match toml::to_string(&file) {
            Ok(text) => text,
            Err(err) => {
                warn!("could not serialize the session: {err}");
                return;
            }
        };
        match desks::write_replace(&path, text.as_bytes()) {
            Ok(()) => info!(
                path = %path.display(),
                apps = file.apps.len(),
                windows = file.window_count(),
                "saved the session"
            ),
            Err(err) => warn!("could not write {}: {err}", path.display()),
        }
    }

    /// Every window worth bringing back, grouped by process.
    fn session_snapshot(&self, home: bool) -> SessionFile {
        let entries = crate::desktop::installed();
        // Back to front: the held-aside windows first, then the visible stack. A desk that is
        // not showing was hidden in its own stacking order (see `desks::switch_to`), so the
        // order within each desk is kept, which is the only order anyone can see.
        let windows: Vec<Window> = self
            .desks
            .hidden()
            .iter()
            .chain(self.space.elements())
            .cloned()
            .collect();

        let mut apps: Vec<SavedApp> = Vec::new();
        let mut by_pid: HashMap<i32, usize> = HashMap::new();
        for (stack, window) in windows.iter().enumerate() {
            let Some(saved) = self.saved_window(window, stack, home) else {
                continue;
            };
            let pid = self.window_pid(window);
            // One process with several windows is started once. A browser restores its own
            // windows; each still finds its place here as it appears.
            if let Some(&index) = pid.as_ref().and_then(|pid| by_pid.get(pid)) {
                apps[index].windows.push(saved);
                continue;
            }

            let instance = window.x11_surface().map(|x11| x11.instance());
            let mut names = vec![saved.app_id.as_str()];
            names.extend(instance.as_deref());
            let Some(entry) = crate::desktop::lookup(&entries, &names) else {
                info!(app_id = %saved.app_id, "no launcher for this window; not saving it");
                continue;
            };
            let Some(argv) =
                crate::desktop::command_for(entry, pid.and_then(crate::desktop::cmdline))
            else {
                info!(desktop_id = %entry.id, "the launcher has no command; not saving it");
                continue;
            };
            if let Some(pid) = pid {
                by_pid.insert(pid, apps.len());
            }
            apps.push(SavedApp {
                argv,
                desktop_id: Some(entry.id.clone()),
                windows: vec![saved],
            });
        }

        SessionFile {
            version: 1,
            desks: home.then(|| self.desks.snapshot()),
            apps,
        }
    }

    /// One window as it should be saved, or `None` for one that is not saved at all.
    fn saved_window(&self, window: &Window, stack: usize, home: bool) -> Option<SavedWindow> {
        // Menus and tooltips, and dialogs: they belong to a window, and come back with it if
        // its application brings them back.
        if let Some(x11) = window.x11_surface() {
            if x11.is_override_redirect() || x11.is_transient_for().is_some() {
                return None;
            }
        } else if window.toplevel()?.parent().is_some() {
            return None;
        }
        if !crate::placement::is_placed(window) {
            return None;
        }

        let app_id = crate::placement::window_app_id(window);
        if app_id.is_empty()
            || SESSION_EXCLUDED.contains(&app_id.as_str())
            || (home && HOME_EXCLUDED.contains(&app_id.as_str()))
            || self.config.session.exclude.contains(&app_id)
        {
            return None;
        }

        let state = desks::window_state(window).borrow();
        let size = match restoring(window) {
            Some(restoring) if !restoring.settled.get() && window.toplevel().is_some() => {
                restoring.size
            }
            _ => window.geometry().size,
        };
        let current = self
            .space
            .element_location(window)
            .map(|loc| Rectangle::new(loc, size));
        let held = Rectangle::new(state.last_pos, size);
        // The geometry it would have were it in none of these states.
        let normal = if state.maximized {
            state.restore_geo.or(current)
        } else if state.fullscreen {
            state.pre_fullscreen.or(current)
        } else if state.minimized {
            state.restore_geo
        } else {
            current
        }
        .unwrap_or(held);
        if normal.size.w <= 0 || normal.size.h <= 0 {
            return None;
        }

        let center = normal.loc.to_f64()
            + Point::from((
                f64::from(normal.size.w) / 2.0,
                f64::from(normal.size.h) / 2.0,
            ));
        let output = self
            .space
            .output_under(center)
            .next()
            .or_else(|| self.space.outputs().next())?;
        let origin = self.space.output_geometry(output)?.loc;
        let properties = output.physical_properties();

        let (desk, global) = if state.desk.is_global() {
            (0, true)
        } else {
            let index = self.desks.order().iter().position(|&d| d == state.desk);
            (index.unwrap_or(0), false)
        };

        Some(SavedWindow {
            app_id,
            title: crate::desks_protocol::window_title(window),
            offset: [normal.loc.x - origin.x, normal.loc.y - origin.y],
            size: [normal.size.w, normal.size.h],
            maximized: state.maximized,
            fullscreen: state.fullscreen,
            minimized: state.minimized,
            desk,
            global,
            stack,
            icon_slot: state.minimized.then_some(state.icon_slot).flatten(),
            output: SavedOutput {
                name: output.name(),
                make: properties.make,
                model: properties.model,
                serial: properties.serial_number,
            },
        })
    }

    /// The process behind a window.
    ///
    /// An X11 window says so itself in `_NET_WM_PID`. Its socket would only say XWayland, the
    /// one process every X11 window shares.
    fn window_pid(&self, window: &Window) -> Option<i32> {
        if let Some(x11) = window.x11_surface() {
            return x11.pid().and_then(|pid| i32::try_from(pid).ok());
        }
        let client = window.toplevel()?.wl_surface().client()?;
        Some(client.get_credentials(&self.display_handle).ok()?.pid)
    }

    /// The monitor a saved window was on, or the nearest thing to it.
    ///
    /// By EDID first, because a monitor moved to another port should take its windows with
    /// it; a monitor that reports no serial cannot be told from its twin that way, so only a
    /// real one counts. Then by connector. Then the pointer's monitor, for a window whose
    /// monitor is not plugged in -- placement clamps it onto that one.
    fn output_for_saved(&self, saved: &SavedOutput) -> Option<Output> {
        let known = |value: &str| !value.is_empty() && value != "Unknown";
        self.space
            .outputs()
            .find(|output| {
                let properties = output.physical_properties();
                known(&saved.serial)
                    && properties.serial_number == saved.serial
                    && properties.make == saved.make
                    && properties.model == saved.model
            })
            .or_else(|| {
                self.space
                    .outputs()
                    .find(|output| output.name() == saved.name)
            })
            .cloned()
            .or_else(|| {
                let pointer = self.pointer_location();
                self.space
                    .output_under(pointer)
                    .next()
                    .or_else(|| self.space.outputs().next())
                    .cloned()
            })
    }

    /// Mark `window` as the restore of `saved`, returning the client rectangle it gets.
    fn begin_restore(
        &mut self,
        window: &Window,
        saved: SavedWindow,
    ) -> Option<Rectangle<i32, Logical>> {
        let output = self.output_for_saved(&saved.output)?;
        let origin = self.space.output_geometry(&output)?.loc;
        let position = origin + Point::from((saved.offset[0], saved.offset[1]));
        let size = Size::from((saved.size[0], saved.size[1]));
        info!(
            app_id = %saved.app_id,
            output = %output.name(),
            ?position,
            ?size,
            "restoring a saved window"
        );
        window.user_data().insert_if_missing(|| Restoring {
            output,
            position,
            size,
            saved,
            settled: Cell::new(false),
        });
        Some(Rectangle::new(position, size))
    }

    /// Match a Wayland toplevel to a saved window, at its first commit.
    ///
    /// Before the initial configure is sent, so the configure carries the saved size -- or
    /// maximized, or fullscreen -- and the window's first frame is already the right one.
    pub fn claim_restore_for_toplevel(&mut self, surface: &WlSurface) {
        if self.pending_restore.windows.is_empty() {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
        else {
            return;
        };
        if window.user_data().get::<RestoreChecked>().is_some() {
            return;
        }
        let Some((app_id, title)) = with_states(surface, |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()?
                .lock()
                .ok()?;
            (!data.initial_configure_sent).then(|| {
                (
                    data.app_id.clone().unwrap_or_default(),
                    data.title.clone().unwrap_or_default(),
                )
            })
        }) else {
            return;
        };
        window.user_data().insert_if_missing(|| RestoreChecked);
        // Dialogs are never saved, so one must not take a saved window's place.
        if window.toplevel().is_some_and(|t| t.parent().is_some()) {
            return;
        }
        let Some(saved) = self.pending_restore.claim(&app_id, &title, None) else {
            return;
        };
        let (maximized, fullscreen) = (saved.maximized, saved.fullscreen);
        let Some(normal) = self.begin_restore(&window, saved) else {
            return;
        };

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(normal.size));
        }
        if maximized {
            self.maximize_window(&window);
        }
        if fullscreen {
            let output = restoring(&window).map(|r| r.output.clone());
            self.fullscreen_window(&window, output);
        }
        // Not yet placed, so neither of those had a geometry to come back to. This is it.
        let mut state = desks::window_state(&window).borrow_mut();
        if maximized {
            state.restore_geo = Some(normal);
        } else if fullscreen {
            state.pre_fullscreen = Some(normal);
        }
    }

    /// Match an X11 window to a saved one as it asks to be mapped, returning the size to
    /// place it at.
    pub fn claim_restore_for_x11(&mut self, window: &Window) -> Option<Size<i32, Logical>> {
        let x11 = window.x11_surface()?;
        if x11.is_override_redirect() || x11.is_transient_for().is_some() {
            return None;
        }
        let startup_id = x11.startup_id();
        let saved =
            self.pending_restore
                .claim(&x11.class(), &x11.title(), startup_id.as_deref())?;
        self.begin_restore(window, saved).map(|normal| normal.size)
    }

    /// The rest of a restore, once the window is placed: its desk, whether it is minimized,
    /// and where it goes in the stack.
    ///
    /// Not focused, and no new-window sound: a dozen windows reappearing at login is the
    /// session coming back, not a dozen things asking for attention.
    pub fn finish_restore(&mut self, window: &Window) {
        let Some((saved, size)) = restoring(window).map(|r| (r.saved.clone(), r.size)) else {
            return;
        };
        // Some clients draw their first frame at their own size whatever the initial configure
        // said -- Alacritty does -- and only take the size from a later one. Nothing else may
        // send them one for a while, so ask again.
        if let Some(toplevel) = window.toplevel()
            && !saved.maximized
            && !saved.fullscreen
            && window.geometry().size != size
        {
            toplevel.with_pending_state(|state| state.size = Some(size));
            toplevel.send_configure();
        }
        // An X11 window's states are applied here, after placement, where a Wayland one's were
        // applied before its first configure. Maximized first, as `map_window_request` does, so
        // a window that is both leaves fullscreen maximized.
        if saved.maximized && !desks::window_state(window).borrow().maximized {
            self.maximize_window(window);
        }
        if saved.fullscreen && !desks::window_state(window).borrow().fullscreen {
            let output = restoring(window).map(|r| r.output.clone());
            self.fullscreen_window(window, output);
        }
        if saved.minimized {
            desks::window_state(window).borrow_mut().icon_slot = saved.icon_slot;
            self.minimize_window(window);
        }

        let target = if saved.global {
            DeskId::GLOBAL
        } else {
            self.desks
                .order()
                .get(saved.desk)
                .copied()
                .unwrap_or_else(|| self.desks.active())
        };
        // `move_window_to_desk` without its refocus, which would hand the keyboard to whatever
        // is on top every time another window came back.
        desks::window_state(window).borrow_mut().desk = target;
        let visible = target == self.desks.active() || target.is_global();
        if !visible && let Some(loc) = self.space.element_location(window) {
            desks::window_state(window).borrow_mut().last_pos = loc;
            self.space.unmap_elem(window);
            self.desks.hide(window.clone());
        }
        self.restack_restored();
        if self.focused_window().is_none() {
            crate::focus::focus_topmost(self);
        }
        self.desks_changed();
        self.request_redraw();
    }

    /// Put the restored windows back in their saved order, however they arrived.
    ///
    /// Each one is mapped on top as it appears, so the ones saved above it are raised again,
    /// bottom first. The windows held aside on other desks are sorted the same way, which is
    /// the order their desk shows them in when it is switched to.
    fn restack_restored(&mut self) {
        let mut mapped: Vec<(usize, Window)> = self
            .space
            .elements()
            .filter_map(|w| restoring(w).map(|r| (r.saved.stack, w.clone())))
            .collect();
        mapped.sort_by_key(|(stack, _)| *stack);
        for (_, window) in mapped {
            self.space.raise_element(&window, false);
        }
        self.desks
            .sort_hidden_by_key(|w| restoring(w).map_or(usize::MAX, |r| r.saved.stack));
    }
}

fn state_file(name: &str) -> Option<PathBuf> {
    desks::state_dir().map(|dir| dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(app_id: &str, title: &str, stack: usize) -> SavedWindow {
        SavedWindow {
            app_id: app_id.to_string(),
            title: title.to_string(),
            offset: [10, 20],
            size: [640, 480],
            maximized: false,
            fullscreen: false,
            minimized: false,
            desk: 0,
            global: false,
            stack,
            icon_slot: None,
            output: SavedOutput {
                name: "DP-1".to_string(),
                make: "Dell".to_string(),
                model: "U2720Q".to_string(),
                serial: "ABC".to_string(),
            },
        }
    }

    fn file(apps: Vec<(Vec<&str>, Vec<SavedWindow>)>) -> SessionFile {
        SessionFile {
            version: 1,
            desks: None,
            apps: apps
                .into_iter()
                .map(|(argv, windows)| SavedApp {
                    argv: argv.into_iter().map(str::to_string).collect(),
                    desktop_id: None,
                    windows,
                })
                .collect(),
        }
    }

    #[test]
    fn a_snapshot_survives_the_round_trip() {
        let mut minimized = window("com.wlrix.writer", "notes.txt", 3);
        minimized.minimized = true;
        minimized.icon_slot = Some(4);
        let mut clock = window("com.wlrix.clock", "Clock", 1);
        clock.global = true;
        let mut saved = file(vec![
            (vec!["wlrix-writer", "/home/v/notes.txt"], vec![minimized]),
            (vec!["wlrix-clock"], vec![clock]),
        ]);
        saved.desks = Some(desks::Desks::new().snapshot());

        let text = toml::to_string(&saved).expect("serialize");
        let back: SessionFile = toml::from_str(&text).expect("parse");
        assert_eq!(back.apps.len(), 2);
        assert_eq!(back.apps[0].argv, ["wlrix-writer", "/home/v/notes.txt"]);
        assert_eq!(back.apps[0].windows, saved.apps[0].windows);
        assert_eq!(back.apps[1].windows, saved.apps[1].windows);
        assert!(back.desks.is_some());
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        assert!(toml::from_str::<SessionFile>("[[app]]\nargv = [\"x\"]\nargs = 1\n").is_err());
    }

    #[test]
    fn exact_title_beats_stacking_order() {
        let mut pending = file(vec![(
            vec!["firefox"],
            vec![window("firefox", "Mail", 2), window("firefox", "News", 5)],
        )])
        .pending();
        assert_eq!(pending.claim("firefox", "News", None).unwrap().stack, 5);
        assert_eq!(pending.claim("firefox", "Whatever", None).unwrap().stack, 2);
        assert!(pending.claim("firefox", "News", None).is_none());
    }

    #[test]
    fn without_a_title_match_the_lowest_window_comes_first() {
        let mut pending = file(vec![
            (vec!["foot"], vec![window("foot", "~", 7)]),
            (vec!["foot"], vec![window("foot", "~", 1)]),
        ])
        .pending();
        assert_eq!(pending.claim("Foot", "zsh", None).unwrap().stack, 1);
        assert_eq!(pending.claim("foot", "zsh", None).unwrap().stack, 7);
    }

    #[test]
    fn the_startup_id_names_the_launch() {
        let mut pending = file(vec![
            (vec!["xterm"], vec![window("XTerm", "a", 1)]),
            (vec!["xterm"], vec![window("XTerm", "b", 2)]),
        ])
        .pending();
        let claimed = pending.claim("XTerm", "a", Some("wlrix-restore-1_TIME123"));
        assert_eq!(claimed.unwrap().title, "b");
    }

    #[test]
    fn nothing_is_claimed_after_the_restore_period() {
        let mut pending = file(vec![(vec!["foot"], vec![window("foot", "~", 1)])]).pending();
        pending.deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(pending.claim("foot", "~", None).is_none());
        assert!(pending.windows.is_empty());
    }

    #[test]
    fn another_application_is_never_matched() {
        let mut pending = file(vec![(vec!["foot"], vec![window("foot", "~", 1)])]).pending();
        assert!(pending.claim("org.gnome.Nautilus", "~", None).is_none());
        assert_eq!(pending.windows.len(), 1);
    }
}
