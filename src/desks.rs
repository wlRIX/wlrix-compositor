// SPDX-License-Identifier: GPL-3.0-or-later
//! Desks: IRIX's virtual desktops.
//!
//! A desk is a named group of windows. Exactly one ordinary desk is *active* at a time and
//! its windows are the ones in the [`Space`]; the rest are unmapped and held aside in
//! [`Desks::hidden`] until their desk is activated. A reserved **global** desk
//! ([`DeskId::GLOBAL`]) is different: its windows stay mapped on every desk, so the
//! toolchest, the desks chooser and the background are present everywhere.
//!
//! Membership is stored per window, in `window.user_data()` (the [`WindowState`] beside the
//! [`crate::placement::Placed`] marker), so it survives a window being unmapped while its
//! desk is inactive. The `Space` therefore holds exactly *global windows + the active desk's
//! non-minimized windows*, which is why every existing consumer of `space.elements()`
//! (render, focus, hit-testing) keeps working unchanged — it only ever sees what is visible.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    desktop::{Space, Window},
    utils::{Logical, Point, Rectangle},
};
use tracing::warn;

/// Where the machine-written desk state lives, relative to the state directory.
const STATE_NAME: &str = "wlrix/desks.toml";

/// A desk identifier. Monotonic; [`DeskId::GLOBAL`] is reserved for the global desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeskId(pub u32);

impl DeskId {
    /// The global desk, whose windows appear on every desk.
    pub const GLOBAL: DeskId = DeskId(0);

    pub fn is_global(self) -> bool {
        self == DeskId::GLOBAL
    }
}

/// Per-window desk membership and window-operation state, hung off `window.user_data()`.
///
/// Kept here rather than in a per-desk list so it survives the window being unmapped (an
/// inactive desk, or minimized). `desk` is the single source of truth for which desk a
/// window is on.
pub struct WindowState {
    /// The desk this window belongs to.
    pub desk: DeskId,
    /// Where to re-map the window when its desk is activated again.
    pub last_pos: Point<i32, Logical>,
    /// Geometry to return to on unmaximize/restore (the pre-op geometry).
    pub restore_geo: Option<Rectangle<i32, Logical>>,
    pub minimized: bool,
    pub maximized: bool,
    /// The window's cell in the minimized-icon grid. Remembered across restores so a window
    /// re-minimized returns to the same spot when it is still free (see [`crate::minimized`]).
    pub icon_slot: Option<usize>,
    /// Set when the window is minimized, cleared once the backend has captured its thumbnail:
    /// the capture needs the renderer, which only the backend has (see [`crate::thumbnail`]).
    pub needs_thumbnail: bool,
    /// The captured snapshot shown in the window's minimized icon, if one has been taken.
    pub thumbnail: Option<MemoryRenderBuffer>,
}

impl Default for WindowState {
    fn default() -> Self {
        // An as-yet-unassigned window defaults to the global desk: visible everywhere is
        // the safe default until placement assigns it (see `assign_new_window`).
        Self {
            desk: DeskId::GLOBAL,
            last_pos: (0, 0).into(),
            restore_geo: None,
            minimized: false,
            maximized: false,
            icon_slot: None,
            needs_thumbnail: false,
            thumbnail: None,
        }
    }
}

/// The [`WindowState`] for a window, created on first access.
pub fn window_state(window: &Window) -> &RefCell<WindowState> {
    window
        .user_data()
        .insert_if_missing(|| RefCell::new(WindowState::default()));
    window
        .user_data()
        .get::<RefCell<WindowState>>()
        .expect("just inserted")
}

/// The desk a window belongs to.
pub fn desk_of(window: &Window) -> DeskId {
    window_state(window).borrow().desk
}

/// All the desks and which one is active.
pub struct Desks {
    /// Ordinary desks in user-visible order (the global desk is not listed here).
    order: Vec<DeskId>,
    /// Every desk's name, including the global desk.
    names: HashMap<DeskId, String>,
    /// The active ordinary desk. Always an element of `order`.
    active: DeskId,
    /// Next id to hand out.
    next_id: u32,
    /// Next number for a default "Desk N" name.
    next_number: u32,
    /// Windows that exist but are not in the `Space`: on an inactive desk, or minimized.
    hidden: Vec<Window>,
}

impl Default for Desks {
    fn default() -> Self {
        Self::new()
    }
}

/// What survives a logout: the desks the user arranged, and which one they were on.
///
/// Names and order only. Windows are not saved -- they belong to processes that are gone by
/// the time this is read -- so a restored desk comes back empty, with its name and its place
/// in the row.
///
/// Ids are not saved either. They are handed out fresh on load, because nothing outside one
/// run of the compositor refers to them: clients learn them from the protocol each time they
/// bind. Saving them would be saving an implementation detail and inviting it to be wrong.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DesksFile {
    /// Ordinary desks, in the order they are shown.
    #[serde(default, rename = "desk")]
    desks: Vec<SavedDesk>,
    /// Which of them was active, as an index into `desks`. An index rather than a name
    /// because names are the user's and need not be unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<usize>,
    /// The global desk's name, which is renameable like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    global: Option<String>,
}

/// One saved desk. A table rather than a bare string so a desk can grow a second property
/// without the file changing shape.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedDesk {
    name: String,
}

impl Desks {
    /// One ordinary desk ("Desk 1", active) plus the global desk.
    pub fn new() -> Self {
        let first = DeskId(1);
        let mut names = HashMap::new();
        names.insert(DeskId::GLOBAL, "Global".to_string());
        names.insert(first, "Desk 1".to_string());
        Self {
            order: vec![first],
            names,
            active: first,
            next_id: 2,
            next_number: 2,
            hidden: Vec::new(),
        }
    }

    /// The desks from the last session, or [`Desks::new`] if there are none to restore.
    ///
    /// A file naming no desks is treated as no file at all: there is always at least one
    /// ordinary desk, and coming up with none would leave nowhere to put a window.
    pub fn restore() -> Self {
        match load_state() {
            Some(file) if !file.desks.is_empty() => Self::from_saved(file),
            _ => Self::new(),
        }
    }

    fn from_saved(file: DesksFile) -> Self {
        let mut names = HashMap::new();
        names.insert(
            DeskId::GLOBAL,
            file.global.unwrap_or_else(|| "Global".to_string()),
        );

        // Fresh ids, in saved order, starting past the global desk's 0.
        let order: Vec<DeskId> = (1..=file.desks.len() as u32).map(DeskId).collect();
        for (id, desk) in order.iter().zip(file.desks) {
            names.insert(*id, desk.name);
        }

        // Clamped rather than trusted: the file is machine-written, but a hand-edited or
        // truncated one must not index past the end.
        let active = file
            .active
            .and_then(|index| order.get(index).copied())
            .unwrap_or(order[0]);

        // `next_number` continues past the highest "Desk N" already in use, so a desk created
        // after a restore does not collide with one restored. Renamed desks contribute
        // nothing to it, which is right -- their names are no longer of that form.
        let next_number = names
            .values()
            .filter_map(|name| name.strip_prefix("Desk ")?.trim().parse::<u32>().ok())
            .max()
            .map_or(1, |highest| highest + 1);

        Self {
            next_id: order.len() as u32 + 1,
            order,
            names,
            active,
            next_number,
            hidden: Vec::new(),
        }
    }

    /// The desks as they should come back next time, for [`save`].
    fn snapshot(&self) -> DesksFile {
        DesksFile {
            desks: self
                .order
                .iter()
                .map(|id| SavedDesk {
                    name: self.name(*id).unwrap_or_default().to_string(),
                })
                .collect(),
            active: self.order.iter().position(|&id| id == self.active),
            global: self.name(DeskId::GLOBAL).map(str::to_string),
        }
    }

    pub fn active(&self) -> DeskId {
        self.active
    }

    /// Ordinary desks, in order (global excluded).
    pub fn order(&self) -> &[DeskId] {
        &self.order
    }

    pub fn name(&self, id: DeskId) -> Option<&str> {
        self.names.get(&id).map(String::as_str)
    }

    pub fn exists(&self, id: DeskId) -> bool {
        self.names.contains_key(&id)
    }

    /// Windows currently held out of the `Space`.
    pub fn hidden(&self) -> &[Window] {
        &self.hidden
    }

    /// Hold a window out of the `Space` (an inactive desk, or minimized).
    pub fn hide(&mut self, window: Window) {
        if !self.hidden.iter().any(|w| w == &window) {
            self.hidden.push(window);
        }
    }

    /// Stop holding a window aside (it is being mapped back into the `Space`).
    pub fn unhide(&mut self, window: &Window) {
        self.hidden.retain(|w| w != window);
    }

    /// Whether `id` may be deleted: not the global desk, and not the last ordinary desk.
    pub fn deletable(&self, id: DeskId) -> bool {
        id != DeskId::GLOBAL && self.order.contains(&id) && self.order.len() > 1
    }

    /// Create a new ordinary desk (not activated), returning its id.
    pub fn create(&mut self) -> DeskId {
        let id = DeskId(self.next_id);
        self.next_id += 1;
        self.names.insert(id, format!("Desk {}", self.next_number));
        self.next_number += 1;
        self.order.push(id);
        id
    }

    /// Rename a desk. Returns whether the desk existed.
    pub fn rename(&mut self, id: DeskId, name: String) -> bool {
        match self.names.get_mut(&id) {
            Some(slot) => {
                *slot = name;
                true
            }
            None => false,
        }
    }

    /// Remove a desk from the model (order/name/active), no window migration. Returns the
    /// neighboring desk (the migration target) or `None` if the delete is refused. The
    /// space-aware [`delete_desk`] uses this after moving windows.
    pub fn remove_desk(&mut self, id: DeskId) -> Option<DeskId> {
        if !self.deletable(id) {
            return None;
        }
        let pos = self.order.iter().position(|&d| d == id)?;
        let neighbor = if pos > 0 {
            self.order[pos - 1]
        } else {
            self.order[pos + 1]
        };
        self.order.retain(|&d| d != id);
        self.names.remove(&id);
        if self.active == id {
            self.active = neighbor;
        }
        Some(neighbor)
    }
}

/// Assign a freshly-mapped window to a desk: the global desk for the wlRIX shell apps that
/// belong on every desk, otherwise the active desk.
pub fn assign_new_window(desks: &Desks, window: &Window, app_id: Option<&str>) {
    let target = match app_id {
        Some("com.wlrix.toolchest") | Some("com.wlrix.desks") => DeskId::GLOBAL,
        _ => desks.active,
    };
    window_state(window).borrow_mut().desk = target;
}

/// Drop a window from the desk model on destroy/unmap-for-good. Its [`WindowState`] goes
/// away with the window itself.
pub fn forget_window(desks: &mut Desks, window: &Window) {
    desks.hidden.retain(|w| w != window);
}

/// Switch the active desk: hide the leaving desk's windows, show the target's.
///
/// Global-desk windows are never touched, so they persist across the switch. Minimized
/// windows stay hidden.
pub fn switch_to(space: &mut Space<Window>, desks: &mut Desks, target: DeskId) {
    if target == desks.active || target == DeskId::GLOBAL || !desks.exists(target) {
        return;
    }

    let leaving = desks.active;

    // Unmap the leaving desk's mapped windows (global excluded — they have desk == GLOBAL),
    // recording where each sat so it can be put back. Collect first: the filter borrows the
    // space immutably, the unmap needs it mutably.
    let to_hide: Vec<Window> = space
        .elements()
        .filter(|w| desk_of(w) == leaving)
        .cloned()
        .collect();
    for window in to_hide {
        if let Some(loc) = space.element_location(&window) {
            window_state(&window).borrow_mut().last_pos = loc;
        }
        space.unmap_elem(&window);
        desks.hidden.push(window);
    }

    desks.active = target;

    // Show the target desk's hidden, non-minimized windows.
    let to_show: Vec<Window> = desks
        .hidden
        .iter()
        .filter(|w| {
            let state = window_state(w).borrow();
            state.desk == target && !state.minimized
        })
        .cloned()
        .collect();
    for window in &to_show {
        let pos = window_state(window).borrow().last_pos;
        space.map_element(window.clone(), pos, false);
    }
    desks.hidden.retain(|w| !to_show.contains(w));
}

/// Delete a desk, migrating its windows to a neighbor. Refuses the global desk and the
/// last ordinary desk. Returns whether the desk was deleted.
pub fn delete_desk(space: &mut Space<Window>, desks: &mut Desks, id: DeskId) -> bool {
    if !desks.deletable(id) {
        return false;
    }
    let pos = desks
        .order
        .iter()
        .position(|&d| d == id)
        .expect("deletable");
    let neighbor = if pos > 0 {
        desks.order[pos - 1]
    } else {
        desks.order[pos + 1]
    };

    // Moving off the active desk first unmaps its windows cleanly into `hidden`.
    if desks.active == id {
        switch_to(space, desks, neighbor);
    }

    // The doomed desk is now inactive: all its windows are in `hidden`. Reassign them.
    let migrated: Vec<Window> = desks
        .hidden
        .iter()
        .filter(|w| desk_of(w) == id)
        .cloned()
        .collect();
    for window in &migrated {
        window_state(window).borrow_mut().desk = neighbor;
    }

    desks.remove_desk(id);

    // If the neighbor is the active desk, the migrated windows should now be visible.
    if desks.active == neighbor {
        let to_show: Vec<Window> = migrated
            .iter()
            .filter(|w| !window_state(w).borrow().minimized)
            .cloned()
            .collect();
        for window in &to_show {
            let position = window_state(window).borrow().last_pos;
            space.map_element(window.clone(), position, false);
        }
        desks.hidden.retain(|w| !to_show.contains(w));
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_one_desk_plus_global() {
        let desks = Desks::new();
        assert_eq!(desks.order(), &[DeskId(1)]);
        assert_eq!(desks.active(), DeskId(1));
        assert_eq!(desks.name(DeskId(1)), Some("Desk 1"));
        assert_eq!(desks.name(DeskId::GLOBAL), Some("Global"));
        assert!(desks.exists(DeskId::GLOBAL));
    }

    #[test]
    fn create_appends_a_named_desk_without_switching() {
        let mut desks = Desks::new();
        let id = desks.create();
        assert_eq!(desks.name(id), Some("Desk 2"));
        assert_eq!(desks.order(), &[DeskId(1), id]);
        assert_eq!(
            desks.active(),
            DeskId(1),
            "create must not change the active desk"
        );
    }

    #[test]
    fn the_last_ordinary_desk_cannot_be_deleted() {
        let mut desks = Desks::new();
        assert!(!desks.deletable(DeskId(1)));
        assert_eq!(desks.remove_desk(DeskId(1)), None);
        assert_eq!(desks.order(), &[DeskId(1)]);
    }

    #[test]
    fn the_global_desk_cannot_be_deleted() {
        let mut desks = Desks::new();
        desks.create();
        assert!(!desks.deletable(DeskId::GLOBAL));
        assert_eq!(desks.remove_desk(DeskId::GLOBAL), None);
        assert!(desks.exists(DeskId::GLOBAL));
    }

    #[test]
    fn deleting_the_active_desk_falls_back_to_a_neighbour() {
        let mut desks = Desks::new(); // Desk 1 active
        let second = desks.create();
        // Delete the active first desk; active must move to the surviving neighbor.
        let neighbor = desks.remove_desk(DeskId(1));
        assert_eq!(neighbor, Some(second));
        assert_eq!(desks.active(), second);
        assert_eq!(desks.order(), &[second]);
    }

    #[test]
    fn deleting_an_inactive_desk_leaves_active_alone() {
        let mut desks = Desks::new(); // Desk 1 active
        let second = desks.create();
        desks.remove_desk(second);
        assert_eq!(desks.active(), DeskId(1));
        assert_eq!(desks.order(), &[DeskId(1)]);
    }

    #[test]
    fn rename_changes_the_name() {
        let mut desks = Desks::new();
        assert!(desks.rename(DeskId(1), "Work".into()));
        assert_eq!(desks.name(DeskId(1)), Some("Work"));
        assert!(!desks.rename(DeskId(999), "Nope".into()));
    }

    #[test]
    fn ids_are_not_reused_after_delete() {
        let mut desks = Desks::new();
        let second = desks.create();
        desks.remove_desk(second);
        let third = desks.create();
        assert_ne!(third, second, "a fresh desk must get a new id");
    }
}

/// Write the desks out, so the next session comes up with the same row.
///
/// Failure is logged and never fatal, as with the display state: a compositor that refuses to
/// carry on because it could not save a preference is worse than one that forgets it.
pub fn save(desks: &Desks) {
    let Some(path) = state_file() else {
        warn!("no state directory ($XDG_STATE_HOME / $HOME); not saving desks");
        return;
    };
    let text = match toml::to_string(&desks.snapshot()) {
        Ok(text) => text,
        Err(err) => {
            warn!("could not serialize desks: {err}");
            return;
        }
    };
    if let Err(err) = write_replace(&path, text.as_bytes()) {
        warn!("could not write {}: {err}", path.display());
    }
}

/// Read `desks.toml`. A missing or broken file yields nothing, and the defaults apply.
fn load_state() -> Option<DesksFile> {
    let path = state_file()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Not-found is the ordinary first-run case; only real errors are worth a line.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("could not read {}: {err}", path.display());
            return None;
        }
    };
    match toml::from_str::<DesksFile>(&text) {
        Ok(file) => Some(file),
        Err(err) => {
            warn!(
                "{} is not valid: {err}; starting with one desk",
                path.display()
            );
            None
        }
    }
}

/// Replace a file atomically: write a sibling temp file, then rename over the target,
/// creating the parent directory if need be.
fn write_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn state_file() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(STATE_NAME))
}

/// `$XDG_STATE_HOME`, or `~/.local/state` as the spec says to assume.
fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
}

#[cfg(test)]
mod state_tests {
    use super::*;

    /// Round-trip through the file's own shape, without touching the disk.
    fn round_trip(desks: &Desks) -> Desks {
        let text = toml::to_string(&desks.snapshot()).expect("serialize");
        let file: DesksFile = toml::from_str(&text).expect("parse");
        Desks::from_saved(file)
    }

    fn names(desks: &Desks) -> Vec<&str> {
        desks
            .order()
            .iter()
            .map(|id| desks.name(*id).unwrap_or_default())
            .collect()
    }

    #[test]
    fn names_and_order_come_back() {
        let mut desks = Desks::new();
        let second = desks.create();
        let third = desks.create();
        desks.rename(second, "Mail".to_string());
        desks.rename(third, "Build".to_string());

        let restored = round_trip(&desks);
        assert_eq!(names(&restored), ["Desk 1", "Mail", "Build"]);
    }

    /// The active desk travels as a position, not an id -- ids are handed out fresh on load,
    /// so what has to survive is *which desk in the row* the user was on.
    #[test]
    fn the_active_desk_comes_back() {
        let file: DesksFile = toml::from_str(
            "active = 2\n\n[[desk]]\nname = \"One\"\n\n[[desk]]\nname = \"Two\"\n\n[[desk]]\nname = \"Three\"\n",
        )
        .expect("parse");
        let desks = Desks::from_saved(file);
        assert_eq!(desks.name(desks.active()), Some("Three"));
        // And writing it back out records the same position.
        assert_eq!(desks.snapshot().active, Some(2));
    }

    #[test]
    fn a_renamed_global_desk_comes_back_too() {
        let mut desks = Desks::new();
        desks.rename(DeskId::GLOBAL, "Everywhere".to_string());
        assert_eq!(round_trip(&desks).name(DeskId::GLOBAL), Some("Everywhere"));
    }

    /// The trap in handing out fresh ids: a desk created after a restore must not be given a
    /// name a restored desk already has.
    #[test]
    fn a_desk_created_after_a_restore_does_not_reuse_a_name() {
        let mut desks = Desks::new();
        desks.create();
        desks.create();
        assert_eq!(names(&desks), ["Desk 1", "Desk 2", "Desk 3"]);

        let mut restored = round_trip(&desks);
        let fresh = restored.create();
        assert_eq!(restored.name(fresh), Some("Desk 4"));
        // And its id is its own, not one already in the row.
        assert!(restored.order().iter().filter(|&&id| id == fresh).count() == 1);
    }

    /// Renaming every desk away from "Desk N" leaves the counter with nothing to continue
    /// from; a new desk should start over rather than pick something arbitrary.
    #[test]
    fn renamed_desks_do_not_hold_the_counter_up() {
        let mut desks = Desks::new();
        desks.rename(DeskId(1), "Mail".to_string());
        let mut restored = round_trip(&desks);
        let fresh = restored.create();
        assert_eq!(restored.name(fresh), Some("Desk 1"));
    }

    #[test]
    fn an_empty_or_missing_file_gives_the_default_single_desk() {
        // What a first run sees, and what a file that somehow lists no desks must also give:
        // there is always at least one desk, or there is nowhere to put a window.
        let file: DesksFile = toml::from_str("").expect("parse");
        assert!(file.desks.is_empty());
        let fresh = Desks::new();
        assert_eq!(names(&fresh), ["Desk 1"]);
    }

    #[test]
    fn an_out_of_range_active_index_falls_back_to_the_first_desk() {
        // The file is machine-written, but a truncated or hand-edited one must not index
        // past the end of the row.
        let file: DesksFile =
            toml::from_str("active = 7\n\n[[desk]]\nname = \"Only\"\n").expect("parse");
        let desks = Desks::from_saved(file);
        assert_eq!(desks.active(), desks.order()[0]);
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // Same rule as the rest of wlRIX's files: a typo is reported, not ignored. Here it
        // means the state file is dropped and the defaults apply.
        assert!(toml::from_str::<DesksFile>("desks = 3").is_err());
        assert!(toml::from_str::<DesksFile>("[[desk]]\nnmae = \"x\"\n").is_err());
    }
}
