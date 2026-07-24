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

use std::{cell::RefCell, collections::HashMap};

use smithay::{
    desktop::{Space, Window},
    utils::{Logical, Point, Rectangle},
};

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
